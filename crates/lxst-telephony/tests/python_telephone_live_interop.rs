//! Opt-in live interoperability against Python LXST over real TCP interfaces.
//!
//! Tests are ignored by default, except the link request stamp tests and the
//! calls across a relay of the other kind at the end, which need `rnsd-rs`
//! (`cargo build -p rns-tools --bin rnsd-rs` in rsReticulum, or `RUST_RNSD_BIN`).
//! Run the rest explicitly with the command documented in the repository README.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rns_identity::identity::Identity;
use rns_identity::name_hash::name_hash;
use rns_interface::tcp::{TcpClientConfig, TcpServerConfig, spawn_tcp_client, spawn_tcp_server};
use rns_transport::actor::TransportActor;
use rns_transport::constants::InterfaceDirection as TransportInterfaceDirection;
use rns_transport::messages::{
    AnnounceHandlerEvent, InterfaceEntry, OutboundRequest, TransportMessage, TransportQuery,
    TransportQueryResponse,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use lxst_core::{
    CallMode, CallRole, FrameStreamEvent, Profile, RawAudioFrame, RawBitDepth, SignallingStatus,
    SyntheticSource, SyntheticSourceKind, TELEPHONY_DESTINATION_NAME,
};
use lxst_telephony::{
    ActiveCallSnapshot, TelephonyCommand, TelephonyControl, TelephonyDriveStep,
    TelephonyRnsEndpoint, TelephonyRuntimeCore, TelephonyService, TelephonyServiceConfig,
    TelephonyServiceEvent, telephony_inbound_destination,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate is under rsLXST/crates/lxst-telephony")
        .to_path_buf()
}

fn helper_script() -> PathBuf {
    repo_root().join("tools/interop/lxst_telephone_helper.py")
}

fn python_interpreter() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| {
        if cfg!(windows) {
            "python".to_string()
        } else {
            "python3".to_string()
        }
    })
}

fn temp_storage(name: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{now}-{counter}", std::process::id()))
}

fn free_tcp_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free TCP port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

struct PythonTelephoneHost {
    child: Child,
    stdin: ChildStdin,
    events: std::sync::mpsc::Receiver<Value>,
    stashed_events: Mutex<Vec<Value>>,
    history: Mutex<Vec<Value>>,
    storage: PathBuf,
    log_path: PathBuf,
}

impl PythonTelephoneHost {
    fn spawn(port: u16) -> Self {
        Self::spawn_with_tcp(port, "client", false)
    }

    fn spawn_transport_hub(port: u16) -> Self {
        Self::spawn_with_tcp(port, "server", true)
    }

    fn spawn_with_tcp(port: u16, tcp_role: &str, enable_transport: bool) -> Self {
        Self::spawn_with_env(port, tcp_role, enable_transport, &[])
    }

    fn spawn_with_env(
        port: u16,
        tcp_role: &str,
        enable_transport: bool,
        env: &[(&str, &str)],
    ) -> Self {
        Self::spawn_script(&helper_script(), port, tcp_role, enable_transport, env)
    }

    fn spawn_script(
        script: &Path,
        port: u16,
        tcp_role: &str,
        enable_transport: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let storage = temp_storage("rs-lxst-python-telephone-live");
        std::fs::create_dir_all(&storage).expect("create Python helper storage");
        let log_path = storage.join("helper.stderr.log");
        let log_file = std::fs::File::create(&log_path).expect("create helper stderr log");

        let mut command = Command::new(python_interpreter());
        command
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg(script)
            .arg("--mode")
            .arg("host")
            .arg("--storage-dir")
            .arg(&storage)
            .arg("--tcp-role")
            .arg(tcp_role)
            .arg("--tcp-host")
            .arg("127.0.0.1")
            .arg("--tcp-port")
            .arg(port.to_string())
            .arg("--ring-time")
            .arg("20")
            .arg("--wait-time")
            .arg("20")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log_file));
        add_platform_python_library_path(&mut command);
        for (key, value) in env {
            command.env(key, value);
        }
        if enable_transport {
            command.arg("--enable-transport");
        }

        let mut child = command.spawn().expect("spawn Python LXST Telephone helper");

        let stdout = child.stdout.take().expect("helper stdout");
        let stdin = child.stdin.take().expect("helper stdin");
        let (event_tx, events) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = line.expect("read helper stdout");
                let event: Value = serde_json::from_str(&line).expect("helper JSON line");
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            events,
            stashed_events: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            storage,
            log_path,
        }
    }

    fn send(&mut self, command: Value) {
        writeln!(self.stdin, "{command}").expect("write helper command");
        self.stdin.flush().expect("flush helper command");
    }

    fn try_wait_event(&self, name: &str, timeout: Duration) -> Option<Value> {
        {
            let mut stashed = self.stashed_events.lock().expect("event stash");
            if let Some(index) = stashed.iter().position(|event| event["event"] == name) {
                return Some(stashed.remove(index));
            }
        }

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self
                .events
                .recv_timeout(remaining.min(Duration::from_millis(250)))
            {
                Ok(event) => {
                    self.history
                        .lock()
                        .expect("event history")
                        .push(event.clone());
                    if event["event"] == name {
                        return Some(event);
                    }
                    if event["event"] == "FATAL" || event["event"] == "ERROR" {
                        panic!("Python helper emitted failure event: {event}")
                    }
                    self.stashed_events.lock().expect("event stash").push(event);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("Python helper stdout closed before {name}")
                }
            }
        }
        None
    }

    fn wait_event(&self, name: &str, timeout: Duration) -> Value {
        if let Some(event) = self.try_wait_event(name, timeout) {
            return event;
        }

        panic!(
            "timed out waiting for Python helper event {name}; stashed events:\n{}\nstderr:\n{}",
            serde_json::to_string_pretty(&*self.stashed_events.lock().expect("event stash"))
                .unwrap_or_default(),
            self.stderr_log()
        );
    }

    fn stderr_log(&self) -> String {
        let mut log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let rns_log_path = self.storage.join("rns_debug.log");
        if let Ok(rns_log) = std::fs::read_to_string(&rns_log_path) {
            if !log.is_empty() {
                log.push_str("\n\n");
            }
            log.push_str("Reticulum log:\n");
            log.push_str(&rns_log);
        }
        log
    }
}

fn add_platform_python_library_path(command: &mut Command) {
    if !cfg!(target_os = "macos") {
        return;
    }

    let defaults = [
        "/opt/homebrew/opt/opus/lib",
        "/opt/homebrew/opt/libogg/lib",
        "/usr/local/opt/opus/lib",
        "/usr/local/opt/libogg/lib",
    ];
    let existing = std::env::var("DYLD_FALLBACK_LIBRARY_PATH").unwrap_or_default();
    let mut paths = Vec::new();
    paths.extend(
        defaults
            .iter()
            .filter(|path| Path::new(path).exists())
            .map(|path| (*path).to_string()),
    );
    if !existing.is_empty() {
        paths.push(existing);
    }
    if !paths.is_empty() {
        command.env("DYLD_FALLBACK_LIBRARY_PATH", paths.join(":"));
    }
}

impl Drop for PythonTelephoneHost {
    fn drop(&mut self) {
        let _ = writeln!(self.stdin, "{}", json!({"cmd": "shutdown"}));
        let _ = self.stdin.flush();
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.storage);
    }
}

async fn spawn_rust_actor_and_tcp(
    port: u16,
) -> (
    mpsc::Sender<TransportMessage>,
    rns_interface::traits::InterfaceHandle,
    mpsc::Receiver<rns_interface::traits::InterfaceHandle>,
) {
    unstamp_rust_announces();
    let (actor, actor_tx) = TransportActor::new();
    tokio::spawn(actor.run());

    let (handle_tx, handle_rx) = mpsc::channel(8);
    let id_gen = Arc::new(AtomicU64::new(9_000));
    let server_id = id_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let cfg = TcpServerConfig::new("rs-lxst-python-telephone-live", "127.0.0.1", port);
    let server = spawn_tcp_server(cfg, server_id, id_gen, actor_tx.clone(), handle_tx)
        .await
        .expect("spawn Rust TCP server");
    server.owner.mark_registered();

    (actor_tx, server, handle_rx)
}

async fn spawn_rust_actor_and_tcp_client(
    name: &str,
    port: u16,
    interface_id: u64,
) -> (mpsc::Sender<TransportMessage>, tokio::task::JoinHandle<()>) {
    let actor_tx = spawn_rust_actor().await;
    let read_task = attach_rust_tcp_client(actor_tx.clone(), name, port, interface_id).await;

    (actor_tx, read_task)
}

async fn spawn_rust_actor() -> mpsc::Sender<TransportMessage> {
    unstamp_rust_announces();
    let (actor, actor_tx) = TransportActor::new();
    tokio::spawn(actor.run());
    actor_tx
}

/// By default a Rust announce waits for its value-25 stamp, seconds to tens of
/// seconds in a debug build. Only stamping is turned off: a stamp test's
/// enforcement stays as it configured it.
fn unstamp_rust_announces() {
    rns_runtime::destination_stamper::configure(
        rns_transport::stamps::StampPolicy {
            stamp_destinations: false,
            ..rns_runtime::destination_stamper::policy()
        },
        None,
    );
}

async fn attach_rust_tcp_client(
    actor_tx: mpsc::Sender<TransportMessage>,
    name: &str,
    port: u16,
    interface_id: u64,
) -> tokio::task::JoinHandle<()> {
    let mut cfg = TcpClientConfig::new(name, "127.0.0.1", port);
    cfg.connect_timeout_secs = 1;
    cfg.max_reconnect_tries = Some(5);

    let handle = spawn_tcp_client(cfg, interface_id, actor_tx.clone())
        .await
        .expect("spawn Rust TCP client");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !handle.online.load(Ordering::SeqCst) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "Rust TCP client {name} did not connect to Python transport hub"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (id, entry, read_task) = register_interface_entry(handle);
    actor_tx
        .send(TransportMessage::RegisterInterface { id, entry })
        .await
        .expect("register Rust TCP client interface");

    read_task
}

fn register_interface_entry(
    handle: rns_interface::traits::InterfaceHandle,
) -> (u64, InterfaceEntry, tokio::task::JoinHandle<()>) {
    let mode = match handle.mode {
        rns_interface::traits::InterfaceMode::AccessPoint => {
            rns_transport::constants::InterfaceMode::AccessPoint
        }
        rns_interface::traits::InterfaceMode::Roaming => {
            rns_transport::constants::InterfaceMode::Roaming
        }
        rns_interface::traits::InterfaceMode::Boundary => {
            rns_transport::constants::InterfaceMode::Boundary
        }
        rns_interface::traits::InterfaceMode::Gateway => {
            rns_transport::constants::InterfaceMode::Gateway
        }
        _ => rns_transport::constants::InterfaceMode::Gateway,
    };
    let direction = TransportInterfaceDirection {
        inbound: handle.direction.inbound,
        outbound: handle.direction.outbound,
    };
    let mut entry = InterfaceEntry::new(
        handle.name,
        mode,
        direction,
        handle.bitrate,
        handle.mtu,
        handle.tx,
    );
    entry.owner = handle.owner;
    entry.packet_capacity = handle.packet_capacity;
    entry.online = Some(handle.online);
    entry.rxb = handle.rxb;
    entry.txb = handle.txb;
    (handle.id, entry, handle.read_task)
}

async fn register_telephony_announce_handler(
    actor_tx: &mpsc::Sender<TransportMessage>,
) -> mpsc::Receiver<AnnounceHandlerEvent> {
    let (callback_tx, callback_rx) = mpsc::channel(16);
    actor_tx
        .send(TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(TELEPHONY_DESTINATION_NAME.to_string()),
            receive_path_responses: true,
            callback_tx,
        })
        .await
        .expect("register lxst.telephony announce handler");
    callback_rx
}

fn hash_from_ready(ready: &Value, field: &str) -> [u8; 16] {
    let bytes = hex::decode(ready[field].as_str().expect(field)).expect("hex hash");
    bytes
        .try_into()
        .expect("Reticulum identity/destination hashes are 16 bytes")
}

async fn send_rust_telephony_announce(
    actor_tx: &mpsc::Sender<TransportMessage>,
    identity: &Identity,
) -> [u8; 16] {
    let mut destination =
        telephony_inbound_destination(identity).expect("create Rust Telephone destination");
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after Unix epoch")
        .as_secs_f64();
    let raw = destination
        .announce_packet(identity, None, None, false, None, now)
        .expect("build Rust Telephone announce");
    actor_tx
        .send(TransportMessage::Outbound(OutboundRequest {
            raw: Bytes::from(raw),
            destination_hash: destination.hash,
        }))
        .await
        .expect("send Rust Telephone announce");
    destination.hash
}

struct RustHubNode {
    actor_tx: mpsc::Sender<TransportMessage>,
    identity: Identity,
    destination_hash: [u8; 16],
    read_task: Option<tokio::task::JoinHandle<()>>,
    service_task: tokio::task::JoinHandle<()>,
    control_tx: mpsc::Sender<TelephonyControl>,
    event_rx: mpsc::Receiver<TelephonyServiceEvent>,
}

impl RustHubNode {
    async fn spawn(name: &str, port: u16, interface_id: u64) -> Self {
        Self::spawn_on_tcp(name, port, interface_id, true).await
    }

    /// A node that does not announce. Its first announce, echoed back by the hub
    /// next to the hub's own and the awaited one, can push its new interface
    /// into an ingress burst that holds the awaited announce (Interface.py).
    async fn spawn_silent(name: &str, port: u16, interface_id: u64) -> Self {
        Self::spawn_on_tcp(name, port, interface_id, false).await
    }

    async fn spawn_on_tcp(name: &str, port: u16, interface_id: u64, announce: bool) -> Self {
        let (actor_tx, read_task) = spawn_rust_actor_and_tcp_client(name, port, interface_id).await;
        Self::spawn_with_actor(
            actor_tx,
            Some(read_task),
            TelephonyServiceConfig {
                poll_interval: Duration::from_millis(20),
                incoming_ring_timeout: Some(Duration::from_secs(15)),
                outgoing_call_timeout: Some(Duration::from_secs(15)),
                media_frames_per_tick: 4,
                announce_on_start: announce,
                announce_interval: TelephonyServiceConfig::default()
                    .announce_interval
                    .filter(|_| announce),
                ..TelephonyServiceConfig::default()
            },
        )
        .await
    }

    async fn spawn_before_tcp() -> Self {
        let actor_tx = spawn_rust_actor().await;
        Self::spawn_with_actor(
            actor_tx,
            None,
            TelephonyServiceConfig {
                poll_interval: Duration::from_millis(20),
                incoming_ring_timeout: Some(Duration::from_secs(15)),
                outgoing_call_timeout: Some(Duration::from_secs(15)),
                media_frames_per_tick: 4,
                startup_announce_retry_interval: Some(Duration::from_millis(100)),
                startup_announce_retries: 10,
                announce_interval: None,
                ..TelephonyServiceConfig::default()
            },
        )
        .await
    }

    async fn spawn_with_actor(
        actor_tx: mpsc::Sender<TransportMessage>,
        read_task: Option<tokio::task::JoinHandle<()>>,
        config: TelephonyServiceConfig,
    ) -> Self {
        let identity = Identity::new();
        let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &identity)
            .expect("register Rust LXST Telephone endpoint");
        let destination_hash = endpoint.destination_hash;
        let (control_tx, control_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(64);
        let service = TelephonyService::with_config(
            endpoint,
            TelephonyRuntimeCore::new(),
            control_rx,
            event_tx,
            config,
        );
        let service_task = tokio::spawn(service.run());

        Self {
            actor_tx,
            identity,
            destination_hash,
            read_task,
            service_task,
            control_tx,
            event_rx,
        }
    }

    async fn attach_tcp(&mut self, name: &str, port: u16, interface_id: u64) {
        assert!(
            self.read_task.is_none(),
            "RustHubNode TCP client is already attached"
        );
        self.read_task =
            Some(attach_rust_tcp_client(self.actor_tx.clone(), name, port, interface_id).await);
    }

    async fn shutdown(mut self) {
        let _ = self.control_tx.send(TelephonyControl::Shutdown).await;
        let _ = wait_matching_service_event(
            &mut self.event_rx,
            Duration::from_secs(5),
            "service stopped",
            |event| matches!(event, TelephonyServiceEvent::Stopped),
        )
        .await;
        let _ = self.service_task.await;
        if let Some(read_task) = self.read_task {
            read_task.abort();
        }
    }
}

async fn wait_for_announce(
    announce_rx: &mut mpsc::Receiver<AnnounceHandlerEvent>,
    destination_hash: [u8; 16],
    timeout: Duration,
    label: &str,
) -> AnnounceHandlerEvent {
    tokio::time::timeout(timeout, async {
        while let Some(event) = announce_rx.recv().await {
            if event.destination_hash == destination_hash {
                return event;
            }
        }
        panic!("announce handler channel closed before {label}");
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for announce {label}"))
}

fn drive_steps_contain_call_termination(
    steps: &[TelephonyDriveStep],
    expected_reason: Option<SignallingStatus>,
) -> bool {
    steps.iter().any(|driven| {
        driven.step.commands.iter().any(|command| {
            matches!(
                command,
                TelephonyCommand::CallTerminated { reason, .. } if *reason == expected_reason
            )
        })
    })
}

async fn drive_until_active_status(
    endpoint: &mut TelephonyRnsEndpoint,
    core: &mut TelephonyRuntimeCore,
    helper: &PythonTelephoneHost,
    status: SignallingStatus,
    timeout: Duration,
    context: &str,
) -> ActiveCallSnapshot {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        endpoint
            .try_drive_ready(core)
            .expect("drive Rust Telephone endpoint");

        if let Some(call) = core.snapshot().active_call
            && call.status == status
        {
            return call;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "{context}; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn drive_until_call_cleared(
    endpoint: &mut TelephonyRnsEndpoint,
    core: &mut TelephonyRuntimeCore,
    helper: &PythonTelephoneHost,
    expected_reason: Option<SignallingStatus>,
    timeout: Duration,
    context: &str,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut saw_expected_termination = false;

    loop {
        let steps = endpoint
            .try_drive_ready(core)
            .expect("drive Rust Telephone endpoint");
        saw_expected_termination |= drive_steps_contain_call_termination(&steps, expected_reason);

        if core.snapshot().active_call.is_none() && saw_expected_termination {
            return;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "{context}; saw expected termination: {saw_expected_termination}; snapshot: {:?}; helper stderr:\n{}",
            core.snapshot(),
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_service_event(
    event_rx: &mut mpsc::Receiver<TelephonyServiceEvent>,
    timeout: Duration,
    label: &str,
) -> TelephonyServiceEvent {
    tokio::time::timeout(timeout, event_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for service event {label}"))
        .unwrap_or_else(|| panic!("service event channel closed before {label}"))
}

async fn wait_matching_service_event(
    event_rx: &mut mpsc::Receiver<TelephonyServiceEvent>,
    timeout: Duration,
    label: &str,
    matches: impl Fn(&TelephonyServiceEvent) -> bool,
) -> TelephonyServiceEvent {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        assert!(
            now < deadline,
            "timed out waiting for service event {label}"
        );
        let event = wait_service_event(event_rx, deadline - now, label).await;
        if matches(&event) {
            return event;
        }
    }
}

/// A caller is Established once it answers CONNECTING (Telephony.py `signal`),
/// but takes media only after the callee's ESTABLISHED starts its audio.
async fn wait_caller_audio_started(
    event_rx: &mut mpsc::Receiver<TelephonyServiceEvent>,
    timeout: Duration,
) {
    wait_matching_service_event(event_rx, timeout, "caller audio start", |event| {
        let TelephonyServiceEvent::Drive(drive) = event else {
            return false;
        };
        drive
            .step
            .commands
            .iter()
            .any(|command| matches!(command, TelephonyCommand::StartAudioPipelines { .. }))
    })
    .await;
}

fn synthetic_frame_for_profile(profile: Profile) -> RawAudioFrame {
    SyntheticSource::new(
        profile.channels(),
        profile.sample_rate_hz(),
        profile.sample_frames_per_packet(),
        SyntheticSourceKind::Sine {
            frequency_hz: 440.0,
            amplitude: 0.25,
        },
    )
    .unwrap()
    .next_raw_frame()
    .unwrap()
}

const OPUS_VOICE_PROFILES: [Profile; 5] = [
    Profile::QualityMedium,
    Profile::QualityHigh,
    Profile::QualityMax,
    Profile::LatencyLow,
    Profile::LatencyUltraLow,
];
const PYTHON_HEADLESS_AUDIO_SAMPLE_RATE_HZ: usize = 48_000;
const PYTHON_HEADLESS_AUDIO_CHANNELS: usize = 1;

fn python_opus_profile(profile: Profile) -> u8 {
    match profile {
        Profile::QualityMedium | Profile::LatencyLow | Profile::LatencyUltraLow => 0x01,
        Profile::QualityHigh => 0x02,
        Profile::QualityMax => 0x03,
        Profile::BandwidthUltraLow | Profile::BandwidthVeryLow | Profile::BandwidthLow => {
            panic!("{} is not an Opus telephony profile", profile.name())
        }
    }
}

fn python_preferred_profile_signal(profile: Profile) -> u64 {
    0xFF + u64::from(profile.wire_value())
}

fn python_headless_output_frames(profile: Profile) -> usize {
    PYTHON_HEADLESS_AUDIO_SAMPLE_RATE_HZ * usize::from(profile.frame_time_ms()) / 1000
}

struct RustCallerSession {
    profile: Profile,
    actor_tx: mpsc::Sender<TransportMessage>,
    rust_destination_hash: [u8; 16],
    accepted_read_task: tokio::task::JoinHandle<()>,
    server_read_task: tokio::task::JoinHandle<()>,
    service_task: tokio::task::JoinHandle<()>,
    control_tx: mpsc::Sender<TelephonyControl>,
    event_rx: mpsc::Receiver<TelephonyServiceEvent>,
    helper: PythonTelephoneHost,
}

impl RustCallerSession {
    async fn establish(profile: Profile) -> Option<Self> {
        let port = free_tcp_port();
        let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
        let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

        let mut helper = PythonTelephoneHost::spawn(port);
        let ready = helper.wait_event("READY", Duration::from_secs(10));
        assert_eq!(ready["headless_audio"], true);
        if !ready["opus_available"].as_bool().unwrap_or(false) {
            eprintln!("Python LXST Opus codec unavailable -> skipping live Opus media interop");
            server.read_task.abort();
            return None;
        }
        let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
        let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

        let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
            .await
            .expect("timeout waiting for Python Telephone helper TCP connection")
            .expect("TCP accept channel closed");
        let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
        actor_tx
            .send(TransportMessage::RegisterInterface {
                id: accepted_id,
                entry: accepted_entry,
            })
            .await
            .expect("register accepted Python TCP interface");

        helper
            .send(json!({"cmd": "announce", "id": format!("announce-{}", profile.abbreviation())}));
        let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
        assert_eq!(
            announced["destination_hash"],
            hex::encode(remote_destination_hash)
        );
        let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
            .expect("announce handler channel closed");
        assert_eq!(announce.destination_hash, remote_destination_hash);

        let local_identity = Identity::new();
        let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
            .expect("register Rust LXST Telephone endpoint");
        let rust_destination_hash = endpoint.destination_hash;
        let (control_tx, control_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let service = TelephonyService::with_config(
            endpoint,
            TelephonyRuntimeCore::new(),
            control_rx,
            event_tx,
            TelephonyServiceConfig {
                poll_interval: Duration::from_millis(20),
                incoming_ring_timeout: Some(Duration::from_secs(10)),
                outgoing_call_timeout: Some(Duration::from_secs(10)),
                media_frames_per_tick: 4,
                ..TelephonyServiceConfig::default()
            },
        );
        let service_task = tokio::spawn(service.run());

        control_tx
            .send(TelephonyControl::Call {
                remote_identity: remote_identity_hash,
                profile: Some(profile),
                mode: None,
                discovery_timeout: Duration::from_secs(5),
            })
            .await
            .expect("send Rust Telephone service call control");
        wait_matching_service_event(
            &mut event_rx,
            Duration::from_secs(5),
            "outgoing call started",
            |event| matches!(event, TelephonyServiceEvent::OutgoingCallStarted { .. }),
        )
        .await;

        helper.wait_event("RINGING", Duration::from_secs(5));
        helper.send(json!({"cmd": "answer", "id": format!("answer-{}", profile.abbreviation())}));
        let answered = helper.wait_event("ANSWERED", Duration::from_secs(5));
        assert_eq!(answered["accepted"], true);
        helper.wait_event("ESTABLISHED", Duration::from_secs(5));
        wait_caller_audio_started(&mut event_rx, Duration::from_secs(5)).await;
        wait_matching_service_event(
            &mut event_rx,
            Duration::from_secs(5),
            "established snapshot",
            |event| {
                matches!(
                    event,
                    TelephonyServiceEvent::Snapshot(snapshot)
                        if snapshot.active_call.as_ref().is_some_and(|call| {
                            call.status == SignallingStatus::Established
                                && call.profile == Some(profile)
                        })
                )
            },
        )
        .await;

        Some(Self {
            profile,
            actor_tx,
            rust_destination_hash,
            accepted_read_task,
            server_read_task: server.read_task,
            service_task,
            control_tx,
            event_rx,
            helper,
        })
    }

    async fn send_rust_opus_to_python(&mut self) -> Value {
        self.control_tx
            .send(TelephonyControl::SendOpusFrames {
                profile: self.profile,
                frames: vec![synthetic_frame_for_profile(self.profile)],
            })
            .await
            .expect("send Opus media frame through Rust Telephone service");
        wait_matching_service_event(
            &mut self.event_rx,
            Duration::from_secs(5),
            "opus media sent",
            |event| {
                matches!(
                    event,
                    TelephonyServiceEvent::MediaSent {
                        frames: 1,
                        packets: 1,
                        ..
                    }
                )
            },
        )
        .await;

        self.helper
            .wait_event("MEDIA_FRAME", Duration::from_secs(5))
    }

    async fn send_python_opus_to_rust(&mut self) {
        let python_frame = synthetic_frame_for_profile(self.profile);
        self.helper.send(json!({
            "cmd": "send_opus_frame",
            "id": format!("py-opus-{}", self.profile.abbreviation()),
            "profile": python_opus_profile(self.profile),
            "channels": self.profile.channels(),
            "samplerate": self.profile.sample_rate_hz(),
            "samples": python_frame.samples,
        }));
        let opus_sent = self
            .helper
            .wait_event("OPUS_FRAME_SENT", Duration::from_secs(5));
        assert_eq!(opus_sent["sent"], true);

        let service_decoded = wait_matching_service_event(
            &mut self.event_rx,
            Duration::from_secs(5),
            "service decoded Opus frames",
            |event| {
                matches!(
                    event,
                    TelephonyServiceEvent::OpusFramesReceived {
                        profile: event_profile,
                        frames,
                        ..
                    } if *event_profile == self.profile
                        && frames.len() == 1
                        && frames[0].channels == self.profile.channels()
                        && frames[0].sample_frames() == self.profile.sample_frames_per_packet()
                )
            },
        )
        .await;
        assert!(matches!(
            service_decoded,
            TelephonyServiceEvent::OpusFramesReceived { .. }
        ));
    }

    async fn switch_rust_profile(&mut self, profile: Profile) {
        self.control_tx
            .send(TelephonyControl::SwitchProfile { profile })
            .await
            .expect("send Rust Telephone service profile switch control");
        wait_matching_service_event(
            &mut self.event_rx,
            Duration::from_secs(5),
            "Rust profile switch snapshot",
            |event| {
                matches!(
                    event,
                    TelephonyServiceEvent::Snapshot(snapshot)
                        if snapshot.active_call.as_ref().is_some_and(|call| {
                            call.status == SignallingStatus::Established
                                && call.profile == Some(profile)
                        })
                )
            },
        )
        .await;
        self.wait_python_preferred_profile_signal(profile);
        self.wait_python_active_profile(profile).await;
        self.profile = profile;
    }

    async fn switch_python_profile(&mut self, profile: Profile) {
        let expected_profile = u64::from(profile.wire_value());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut attempt = 0_u64;
        let switched = loop {
            attempt += 1;
            self.helper.send(json!({
                "cmd": "switch_profile",
                "id": format!("py-switch-{}-{attempt}", profile.abbreviation()),
                "profile": profile.wire_value(),
            }));
            let event = self
                .helper
                .wait_event("PROFILE_SWITCHED", Duration::from_secs(5));
            if event["profile"].as_u64() == Some(expected_profile) {
                break event;
            }

            assert!(
                Instant::now() < deadline,
                "Python helper did not switch to profile {}; last event: {}; stderr:\n{}",
                expected_profile,
                event,
                self.helper.stderr_log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(switched["profile"].as_u64(), Some(expected_profile));

        wait_matching_service_event(
            &mut self.event_rx,
            Duration::from_secs(5),
            "Python profile switch reaches Rust",
            |event| {
                matches!(
                    event,
                    TelephonyServiceEvent::Snapshot(snapshot)
                        if snapshot.active_call.as_ref().is_some_and(|call| {
                            call.status == SignallingStatus::Established
                                && call.profile == Some(profile)
                        })
                )
            },
        )
        .await;
        self.profile = profile;
    }

    fn wait_python_preferred_profile_signal(&self, profile: Profile) {
        let expected_signal = python_preferred_profile_signal(profile);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for Python preferred-profile signal {}; helper stderr:\n{}",
                expected_signal,
                self.helper.stderr_log()
            );
            if let Some(event) = self
                .helper
                .try_wait_event("SIGNALS", remaining.min(Duration::from_millis(500)))
            {
                let saw_expected = event["signals"].as_array().is_some_and(|signals| {
                    signals
                        .iter()
                        .any(|signal| signal.as_u64() == Some(expected_signal))
                });
                if saw_expected {
                    return;
                }
            }
        }
    }

    async fn wait_python_active_profile(&mut self, profile: Profile) {
        let expected_profile = u64::from(profile.wire_value());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut attempt = 0_u64;
        loop {
            attempt += 1;
            self.helper.send(json!({
                "cmd": "snapshot",
                "id": format!("profile-switch-{}-{attempt}", profile.abbreviation()),
            }));
            let event = self
                .helper
                .wait_event("SNAPSHOT", Duration::from_millis(500));
            let active_call_profile = event["active_call"]["profile"].as_u64();
            let active_profile = event["active_profile"].as_u64();
            if active_profile == Some(expected_profile)
                && active_call_profile == Some(expected_profile)
            {
                return;
            }

            assert!(
                Instant::now() < deadline,
                "timed out waiting for Python active profile {}; last snapshot: {}; helper stderr:\n{}",
                expected_profile,
                event,
                self.helper.stderr_log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn shutdown(self) {
        let Self {
            actor_tx,
            rust_destination_hash,
            accepted_read_task,
            server_read_task,
            service_task,
            control_tx,
            mut event_rx,
            ..
        } = self;

        control_tx
            .send(TelephonyControl::Shutdown)
            .await
            .expect("send Rust Telephone service shutdown");
        wait_matching_service_event(
            &mut event_rx,
            Duration::from_secs(2),
            "service stopped",
            |event| matches!(event, TelephonyServiceEvent::Stopped),
        )
        .await;
        service_task.await.expect("join Rust Telephone service");
        actor_tx
            .send(TransportMessage::DeregisterDestination {
                hash: rust_destination_hash,
            })
            .await
            .expect("deregister Rust Telephone endpoint");
        accepted_read_task.abort();
        server_read_task.abort();
    }
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_telephone_announce_reaches_rust_transport() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    let destination_hash = hash_from_ready(&ready, "destination_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");

    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(announced["destination_hash"], hex::encode(destination_hash));

    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, destination_hash);
    assert_eq!(announce.name_hash, name_hash(TELEPHONY_DESTINATION_NAME));
    assert!(
        announce.public_key.is_some(),
        "Python Telephone announce did not include public key"
    );
    assert!(
        announce.hops <= 1,
        "direct Python TCP announce should not arrive through a routed path"
    );

    let local_identity = Identity::new();
    let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");
    let peer = endpoint
        .discover_remote_telephony_peer(remote_identity_hash, Duration::from_secs(2))
        .await
        .expect("discover Python Telephone peer from live announce cache");
    assert_eq!(peer.identity_hash, remote_identity_hash);
    assert_eq!(peer.destination_hash, destination_hash);
    assert_eq!(
        peer.public_key,
        announce.public_key.expect("announce public key")
    );
    endpoint
        .await_path_to_identity(remote_identity_hash, Duration::from_secs(2))
        .await
        .expect("live Python Telephone path is available after announce");
    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");

    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_to_rust_call_establishes_through_python_transport_hub() {
    let mut pair = RustCallPair::connect().await;
    pair.establish(Profile::QualityMedium).await;
    pair.caller
        .control_tx
        .send(TelephonyControl::Hangup {
            ring_timeout: false,
        })
        .await
        .expect("send caller hangup");
    for node in [&mut pair.caller, &mut pair.callee] {
        wait_matching_service_event(
            &mut node.event_rx,
            Duration::from_secs(15),
            "call terminated through upstream transport",
            |event| matches!(event, TelephonyServiceEvent::CallTerminated { .. }),
        )
        .await;
    }
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_telephony_startup_retry_announces_after_late_tcp_hub_connection() {
    let port = free_tcp_port();
    let hub = PythonTelephoneHost::spawn_transport_hub(port);
    let ready = hub.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);

    let caller = RustHubNode::spawn_silent("rust-hub-caller-late", port, 13_001).await;
    let mut caller_announces = register_telephony_announce_handler(&caller.actor_tx).await;
    let mut callee = RustHubNode::spawn_before_tcp().await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    callee
        .attach_tcp("rust-hub-callee-late", port, 14_001)
        .await;

    let announce = wait_for_announce(
        &mut caller_announces,
        callee.destination_hash,
        // The first announce follows one job interval, as in Python.
        Duration::from_secs(10),
        "callee startup retry after late TCP hub connection",
    )
    .await;
    assert_eq!(announce.name_hash, name_hash(TELEPHONY_DESTINATION_NAME));
    assert!(
        announce.public_key.is_some(),
        "Rust callee startup retry announce through Python hub did not include a public key"
    );
    assert!(
        announce.hops >= 1,
        "callee startup retry announce should arrive through at least one transport hop"
    );

    caller.shutdown().await;
    callee.shutdown().await;
    drop(hub);
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_outgoing_call_reaches_python_ringing_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let link_id = endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start outgoing Rust -> Python Telephone link");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");

        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            let call = snapshot.active_call.expect("active outgoing call");
            assert_eq!(call.link_id, link_id);
            assert_eq!(call.remote_identity, remote_identity_hash);
            assert_eq!(call.role, CallRole::Outgoing);
            break;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "Rust outgoing call did not reach Python RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let ringing = helper.wait_event("RINGING", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ringing["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_outgoing_call_establishes_when_python_answers_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let link_id = endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start outgoing Rust -> Python Telephone link");

    let ringing_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");
        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ringing_deadline,
            "Rust outgoing call did not reach Python RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let ringing = helper.wait_event("RINGING", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ringing["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    helper.send(json!({"cmd": "answer", "id": "answer-1"}));
    let answered = helper.wait_event("ANSWERED", Duration::from_secs(5));
    assert_eq!(answered["accepted"], true);
    let established = helper.wait_event("ESTABLISHED", Duration::from_secs(5));
    assert_eq!(
        established["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    let established_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");
        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Established)
        {
            let call = snapshot.active_call.expect("active outgoing call");
            assert_eq!(call.link_id, link_id);
            assert_eq!(call.remote_identity, remote_identity_hash);
            assert_eq!(call.role, CallRole::Outgoing);
            assert_eq!(call.profile, Some(Profile::DEFAULT));
            break;
        }
        assert!(
            tokio::time::Instant::now() < established_deadline,
            "Rust outgoing call did not reach ESTABLISHED after Python answer; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_outgoing_call_reaches_rust_incoming_ringing_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let python_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(rust_destination_hash, endpoint.destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");

        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            let call = snapshot.active_call.expect("active incoming call");
            assert_eq!(call.remote_identity, python_identity_hash);
            assert_eq!(call.role, CallRole::Incoming);
            break;
        }

        assert!(
            tokio::time::Instant::now() < deadline,
            "Python outgoing call did not reach Rust incoming RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_outgoing_call_establishes_when_rust_answers_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let python_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(rust_destination_hash, endpoint.destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    let ringing_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");

        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            let call = snapshot.active_call.expect("active incoming call");
            assert_eq!(call.remote_identity, python_identity_hash);
            assert_eq!(call.role, CallRole::Incoming);
            break;
        }

        assert!(
            tokio::time::Instant::now() < ringing_deadline,
            "Python outgoing call did not reach Rust incoming RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let link_id = core.active_call().expect("active incoming call").link_id;
    let commands = core
        .answer_active(link_id)
        .expect("answer active Rust incoming call");
    endpoint
        .execute_commands(&commands)
        .expect("send Rust answer signalling");
    let established = helper.wait_event("ESTABLISHED", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        established["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    // Python's callback proves it consumed our ESTABLISHED offer, but its
    // reciprocal ESTABLISHED packet can still be in the TCP/transport queues.
    // Drive until that causal response reaches the Rust state machine instead
    // of treating one empty nonblocking poll as a failed handshake.
    let call = drive_until_active_status(
        &mut endpoint,
        &mut core,
        &helper,
        SignallingStatus::Established,
        Duration::from_secs(5),
        "Rust incoming call did not consume Python's ESTABLISHED response",
    )
    .await;
    assert_eq!(call.remote_identity, python_identity_hash);
    assert_eq!(call.role, CallRole::Incoming);
    assert_eq!(call.status, SignallingStatus::Established);
    assert_eq!(call.profile, Some(Profile::DEFAULT));
    assert!(call.answered);

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_hangup_after_python_answer_ends_python_call_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start outgoing Rust -> Python Telephone link");

    let ringing_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");
        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ringing_deadline,
            "Rust outgoing call did not reach Python RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    helper.wait_event("RINGING", Duration::from_secs(5));
    helper.send(json!({"cmd": "answer", "id": "answer-1"}));
    let answered = helper.wait_event("ANSWERED", Duration::from_secs(5));
    assert_eq!(answered["accepted"], true);
    helper.wait_event("ESTABLISHED", Duration::from_secs(5));

    let established_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");
        if core
            .snapshot()
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Established)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < established_deadline,
            "Rust outgoing call did not reach ESTABLISHED after Python answer; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let commands = core
        .hangup_active(false)
        .expect("hang up active Rust outgoing call");
    endpoint
        .execute_commands(&commands)
        .expect("send Rust link close");
    assert!(core.snapshot().active_call.is_none());
    let ended = helper.wait_event("ENDED", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ended["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_hangup_after_rust_answer_ends_rust_call_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let python_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(rust_destination_hash, endpoint.destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    let ringing_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint");

        let snapshot = core.snapshot();
        if snapshot
            .active_call
            .as_ref()
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            let call = snapshot.active_call.expect("active incoming call");
            assert_eq!(call.remote_identity, python_identity_hash);
            assert_eq!(call.role, CallRole::Incoming);
            break;
        }

        assert!(
            tokio::time::Instant::now() < ringing_deadline,
            "Python outgoing call did not reach Rust incoming RINGING; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let link_id = core.active_call().expect("active incoming call").link_id;
    let commands = core
        .answer_active(link_id)
        .expect("answer active Rust incoming call");
    endpoint
        .execute_commands(&commands)
        .expect("send Rust answer signalling");
    helper.wait_event("ESTABLISHED", Duration::from_secs(5));

    let established_call = drive_until_active_status(
        &mut endpoint,
        &mut core,
        &helper,
        SignallingStatus::Established,
        Duration::from_secs(5),
        "Rust incoming call did not consume Python's ESTABLISHED response before hangup",
    )
    .await;
    assert_eq!(established_call.link_id, link_id);
    assert_eq!(established_call.role, CallRole::Incoming);

    helper.send(json!({"cmd": "hangup", "id": "hangup-1"}));
    helper.wait_event("HUNG_UP", Duration::from_secs(5));

    let ended_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint after Python hangup");
        if core.snapshot().active_call.is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ended_deadline,
            "Rust incoming call did not clear after Python hangup; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_outgoing_call_receives_python_busy_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "set_busy", "id": "busy-1", "busy": true}));
    let busy_set = helper.wait_event("BUSY_SET", Duration::from_secs(5));
    assert_eq!(busy_set["busy"], true);

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start outgoing Rust -> busy Python Telephone link");

    drive_until_call_cleared(
        &mut endpoint,
        &mut core,
        &helper,
        Some(SignallingStatus::Busy),
        Duration::from_secs(10),
        "Rust caller did not terminate with BUSY from Python callee",
    )
    .await;

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_outgoing_call_receives_rust_busy_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    core.set_external_busy(true);
    let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(rust_destination_hash, endpoint.destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-busy-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    let busy_signal = u64::from(SignallingStatus::Busy.wire_value());
    let busy_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut busy_callback = None;
    let mut saw_busy_signal = false;
    loop {
        endpoint
            .try_drive_ready(&mut core)
            .expect("drive Rust Telephone endpoint while waiting for Python busy");
        if let Some(busy) = helper.try_wait_event("BUSY", Duration::from_millis(10)) {
            busy_callback = Some(busy);
            saw_busy_signal = true;
            break;
        }
        if let Some(signals) = helper.try_wait_event("SIGNALS", Duration::from_millis(10)) {
            saw_busy_signal |= signals["signals"].as_array().is_some_and(|signals| {
                signals
                    .iter()
                    .any(|signal| signal.as_u64() == Some(busy_signal))
            });
            if saw_busy_signal {
                break;
            }
        }

        assert!(
            tokio::time::Instant::now() < busy_deadline,
            "Python caller did not receive BUSY from externally busy Rust callee; helper stderr:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if let Some(busy) = busy_callback {
        let local_identity_hash = hex::encode(local_identity.hash);
        assert_eq!(
            busy["identity_hash"].as_str(),
            Some(local_identity_hash.as_str())
        );
    }
    assert!(saw_busy_signal);
    assert!(core.snapshot().active_call.is_none());
    assert!(core.snapshot().external_busy);

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_outgoing_call_receives_python_reject_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start outgoing Rust -> Python Telephone link");

    let call = drive_until_active_status(
        &mut endpoint,
        &mut core,
        &helper,
        SignallingStatus::Ringing,
        Duration::from_secs(10),
        "Rust outgoing call did not reach Python RINGING before reject test",
    )
    .await;
    assert_eq!(call.remote_identity, remote_identity_hash);
    assert_eq!(call.role, CallRole::Outgoing);

    let ringing = helper.wait_event("RINGING", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ringing["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );
    helper.send(json!({"cmd": "hangup", "id": "reject-rust-1"}));
    helper.wait_event("HUNG_UP", Duration::from_secs(5));

    drive_until_call_cleared(
        &mut endpoint,
        &mut core,
        &helper,
        Some(SignallingStatus::Rejected),
        Duration::from_secs(10),
        "Rust caller did not terminate with REJECTED from Python callee",
    )
    .await;

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_outgoing_call_receives_rust_reject_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let python_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let mut core = TelephonyRuntimeCore::new();
    let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(rust_destination_hash, endpoint.destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-reject-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    let call = drive_until_active_status(
        &mut endpoint,
        &mut core,
        &helper,
        SignallingStatus::Ringing,
        Duration::from_secs(10),
        "Python outgoing call did not reach Rust incoming RINGING before reject test",
    )
    .await;
    assert_eq!(call.remote_identity, python_identity_hash);
    assert_eq!(call.role, CallRole::Incoming);

    let commands = core
        .hangup_active(false)
        .expect("reject active Rust incoming call");
    endpoint
        .execute_commands(&commands)
        .expect("send Rust reject signalling");
    assert!(core.snapshot().active_call.is_none());

    let local_identity_hash = hex::encode(local_identity.hash);
    if let Some(rejected) = helper.try_wait_event("REJECTED", Duration::from_secs(2)) {
        assert_eq!(
            rejected["identity_hash"].as_str(),
            Some(local_identity_hash.as_str())
        );
    } else {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let now = Instant::now();
            assert!(
                now < deadline,
                "Python helper did not observe rejected signalling before call ended"
            );
            let signals =
                helper.wait_event("SIGNALS", (deadline - now).min(Duration::from_secs(1)));
            let saw_rejected = signals["signals"]
                .as_array()
                .is_some_and(|signals| signals.iter().any(|signal| signal.as_u64() == Some(1)));
            if saw_rejected {
                break;
            }
        }
        let ended = helper.wait_event("ENDED", Duration::from_secs(5));
        assert_eq!(
            ended["identity_hash"].as_str(),
            Some(local_identity_hash.as_str())
        );
    }

    endpoint
        .deregister_destination()
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_outgoing_timeout_ends_python_ringing_call_without_audio() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let rust_destination_hash = endpoint.destination_hash;
    let (control_tx, control_rx) = mpsc::channel(16);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(20),
            incoming_ring_timeout: Some(Duration::from_secs(10)),
            outgoing_call_timeout: Some(Duration::from_millis(900)),
            media_frames_per_tick: 4,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());

    control_tx
        .send(TelephonyControl::Call {
            remote_identity: remote_identity_hash,
            profile: None,
            mode: None,
            discovery_timeout: Duration::from_secs(5),
        })
        .await
        .expect("send Rust Telephone service call control");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "outgoing call started",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::OutgoingCallStarted {
                    remote_identity,
                    ..
                } if *remote_identity == remote_identity_hash
            )
        },
    )
    .await;

    let ringing = helper.wait_event("RINGING", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ringing["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    let terminated = wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "outgoing timeout termination",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::CallTerminated { reason: None, .. }
            )
        },
    )
    .await;
    assert!(matches!(
        terminated,
        TelephonyServiceEvent::CallTerminated { reason: None, .. }
    ));

    let ended = helper.wait_event("ENDED", Duration::from_secs(5));
    assert_eq!(
        ended["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );

    control_tx
        .send(TelephonyControl::Shutdown)
        .await
        .expect("send Rust Telephone service shutdown");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(2),
        "service stopped",
        |event| matches!(event, TelephonyServiceEvent::Stopped),
    )
    .await;
    service_task.await.expect("join Rust Telephone service");
    actor_tx
        .send(TransportMessage::DeregisterDestination {
            hash: rust_destination_hash,
        })
        .await
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_incoming_timeout_ends_python_outgoing_call_without_reject() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let python_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    let local_identity = Identity::new();
    let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let rust_destination_hash = endpoint.destination_hash;
    let (control_tx, control_rx) = mpsc::channel(16);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(20),
            incoming_ring_timeout: Some(Duration::from_millis(900)),
            outgoing_call_timeout: Some(Duration::from_secs(10)),
            media_frames_per_tick: 4,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());
    let announced_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
    assert_eq!(announced_hash, rust_destination_hash);

    tokio::time::sleep(Duration::from_millis(500)).await;
    helper.send(json!({
        "cmd": "call",
        "id": "call-timeout-rust-1",
        "target_destination_hash": hex::encode(rust_destination_hash),
    }));
    let requested = helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
    assert_eq!(
        requested["target_destination_hash"],
        hex::encode(rust_destination_hash)
    );

    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "incoming call",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::IncomingCall {
                    remote_identity,
                    ..
                } if *remote_identity == python_identity_hash
            )
        },
    )
    .await;

    let terminated = wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "incoming timeout termination",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::CallTerminated { reason: None, .. }
            )
        },
    )
    .await;
    assert!(matches!(
        terminated,
        TelephonyServiceEvent::CallTerminated { reason: None, .. }
    ));

    let ended = helper.wait_event("ENDED", Duration::from_secs(5));
    let local_identity_hash = hex::encode(local_identity.hash);
    assert_eq!(
        ended["identity_hash"].as_str(),
        Some(local_identity_hash.as_str())
    );
    assert!(
        helper
            .try_wait_event("REJECTED", Duration::from_millis(100))
            .is_none(),
        "incoming ring timeout must close the link without rejected signalling"
    );

    control_tx
        .send(TelephonyControl::Shutdown)
        .await
        .expect("send Rust Telephone service shutdown");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(2),
        "service stopped",
        |event| matches!(event, TelephonyServiceEvent::Stopped),
    )
    .await;
    service_task.await.expect("join Rust Telephone service");
    actor_tx
        .send(TransportMessage::DeregisterDestination {
            hash: rust_destination_hash,
        })
        .await
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_sends_raw_media_to_python_established_call() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let rust_destination_hash = endpoint.destination_hash;
    let (control_tx, control_rx) = mpsc::channel(16);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(20),
            incoming_ring_timeout: Some(Duration::from_secs(10)),
            outgoing_call_timeout: Some(Duration::from_secs(10)),
            media_frames_per_tick: 4,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());

    control_tx
        .send(TelephonyControl::Call {
            remote_identity: remote_identity_hash,
            profile: None,
            mode: None,
            discovery_timeout: Duration::from_secs(5),
        })
        .await
        .expect("send Rust Telephone service call control");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "outgoing call started",
        |event| matches!(event, TelephonyServiceEvent::OutgoingCallStarted { .. }),
    )
    .await;

    helper.wait_event("RINGING", Duration::from_secs(5));
    helper.send(json!({"cmd": "answer", "id": "answer-media-1"}));
    let answered = helper.wait_event("ANSWERED", Duration::from_secs(5));
    assert_eq!(answered["accepted"], true);
    helper.wait_event("ESTABLISHED", Duration::from_secs(5));
    wait_caller_audio_started(&mut event_rx, Duration::from_secs(5)).await;
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "established snapshot",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| call.status == SignallingStatus::Established)
            )
        },
    )
    .await;

    control_tx
        .send(TelephonyControl::SendRawFrames {
            bit_depth: RawBitDepth::Float32,
            frames: vec![
                RawAudioFrame::new(1, vec![0.0, 0.25, -0.5]).unwrap(),
                RawAudioFrame::new(1, vec![1.0, -1.0]).unwrap(),
            ],
        })
        .await
        .expect("send Raw media frames through Rust Telephone service");
    let sent = wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "media sent",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::MediaSent {
                    frames: 2,
                    packets: 2,
                    ..
                }
            )
        },
    )
    .await;
    assert!(matches!(
        sent,
        TelephonyServiceEvent::MediaSent {
            frames: 2,
            packets: 2,
            ..
        }
    ));

    // Link.py runs each packet callback on its own thread, so the mixer may see either first.
    let mut frames = [
        helper.wait_event("MEDIA_FRAME", Duration::from_secs(5)),
        helper.wait_event("MEDIA_FRAME", Duration::from_secs(5)),
    ];
    frames.sort_by_key(|frame| std::cmp::Reverse(frame["total_samples"].as_u64()));
    assert_eq!(frames[0]["shape"], json!([3, 1]));
    assert_eq!(frames[0]["samples"], json!([0.0, 0.25, -0.5]));
    assert_eq!(frames[1]["shape"], json!([2, 1]));
    assert_eq!(frames[1]["samples"], json!([1.0, -1.0]));

    control_tx
        .send(TelephonyControl::Shutdown)
        .await
        .expect("send Rust Telephone service shutdown");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(2),
        "service stopped",
        |event| matches!(event, TelephonyServiceEvent::Stopped),
    )
    .await;
    service_task.await.expect("join Rust Telephone service");
    actor_tx
        .send(TransportMessage::DeregisterDestination {
            hash: rust_destination_hash,
        })
        .await
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_sends_opus_voice_profile_matrix_to_python_established_calls() {
    for profile in OPUS_VOICE_PROFILES {
        let Some(mut session) = RustCallerSession::establish(profile).await else {
            return;
        };
        let received = session.send_rust_opus_to_python().await;
        let expected_frames = python_headless_output_frames(profile);
        let expected_total_samples = expected_frames * PYTHON_HEADLESS_AUDIO_CHANNELS;

        assert_eq!(received["decoded"], true, "{}", profile.name());
        assert_eq!(
            received["shape"],
            json!([expected_frames, PYTHON_HEADLESS_AUDIO_CHANNELS]),
            "{}",
            profile.name()
        );
        assert_eq!(
            received["total_samples"],
            expected_total_samples,
            "{}",
            profile.name()
        );
        session.shutdown().await;
    }
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_profile_switch_reaches_python_and_media_uses_new_profile() {
    let Some(mut session) = RustCallerSession::establish(Profile::QualityMedium).await else {
        return;
    };
    session.switch_rust_profile(Profile::QualityHigh).await;

    let received = session.send_rust_opus_to_python().await;
    let expected_frames = python_headless_output_frames(Profile::QualityHigh);
    assert_eq!(received["decoded"], true);
    assert_eq!(
        received["shape"],
        json!([expected_frames, PYTHON_HEADLESS_AUDIO_CHANNELS])
    );

    session.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_profile_switch_reaches_rust_and_media_uses_new_profile() {
    let Some(mut session) = RustCallerSession::establish(Profile::QualityHigh).await else {
        return;
    };
    session.switch_python_profile(Profile::QualityMax).await;
    session.send_python_opus_to_rust().await;
    session.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_receives_python_raw_media_on_established_call() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;

    let mut helper = PythonTelephoneHost::spawn(port);
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");

    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .expect("timeout waiting for Python Telephone helper TCP connection")
        .expect("TCP accept channel closed");
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .expect("register accepted Python TCP interface");

    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    let announced = helper.wait_event("ANNOUNCED", Duration::from_secs(10));
    assert_eq!(
        announced["destination_hash"],
        hex::encode(remote_destination_hash)
    );
    let announce = tokio::time::timeout(Duration::from_secs(15), announce_rx.recv())
        .await
        .unwrap_or_else(|_| panic!("Rust transport never received Python announce"))
        .expect("announce handler channel closed");
    assert_eq!(announce.destination_hash, remote_destination_hash);

    let local_identity = Identity::new();
    let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity)
        .expect("register Rust LXST Telephone endpoint");
    let rust_destination_hash = endpoint.destination_hash;
    let (control_tx, control_rx) = mpsc::channel(16);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let service = TelephonyService::with_config(
        endpoint,
        TelephonyRuntimeCore::new(),
        control_rx,
        event_tx,
        TelephonyServiceConfig {
            poll_interval: Duration::from_millis(20),
            incoming_ring_timeout: Some(Duration::from_secs(10)),
            outgoing_call_timeout: Some(Duration::from_secs(10)),
            media_frames_per_tick: 4,
            ..TelephonyServiceConfig::default()
        },
    );
    let service_task = tokio::spawn(service.run());

    control_tx
        .send(TelephonyControl::Call {
            remote_identity: remote_identity_hash,
            profile: None,
            mode: None,
            discovery_timeout: Duration::from_secs(5),
        })
        .await
        .expect("send Rust Telephone service call control");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "outgoing call started",
        |event| matches!(event, TelephonyServiceEvent::OutgoingCallStarted { .. }),
    )
    .await;

    helper.wait_event("RINGING", Duration::from_secs(5));
    helper.send(json!({"cmd": "answer", "id": "answer-media-1"}));
    let answered = helper.wait_event("ANSWERED", Duration::from_secs(5));
    assert_eq!(answered["accepted"], true);
    helper.wait_event("ESTABLISHED", Duration::from_secs(5));
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "established snapshot",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| call.status == SignallingStatus::Established)
            )
        },
    )
    .await;

    helper.send(json!({
        "cmd": "send_raw_frame",
        "id": "py-raw-1",
        "channels": 2,
        "bitdepth": 32,
        "samples": [0.0, 0.5, -0.25, 1.0],
    }));
    let raw_sent = helper.wait_event("RAW_FRAME_SENT", Duration::from_secs(5));
    assert_eq!(raw_sent["sent"], true);

    let drive = wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "Rust media drive",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::Drive(step)
                    if step.step.inbound.as_ref().is_some_and(|inbound| {
                        inbound
                            .frame_events
                            .iter()
                            .any(|event| matches!(event, FrameStreamEvent::Frame(_)))
                    })
            )
        },
    )
    .await;
    let TelephonyServiceEvent::Drive(step) = drive else {
        panic!("expected Drive event with inbound media");
    };
    let inbound = step.step.inbound.expect("inbound LXST media packet");
    let raw_frames = inbound
        .frame_events
        .iter()
        .filter_map(|event| match event {
            FrameStreamEvent::Frame(frame) => Some(RawAudioFrame::from_frame(frame).unwrap()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        raw_frames,
        vec![RawAudioFrame::new(2, vec![0.0, 0.5, -0.25, 1.0]).unwrap()]
    );

    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(5),
        "media received",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::MediaReceived { frames: 1, .. }
            )
        },
    )
    .await;

    control_tx
        .send(TelephonyControl::Shutdown)
        .await
        .expect("send Rust Telephone service shutdown");
    wait_matching_service_event(
        &mut event_rx,
        Duration::from_secs(2),
        "service stopped",
        |event| matches!(event, TelephonyServiceEvent::Stopped),
    )
    .await;
    service_task.await.expect("join Rust Telephone service");
    actor_tx
        .send(TransportMessage::DeregisterDestination {
            hash: rust_destination_hash,
        })
        .await
        .expect("deregister Rust Telephone endpoint");
    accepted_read_task.abort();
    server.read_task.abort();
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_service_decodes_python_opus_voice_profile_matrix() {
    for profile in OPUS_VOICE_PROFILES {
        let Some(mut session) = RustCallerSession::establish(profile).await else {
            return;
        };
        session.send_python_opus_to_rust().await;
        session.shutdown().await;
    }
}

struct ServiceCallPair {
    helper: PythonTelephoneHost,
    actor_tx: mpsc::Sender<TransportMessage>,
    control_tx: mpsc::Sender<TelephonyControl>,
    events: mpsc::Receiver<TelephonyServiceEvent>,
    python_identity: [u8; 16],
    rust_destination: [u8; 16],
    service_task: tokio::task::JoinHandle<()>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl ServiceCallPair {
    async fn connect(env: &[(&str, &str)]) -> Self {
        Self::connect_with_config(env, TelephonyServiceConfig::default()).await
    }

    async fn connect_with_config(env: &[(&str, &str)], config: TelephonyServiceConfig) -> Self {
        let port = free_tcp_port();
        let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
        let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;
        let helper = PythonTelephoneHost::spawn_with_env(port, "client", false, env);
        let ready = helper.wait_event("READY", Duration::from_secs(10));
        assert_eq!(ready["lxst_version"], "0.6.0");
        let python_identity = hash_from_ready(&ready, "identity_hash");
        let python_destination = hash_from_ready(&ready, "destination_hash");
        let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
            .await
            .expect("Python helper TCP connection")
            .expect("TCP accept channel");
        let (id, entry, read_task) = register_interface_entry(accepted);
        actor_tx
            .send(TransportMessage::RegisterInterface { id, entry })
            .await
            .unwrap();

        let endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &Identity::new())
            .expect("register Rust Telephone endpoint");
        let rust_destination = endpoint.destination_hash;
        let (control_tx, control_rx) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(256);
        let service = TelephonyService::with_config(
            endpoint,
            TelephonyRuntimeCore::new(),
            control_rx,
            event_tx,
            TelephonyServiceConfig {
                poll_interval: Duration::from_millis(20),
                announce_on_start: false,
                announce_interval: None,
                ..config
            },
        );
        let service_task = tokio::spawn(service.run());
        control_tx.send(TelephonyControl::Announce).await.unwrap();
        helper.send_announce();
        wait_for_announce(
            &mut announce_rx,
            python_destination,
            Duration::from_secs(15),
            "Python Telephone",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        Self {
            helper,
            actor_tx,
            control_tx,
            events,
            python_identity,
            rust_destination,
            service_task,
            tasks: vec![read_task, server.read_task],
        }
    }

    async fn wait_event(
        &mut self,
        label: &str,
        timeout: Duration,
        matches: impl Fn(&TelephonyServiceEvent) -> bool,
    ) -> TelephonyServiceEvent {
        wait_matching_service_event(&mut self.events, timeout, label, matches).await
    }

    async fn wait_snapshot(
        &mut self,
        label: &str,
        matches: impl Fn(&ActiveCallSnapshot) -> bool,
    ) -> ActiveCallSnapshot {
        let event = self
            .wait_event(label, Duration::from_secs(10), |event| {
                matches!(event, TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(&matches))
            })
            .await;
        let TelephonyServiceEvent::Snapshot(snapshot) = event else {
            unreachable!()
        };
        snapshot.active_call.unwrap()
    }

    fn drain(&mut self) -> Vec<TelephonyServiceEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    async fn python_calls_rust_and_rust_answers(&mut self, mode: Option<u8>) -> [u8; 16] {
        self.helper.send(json!({
            "cmd": "call",
            "id": "call",
            "target_destination_hash": hex::encode(self.rust_destination),
            "mode": mode,
        }));
        self.helper
            .wait_event("CALL_REQUESTED", Duration::from_secs(20));
        let python_identity = self.python_identity;
        let incoming = self
            .wait_event("incoming call", Duration::from_secs(15), |event| {
                matches!(event, TelephonyServiceEvent::IncomingCall { remote_identity, .. }
                    if *remote_identity == python_identity)
            })
            .await;
        let TelephonyServiceEvent::IncomingCall { link_id, .. } = incoming else {
            unreachable!()
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        let answered = lxst_telephony::request_answer(&self.control_tx, link_id)
            .await
            .expect("Rust answer");
        assert_eq!(answered.status, SignallingStatus::Established);
        link_id
    }

    async fn rust_calls_python_and_python_answers(&mut self, mode: Option<CallMode>) {
        self.control_tx
            .send(TelephonyControl::Call {
                remote_identity: self.python_identity,
                profile: None,
                mode,
                discovery_timeout: Duration::from_secs(15),
            })
            .await
            .unwrap();
        self.helper.wait_event("RINGING", Duration::from_secs(20));
        // Answer only after the caller's profile and mode packet has reached Python.
        let mode_signal = 0xF0 + u64::from(mode.unwrap_or(CallMode::DEFAULT).wire_value());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let signals = self.helper.wait_event("SIGNALS", Duration::from_secs(10));
            if signals["signals"]
                .as_array()
                .is_some_and(|all| all.iter().any(|v| v.as_u64() == Some(mode_signal)))
            {
                break;
            }
            assert!(Instant::now() < deadline, "no mode signal reached Python");
        }
        self.helper.send(json!({"cmd": "answer", "id": "answer"}));
        let answered = self.helper.wait_event("ANSWERED", Duration::from_secs(5));
        assert_eq!(answered["accepted"], true);
        self.wait_snapshot("Rust caller established", |call| {
            call.status == SignallingStatus::Established
        })
        .await;
    }

    fn python_snapshot(&mut self) -> Value {
        self.helper.send(json!({"cmd": "snapshot", "id": "snap"}));
        self.helper.wait_event("SNAPSHOT", Duration::from_secs(5))
    }

    async fn shutdown(self) {
        let _ = self.control_tx.send(TelephonyControl::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.service_task).await;
        drop(self.actor_tx);
        for task in self.tasks {
            task.abort();
        }
        drop(self.helper);
    }
}

impl PythonTelephoneHost {
    fn send_announce(&self) {
        let mut stdin = &self.stdin;
        writeln!(stdin, "{}", json!({"cmd": "announce", "id": "announce"})).unwrap();
        stdin.flush().unwrap();
    }

    fn count_stashed(&self, name: &str, within: Duration) -> usize {
        let _ = self.try_wait_event("__none__", within);
        self.stashed_events
            .lock()
            .expect("event stash")
            .iter()
            .filter(|event| event["event"] == name)
            .count()
    }
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_caller_with_slow_pipelines_is_answered_once_by_rust_service() {
    let mut pair = ServiceCallPair::connect(&[("LXST_HELPER_OPEN_DELAY", "6.0")]).await;
    pair.python_calls_rust_and_rust_answers(None).await;
    // Python establishes once and the Rust callee does not give up while the caller is slow.
    let established = pair
        .helper
        .count_stashed("ESTABLISHED", Duration::from_secs(9));
    let opened = pair.helper.count_stashed("OPEN_PIPELINES", Duration::ZERO);
    let ended = pair.helper.count_stashed("ENDED", Duration::ZERO);
    let terminated = pair
        .drain()
        .iter()
        .any(|event| matches!(event, TelephonyServiceEvent::CallTerminated { .. }));
    assert_eq!((established, opened, ended, terminated), (1, 1, 0, false));
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_caller_is_answered_once_by_python_callee() {
    let mut pair = ServiceCallPair::connect(&[]).await;
    pair.rust_calls_python_and_python_answers(None).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    let terminated = pair
        .drain()
        .iter()
        .any(|event| matches!(event, TelephonyServiceEvent::CallTerminated { .. }));
    assert!(!terminated);
    assert_eq!(pair.helper.count_stashed("ENDED", Duration::ZERO), 0);
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_caller_keeps_call_when_python_callee_established_is_lost() {
    let wait = Duration::from_secs(10);
    let mut pair = ServiceCallPair::connect_with_config(
        &[("LXST_HELPER_DROP_SENT", "6")],
        TelephonyServiceConfig {
            outgoing_call_timeout: Some(wait),
            ..TelephonyServiceConfig::default()
        },
    )
    .await;
    pair.rust_calls_python_and_python_answers(None).await;
    tokio::time::sleep(wait + Duration::from_secs(2)).await;
    let terminated = pair
        .drain()
        .iter()
        .any(|event| matches!(event, TelephonyServiceEvent::CallTerminated { .. }));
    assert!(!terminated, "Rust caller ended the call at its wait time");
    assert_eq!(pair.helper.count_stashed("ENDED", Duration::ZERO), 0);
    let python = pair.python_snapshot();
    assert_eq!(python["active_call"]["status"], 6, "{python}");
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_caller_keeps_call_when_rust_callee_established_is_lost() {
    let wait = 8;
    let mut pair = ServiceCallPair::connect(&[
        ("LXST_HELPER_DROP_RECEIVED", "6"),
        ("LXST_HELPER_WAIT_TIME", "8"),
    ])
    .await;
    pair.python_calls_rust_and_rust_answers(None).await;
    tokio::time::sleep(Duration::from_secs(wait + 2)).await;
    assert_eq!(pair.helper.count_stashed("ENDED", Duration::ZERO), 0);
    let python = pair.python_snapshot();
    assert_eq!(python["active_call"]["status"], 6, "{python}");
    assert_eq!(python["active_call"]["audio_started"], false, "{python}");
    let terminated = pair
        .drain()
        .iter()
        .any(|event| matches!(event, TelephonyServiceEvent::CallTerminated { .. }));
    assert!(!terminated);
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_half_duplex_caller_sets_rust_callee_mode_and_squelch() {
    let mut pair = ServiceCallPair::connect(&[]).await;
    pair.python_calls_rust_and_rust_answers(Some(2)).await;
    let call = pair
        .wait_snapshot("Rust callee half duplex", |call| {
            call.status == SignallingStatus::Established
        })
        .await;
    assert_eq!(call.mode, Some(CallMode::HalfDuplex));
    assert!(call.transmit_squelched);
    pair.helper
        .wait_event("ESTABLISHED", Duration::from_secs(5));
    let python = pair.python_snapshot();
    assert_eq!(python["active_call"]["call_mode"], 2);
    assert_eq!(python["active_call"]["squelched"], true);

    // Mid-call switch from Python reaches Rust.
    pair.helper
        .send(json!({"cmd": "switch_mode", "id": "fdx", "mode": 1}));
    pair.helper
        .wait_event("MODE_SWITCHED", Duration::from_secs(5));
    let call = pair
        .wait_snapshot("Rust follows Python full duplex", |call| {
            call.mode == Some(CallMode::FullDuplex)
        })
        .await;
    assert!(!call.transmit_squelched);
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn rust_half_duplex_caller_sets_python_callee_mode_and_switch_reaches_python() {
    let mut pair = ServiceCallPair::connect(&[]).await;
    pair.rust_calls_python_and_python_answers(Some(CallMode::HalfDuplex))
        .await;
    let python = pair.python_snapshot();
    assert_eq!(python["active_call"]["call_mode"], 2, "{python}");
    assert_eq!(python["active_call"]["squelched"], true);

    pair.control_tx
        .send(TelephonyControl::SwitchMode {
            mode: CallMode::FullDuplex,
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let python = pair.python_snapshot();
        if python["active_call"]["call_mode"] == 1 {
            assert_eq!(python["active_call"]["squelched"], false);
            break;
        }
        assert!(Instant::now() < deadline, "Python did not follow: {python}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires opt-in live Python LXST/Reticulum TCP interop"]
async fn python_caller_gets_busy_while_rust_service_discovers_a_path() {
    let mut pair = ServiceCallPair::connect(&[]).await;
    pair.control_tx
        .send(TelephonyControl::Call {
            remote_identity: Identity::new().hash,
            profile: None,
            mode: None,
            discovery_timeout: Duration::from_secs(30),
        })
        .await
        .unwrap();
    pair.wait_event("pending", Duration::from_secs(5), |event| {
        matches!(event, TelephonyServiceEvent::OutgoingCallPending { .. })
    })
    .await;
    pair.helper.send(json!({
        "cmd": "call",
        "id": "call",
        "target_destination_hash": hex::encode(pair.rust_destination),
    }));
    // Python may report BUSY or, if the link close wins the race, ENDED.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut signals = Vec::new();
    loop {
        if let Some(event) = pair
            .helper
            .try_wait_event("SIGNALS", Duration::from_millis(200))
        {
            signals.push(event["signals"].clone());
        }
        let ended = pair
            .helper
            .stashed_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event["event"] == "BUSY" || event["event"] == "ENDED");
        if ended {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Python call did not end: {signals:?}"
        );
    }
    while let Some(event) = pair.helper.try_wait_event("SIGNALS", Duration::ZERO) {
        signals.push(event["signals"].clone());
    }
    let flat = signals
        .iter()
        .flat_map(|list| list.as_array().cloned().unwrap_or_default())
        .filter_map(|value| value.as_u64())
        .collect::<Vec<_>>();
    assert!(flat.contains(&0), "Python never received BUSY: {flat:?}");
    assert!(!flat.contains(&4), "Python received RINGING: {flat:?}");
    let rang = pair
        .drain()
        .iter()
        .any(|event| matches!(event, TelephonyServiceEvent::IncomingCall { .. }));
    assert!(!rang);
    pair.shutdown().await;
}

// Link request stamps under enforcement (Link.py 1.6.0).
// LXST 0.6.0 has no stamp code of its own; its Telephone destination keeps the
// default link cost LR_COST = 12.5.

const PY_ENFORCING_STAMPS: &str =
    "stamp_destinations = yes;enforce_stamps = yes;destination_cost = 6;destination_value = 8";

fn low_cost_policy() -> rns_transport::stamps::StampPolicy {
    rns_transport::stamps::StampPolicy {
        destination_cost: 6.0,
        ..Default::default()
    }
}

/// A Rust caller reaches an enforcing Python Telephone: it recalls the
/// announced link cost and stamps its link request at it.
#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_outgoing_call_reaches_an_enforcing_python_telephone() {
    let port = free_tcp_port();
    let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
    actor_tx
        .send(TransportMessage::SetStampPolicy {
            policy: low_cost_policy(),
        })
        .await
        .unwrap();
    let mut announce_rx = register_telephony_announce_handler(&actor_tx).await;
    let mut helper = PythonTelephoneHost::spawn_with_env(
        port,
        "client",
        false,
        &[("LXST_HELPER_STAMPS", PY_ENFORCING_STAMPS)],
    );
    let ready = helper.wait_event("READY", Duration::from_secs(10));
    let remote_identity_hash = hash_from_ready(&ready, "identity_hash");
    let remote_destination_hash = hash_from_ready(&ready, "destination_hash");
    let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
    actor_tx
        .send(TransportMessage::RegisterInterface {
            id: accepted_id,
            entry: accepted_entry,
        })
        .await
        .unwrap();
    helper.send(json!({"cmd": "announce", "id": "announce-1"}));
    helper.wait_event("ANNOUNCED", Duration::from_secs(20));
    tokio::time::timeout(Duration::from_secs(20), announce_rx.recv())
        .await
        .expect("Python announce")
        .unwrap();
    let recalled =
        rns_runtime::link_stamp::recall_link_cost(&actor_tx, remote_destination_hash).await;
    assert_eq!(recalled, Some(12.5), "announced link cost");

    let local_identity = Identity::new();
    let mut endpoint = TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity).unwrap();
    let mut core = TelephonyRuntimeCore::new();
    endpoint
        .begin_outgoing_link(
            &mut core,
            remote_identity_hash,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("start stamped outgoing call");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        endpoint.try_drive_ready(&mut core).unwrap();
        if core
            .snapshot()
            .active_call
            .is_some_and(|call| call.status == SignallingStatus::Ringing)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Rust call did not reach the enforcing Python Telephone:\n{}",
            helper.stderr_log()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    helper.wait_event("RINGING", Duration::from_secs(5));
    endpoint.deregister_destination().unwrap();
    accepted_read_task.abort();
    server.read_task.abort();
}

/// An enforcing Rust telephone answers a Python caller that stamps at its
/// cost (`lr_cost`, via `always_stamp_lrs` since the Rust announce here
/// carries no cost) and drops one that does not stamp.
#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_outgoing_call_reaches_an_enforcing_rust_telephone_only_when_stamped() {
    // Below 10 upstream grinds in-process; its worker pool deadlocks in this helper.
    for (rust_lr_cost, python_stamps, expect_ringing) in [
        (8.0, "always_stamp_lrs = yes;lr_cost = 8", true),
        (12.5, "", false),
    ] {
        let mut policy = rns_transport::stamps::StampPolicy {
            lr_cost: rust_lr_cost,
            ..Default::default()
        };
        policy.enforce();
        rns_runtime::destination_stamper::configure(policy, None);
        let port = free_tcp_port();
        let (actor_tx, server, mut handle_rx) = spawn_rust_actor_and_tcp(port).await;
        let mut helper = PythonTelephoneHost::spawn_with_env(
            port,
            "client",
            false,
            &[("LXST_HELPER_STAMPS", python_stamps)],
        );
        let ready = helper.wait_event("READY", Duration::from_secs(10));
        let python_identity_hash = hash_from_ready(&ready, "identity_hash");
        let accepted = tokio::time::timeout(Duration::from_secs(15), handle_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let (accepted_id, accepted_entry, accepted_read_task) = register_interface_entry(accepted);
        actor_tx
            .send(TransportMessage::RegisterInterface {
                id: accepted_id,
                entry: accepted_entry,
            })
            .await
            .unwrap();
        let local_identity = Identity::new();
        let mut endpoint =
            TelephonyRnsEndpoint::register(actor_tx.clone(), &local_identity).unwrap();
        let mut core = TelephonyRuntimeCore::new();
        let rust_destination_hash = send_rust_telephony_announce(&actor_tx, &local_identity).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        helper.send(json!({
            "cmd": "call",
            "id": "call-rust-1",
            "target_destination_hash": hex::encode(rust_destination_hash),
        }));
        helper.wait_event("CALL_REQUESTED", Duration::from_secs(10));
        let wait = if expect_ringing { 40 } else { 15 };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
        let rang = loop {
            endpoint.try_drive_ready(&mut core).unwrap();
            if core
                .snapshot()
                .active_call
                .is_some_and(|call| call.status == SignallingStatus::Ringing)
            {
                break true;
            }
            if tokio::time::Instant::now() >= deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        rns_runtime::destination_stamper::configure(Default::default(), None);
        assert_eq!(
            rang,
            expect_ringing,
            "Python stamps {python_stamps:?} against Rust cost {rust_lr_cost}:\n{}",
            helper.stderr_log()
        );
        if rang {
            assert_eq!(
                core.snapshot().active_call.unwrap().remote_identity,
                python_identity_hash
            );
        }
        endpoint.deregister_destination().unwrap();
        accepted_read_task.abort();
        server.read_task.abort();
    }
}

// Calls across a relay of the other kind: two upstream Telephones through an
// `rnsd-rs` transport, and two Rust Telephones through an upstream transport.

const VOICE_MEDIA_FRAMES: usize = 34;
const SWITCH_MEDIA_FRAMES: usize = 10;
const CALL_SEQUENCE_EVENTS: [&str; 4] = ["SIGNALS", "RINGING", "ESTABLISHED", "ENDED"];

fn relay_helper_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/python_telephone_relay_helper.py")
}

fn rnsd_rs_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("RUST_RNSD_BIN") {
        return PathBuf::from(path);
    }
    let exe = format!("rnsd-rs{}", std::env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        let target = PathBuf::from(target);
        candidates.push(target.join("debug").join(&exe));
        candidates.push(target.join("release").join(&exe));
    }
    let rs_reticulum = repo_root().join("../rsReticulum/target");
    candidates.push(rs_reticulum.join("debug").join(&exe));
    candidates.push(rs_reticulum.join("release").join(&exe));
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "rnsd-rs not found: build it with `cargo build -p rns-tools --bin rnsd-rs` in \
                 rsReticulum, or set RUST_RNSD_BIN"
            )
        })
}

/// An `rnsd-rs` transport node with one TCP server interface.
struct RustRelay {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl RustRelay {
    fn spawn() -> Self {
        let dir = temp_storage("rs-lxst-rnsd-rs-relay");
        std::fs::create_dir_all(&dir).expect("create rnsd-rs config dir");
        let port = free_tcp_port();
        std::fs::write(
            dir.join("config"),
            format!(
                "[reticulum]\n  enable_transport = yes\n  share_instance = no\n\n\
                 [logging]\n  loglevel = 4\n\n[interfaces]\n  [[Relay]]\n    \
                 type = TCPServerInterface\n    enabled = yes\n    listen_ip = 127.0.0.1\n    \
                 listen_port = {port}\n"
            ),
        )
        .expect("write rnsd-rs config");
        let log = std::fs::File::create(dir.join("rnsd.log")).expect("create rnsd-rs log");
        let child = Command::new(rnsd_rs_binary())
            .arg("--config")
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().expect("clone rnsd-rs log")))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("spawn rnsd-rs");
        let relay = Self { child, dir, port };
        let deadline = Instant::now() + Duration::from_secs(20);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                Instant::now() < deadline,
                "rnsd-rs never listened on {port}:\n{}",
                relay.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        relay
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("rnsd.log")).unwrap_or_default()
    }
}

impl Drop for RustRelay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl PythonTelephoneHost {
    /// Every event read so far, after reading whatever arrives within `settle`.
    fn history_after(&self, settle: Duration) -> Vec<Value> {
        let _ = self.try_wait_event("__history__", settle);
        self.history.lock().expect("event history").clone()
    }

    fn history_len(&self) -> usize {
        self.history_after(Duration::ZERO).len()
    }

    /// The codec header byte, channels and frame time upstream's `Profiles` gives `profile`.
    fn profile_codec(&mut self, profile: Profile) -> UpstreamProfileCodec {
        self.send(json!({"cmd": "profile_codec", "profile": profile.wire_value()}));
        let event = self.wait_event("PROFILE_CODEC", Duration::from_secs(10));
        let codec = UpstreamProfileCodec {
            codec: event["codec"].as_u64().expect("codec header byte"),
            stereo: event["channels"].as_u64().expect("channels") == 2,
        };
        assert_eq!(event["frame_ms"], profile.frame_time_ms());
        codec
    }

    fn wait_active_profile(&mut self, profile: Profile) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.send(json!({"cmd": "snapshot"}));
            let snapshot = self.wait_event("SNAPSHOT", Duration::from_secs(5));
            if snapshot["active_profile"].as_u64() == Some(u64::from(profile.wire_value())) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Python Telephone never reported profile {}: {snapshot}",
                profile.name()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

struct UpstreamProfileCodec {
    codec: u64,
    stereo: bool,
}

/// The call events one Telephone records, in order: received signals and callbacks.
fn call_sequence(history: &[Value]) -> Vec<String> {
    history
        .iter()
        .filter_map(|event| {
            let name = event["event"].as_str()?;
            if !CALL_SEQUENCE_EVENTS.contains(&name) {
                return None;
            }
            Some(if name == "SIGNALS" {
                format!("SIGNALS {}", event["signals"])
            } else {
                name.to_string()
            })
        })
        .collect()
}

/// Two upstream Telephones, either linked directly or both clients of an `rnsd-rs` relay.
struct PythonCallPair {
    caller: PythonTelephoneHost,
    callee: PythonTelephoneHost,
    callee_destination: String,
    relay: Option<RustRelay>,
}

/// One direction of a media run between upstream Telephones.
struct PythonMediaRun {
    sent: Vec<Value>,
    received: Vec<Value>,
    decoded: Vec<Value>,
}

impl PythonCallPair {
    fn connect(through_rust_relay: bool) -> Self {
        let script = relay_helper_script();
        let (relay, port, callee_role) = if through_rust_relay {
            let relay = RustRelay::spawn();
            let port = relay.port;
            (Some(relay), port, "client")
        } else {
            (None, free_tcp_port(), "server")
        };
        let mut callee = PythonTelephoneHost::spawn_script(&script, port, callee_role, false, &[]);
        let callee_ready = callee.wait_event("READY", Duration::from_secs(20));
        let caller = PythonTelephoneHost::spawn_script(&script, port, "client", false, &[]);
        let caller_ready = caller.wait_event("READY", Duration::from_secs(20));
        for ready in [&callee_ready, &caller_ready] {
            assert_eq!(ready["lxst_version"], "0.6.0");
            assert_eq!(ready["rns_version"], "1.6.1");
            assert_eq!(ready["opus_available"], true, "live voice media needs Opus");
        }
        let callee_destination = callee_ready["destination_hash"]
            .as_str()
            .expect("callee destination")
            .to_string();
        callee.send(json!({"cmd": "announce"}));
        callee.wait_event("ANNOUNCED", Duration::from_secs(10));
        let mut pair = Self {
            caller,
            callee,
            callee_destination,
            relay,
        };
        pair.caller.send(json!({
            "cmd": "hops",
            "destination_hash": pair.callee_destination,
            "timeout": 15,
        }));
        let hops = pair.caller.wait_event("HOPS", Duration::from_secs(20));
        assert_eq!(
            hops["has_path"], true,
            "caller has no path to the callee: {hops}"
        );
        let expected_hops = if through_rust_relay { 2 } else { 1 };
        assert_eq!(
            hops["hops"],
            expected_hops,
            "hops from caller to callee{}",
            pair.relay_log()
        );
        pair
    }

    fn relay_log(&self) -> String {
        self.relay
            .as_ref()
            .map(|relay| format!("\nrnsd-rs log:\n{}", relay.log()))
            .unwrap_or_default()
    }

    fn establish(&mut self, profile: Profile) {
        self.caller.send(json!({
            "cmd": "call",
            "target_destination_hash": self.callee_destination,
            "profile": profile.wire_value(),
        }));
        self.caller
            .wait_event("CALL_REQUESTED", Duration::from_secs(20));
        self.callee.wait_event("RINGING", Duration::from_secs(15));
        // The callee selects the caller's profile from the signal sent on RINGING.
        let profile_signal = python_preferred_profile_signal(profile);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let signals = self.callee.wait_event("SIGNALS", Duration::from_secs(10));
            if signals["signals"]
                .as_array()
                .is_some_and(|all| all.iter().any(|v| v.as_u64() == Some(profile_signal)))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no profile signal reached the callee"
            );
        }
        self.callee.send(json!({"cmd": "answer"}));
        let answered = self.callee.wait_event("ANSWERED", Duration::from_secs(5));
        assert_eq!(answered["accepted"], true);
        self.callee
            .wait_event("ESTABLISHED", Duration::from_secs(10));
        self.caller
            .wait_event("ESTABLISHED", Duration::from_secs(10));
        self.caller.wait_active_profile(profile);
        self.callee.wait_active_profile(profile);
    }

    fn hang_up(&mut self) {
        self.caller.send(json!({"cmd": "hangup"}));
        self.caller.wait_event("ENDED", Duration::from_secs(10));
        self.callee.wait_event("ENDED", Duration::from_secs(15));
    }

    /// Both Telephones send `frames` paced call frames at once; returns
    /// caller-to-callee and callee-to-caller.
    fn exchange_media(&mut self, frames: usize) -> (PythonMediaRun, PythonMediaRun) {
        let caller_start = self.caller.history_len();
        let callee_start = self.callee.history_len();
        self.caller
            .send(json!({"cmd": "send_call_frames", "count": frames}));
        self.callee
            .send(json!({"cmd": "send_call_frames", "count": frames}));
        let limit = Duration::from_secs(10) + Duration::from_millis(60 * frames as u64);
        for host in [&self.caller, &self.callee] {
            let done = host.wait_event("CALL_FRAMES_DONE", limit);
            assert_eq!(done["sent"], frames, "{done}");
        }
        let caller = self.caller.history_after(Duration::from_secs(1))[caller_start..].to_vec();
        let callee = self.callee.history_after(Duration::from_secs(1))[callee_start..].to_vec();
        let pick = |history: &[Value], name: &str| -> Vec<Value> {
            history
                .iter()
                .filter(|event| event["event"] == name)
                .cloned()
                .collect()
        };
        let received = |history: &[Value]| -> Vec<Value> {
            pick(history, "FRAME_RECEIVED")
                .into_iter()
                .flat_map(|event| event["frames"].as_array().cloned().unwrap_or_default())
                .collect()
        };
        (
            PythonMediaRun {
                sent: pick(&caller, "FRAME_SENT"),
                received: received(&callee),
                decoded: pick(&callee, "MEDIA_FRAME"),
            },
            PythonMediaRun {
                sent: pick(&callee, "FRAME_SENT"),
                received: received(&caller),
                decoded: pick(&caller, "MEDIA_FRAME"),
            },
        )
    }

    fn assert_media(&mut self, frames: usize, profile: Profile) {
        let expected = self.caller.profile_codec(profile);
        let (forward, backward) = self.exchange_media(frames);
        for (label, run) in [
            ("caller to callee", forward),
            ("callee to caller", backward),
        ] {
            assert_eq!(run.sent.len(), frames, "{label}: frames sent");
            // Upstream hands each link packet to its own thread, so arrival order is not kept.
            let strip = |frames: &[Value]| {
                let mut keys = frames
                    .iter()
                    .map(|frame| {
                        json!([frame["codec"], frame["stereo"], frame["digest"]]).to_string()
                    })
                    .collect::<Vec<_>>();
                keys.sort();
                keys
            };
            assert_eq!(
                strip(&run.received),
                strip(&run.sent),
                "{label}: frames received differ from frames sent{}",
                self.relay_log()
            );
            for frame in &run.sent {
                assert_eq!(frame["codec"], expected.codec, "{label}: {frame}");
                assert_eq!(frame["stereo"], expected.stereo, "{label}: {frame}");
            }
            assert_eq!(run.decoded.len(), frames, "{label}: frames decoded");
            let output_frames = python_headless_output_frames(profile);
            for decoded in &run.decoded {
                assert_eq!(decoded["decoded"], true);
                assert_eq!(decoded["shape"][0], output_frames, "{label}: {decoded}");
            }
        }
    }

    fn switch_profile(&mut self, profile: Profile) {
        self.caller
            .send(json!({"cmd": "switch_profile", "profile": profile.wire_value()}));
        let switched = self
            .caller
            .wait_event("PROFILE_SWITCHED", Duration::from_secs(5));
        assert_eq!(switched["profile"], profile.wire_value());
        self.callee.wait_active_profile(profile);
    }
}

#[serial_test::serial(python_lxst_live)]
#[test]
fn python_call_through_rust_relay_signals_as_a_direct_python_call() {
    let mut sequences = Vec::new();
    for through_rust_relay in [false, true] {
        let mut pair = PythonCallPair::connect(through_rust_relay);
        pair.establish(Profile::QualityMedium);
        pair.hang_up();
        let settle = Duration::from_millis(500);
        sequences.push((
            call_sequence(&pair.caller.history_after(settle)),
            call_sequence(&pair.callee.history_after(settle)),
        ));
    }
    let (direct, relayed) = (&sequences[0], &sequences[1]);
    assert!(
        direct.0.iter().any(|event| event == "ESTABLISHED")
            && direct.1.iter().any(|event| event == "RINGING"),
        "direct reference call is incomplete: {direct:?}"
    );
    assert_eq!(relayed.0, direct.0, "caller sequence through rnsd-rs");
    assert_eq!(relayed.1, direct.1, "callee sequence through rnsd-rs");
}

#[serial_test::serial(python_lxst_live)]
#[test]
fn python_voice_media_crosses_rust_relay_both_ways() {
    let mut pair = PythonCallPair::connect(true);
    pair.establish(Profile::QualityMedium);
    pair.assert_media(VOICE_MEDIA_FRAMES, Profile::QualityMedium);
    pair.hang_up();
}

#[serial_test::serial(python_lxst_live)]
#[test]
fn python_profile_switches_cross_rust_relay() {
    let mut pair = PythonCallPair::connect(true);
    pair.establish(Profile::QualityHigh);
    for profile in [Profile::QualityMedium, Profile::QualityMax] {
        pair.switch_profile(profile);
        pair.assert_media(SWITCH_MEDIA_FRAMES, profile);
    }
    pair.hang_up();
}

/// Two Rust Telephones, both clients of an upstream transport.
struct RustCallPair {
    hub: PythonTelephoneHost,
    caller: RustHubNode,
    callee: RustHubNode,
}

#[derive(Default)]
struct RustMediaTally {
    sent: usize,
    received: Vec<(u8, Option<u8>)>,
    decoded: usize,
}

impl RustCallPair {
    async fn connect() -> Self {
        let port = free_tcp_port();
        let hub =
            PythonTelephoneHost::spawn_script(&relay_helper_script(), port, "server", true, &[]);
        let ready = hub.wait_event("READY", Duration::from_secs(20));
        assert_eq!(ready["rns_version"], "1.6.1");
        assert_eq!(
            ready["opus_available"], true,
            "upstream TOC vectors need Opus"
        );
        let caller = RustHubNode::spawn("rust-relay-caller", port, 21_001).await;
        let mut caller_announces = register_telephony_announce_handler(&caller.actor_tx).await;
        let callee = RustHubNode::spawn("rust-relay-callee", port, 22_001).await;
        // One start-up announce can be missed while the hub spawns the callee's interface.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match tokio::time::timeout(Duration::from_secs(3), caller_announces.recv()).await {
                Ok(Some(event)) if event.destination_hash == callee.destination_hash => break,
                Ok(Some(_)) => continue,
                Ok(None) => panic!("announce handler channel closed"),
                Err(_) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "callee announce never crossed the upstream transport:\n{}",
                        hub.stderr_log()
                    );
                    callee
                        .control_tx
                        .send(TelephonyControl::Announce)
                        .await
                        .expect("re-announce callee");
                }
            }
        }
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        caller
            .actor_tx
            .send(TransportMessage::Rpc {
                query: TransportQuery::HopsTo {
                    dest: callee.destination_hash,
                },
                response_tx,
            })
            .await
            .expect("query hops");
        let hops = tokio::time::timeout(Duration::from_secs(5), response_rx)
            .await
            .expect("hops reply")
            .expect("hops reply channel");
        assert!(
            matches!(hops, TransportQueryResponse::IntResult(2)),
            "the call must cross the upstream transport: {hops:?}"
        );
        Self {
            hub,
            caller,
            callee,
        }
    }

    async fn establish(&mut self, profile: Profile) {
        self.caller
            .control_tx
            .send(TelephonyControl::Call {
                remote_identity: self.callee.identity.hash,
                profile: Some(profile),
                mode: None,
                discovery_timeout: Duration::from_secs(10),
            })
            .await
            .expect("send caller Call control");
        let incoming = wait_matching_service_event(
            &mut self.callee.event_rx,
            Duration::from_secs(15),
            "incoming call through upstream transport",
            |event| matches!(event, TelephonyServiceEvent::IncomingCall { .. }),
        )
        .await;
        let TelephonyServiceEvent::IncomingCall { link_id, .. } = incoming else {
            unreachable!()
        };
        // Answer once the caller's profile has reached the callee.
        wait_matching_service_event(
            &mut self.callee.event_rx,
            Duration::from_secs(10),
            "caller profile at callee",
            |event| {
                matches!(event, TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| call.profile == Some(profile)))
            },
        )
        .await;
        lxst_telephony::request_answer(&self.callee.control_tx, link_id)
            .await
            .expect("answer callee call");
        wait_caller_audio_started(&mut self.caller.event_rx, Duration::from_secs(15)).await;
        self.wait_both_profile(profile).await;
    }

    async fn wait_both_profile(&mut self, profile: Profile) {
        let caller_identity = self.caller.identity.hash;
        let callee_identity = self.callee.identity.hash;
        for (node, role, remote) in [
            (&mut self.caller, CallRole::Outgoing, callee_identity),
            (&mut self.callee, CallRole::Incoming, caller_identity),
        ] {
            wait_matching_service_event(
                &mut node.event_rx,
                Duration::from_secs(15),
                "established in profile",
                |event| {
                    matches!(event, TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| {
                        call.status == SignallingStatus::Established
                            && call.profile == Some(profile)
                            && call.role == role
                            && call.remote_identity == remote
                    }))
                },
            )
            .await;
        }
    }

    async fn switch_profile(&mut self, profile: Profile) {
        self.caller
            .control_tx
            .send(TelephonyControl::SwitchProfile { profile })
            .await
            .expect("send profile switch");
        self.wait_both_profile(profile).await;
    }

    async fn assert_media(&mut self, frames: usize, profile: Profile) {
        let expected = self.hub.profile_codec(profile);
        for node in [&self.caller, &self.callee] {
            let (frame_tx, frame_rx) = mpsc::channel(frames);
            node.control_tx
                .send(TelephonyControl::StartOpusStream {
                    profile,
                    frames: frame_rx,
                })
                .await
                .expect("start Opus stream");
            tokio::spawn(async move {
                let mut source = SyntheticSource::new(
                    profile.channels(),
                    profile.sample_rate_hz(),
                    profile.sample_frames_per_packet(),
                    SyntheticSourceKind::Sine {
                        frequency_hz: 440.0,
                        amplitude: 0.25,
                    },
                )
                .unwrap();
                let mut tick = tokio::time::interval(Duration::from_millis(u64::from(
                    profile.frame_time_ms(),
                )));
                for _ in 0..frames {
                    tick.tick().await;
                    if frame_tx
                        .send(source.next_raw_frame().unwrap())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(10)
            + Duration::from_millis(u64::from(profile.frame_time_ms()) * frames as u64);
        let (caller, callee) = tokio::join!(
            collect_rust_media(&mut self.caller.event_rx, frames, profile, deadline),
            collect_rust_media(&mut self.callee.event_rx, frames, profile, deadline),
        );
        for (label, sender, receiver) in [
            ("caller to callee", &caller, &callee),
            ("callee to caller", &callee, &caller),
        ] {
            assert_eq!(sender.sent, frames, "{label}: frames sent");
            assert_eq!(receiver.received.len(), frames, "{label}: frames received");
            assert_eq!(receiver.decoded, frames, "{label}: frames decoded");
            for (codec, toc) in &receiver.received {
                assert_eq!(
                    u64::from(*codec),
                    expected.codec,
                    "{label}: codec header byte"
                );
                assert_eq!(
                    toc.map(|toc| toc & 0x04 != 0),
                    Some(expected.stereo),
                    "{label}: Opus stereo flag in {}",
                    profile.name()
                );
            }
        }
    }

    async fn shutdown(self) {
        self.caller.shutdown().await;
        self.callee.shutdown().await;
        drop(self.hub);
    }
}

async fn collect_rust_media(
    event_rx: &mut mpsc::Receiver<TelephonyServiceEvent>,
    frames: usize,
    profile: Profile,
    deadline: tokio::time::Instant,
) -> RustMediaTally {
    let mut tally = RustMediaTally::default();
    while tally.sent < frames || tally.decoded < frames {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, event_rx.recv()).await else {
            break;
        };
        match event {
            TelephonyServiceEvent::MediaSent { frames, .. } => tally.sent += frames,
            TelephonyServiceEvent::Drive(step) => {
                if let Some(inbound) = step.step.inbound {
                    for frame_event in inbound.frame_events {
                        if let FrameStreamEvent::Frame(frame) = frame_event {
                            tally
                                .received
                                .push((frame.codec.wire_id(), frame.payload.first().copied()));
                        }
                    }
                }
            }
            TelephonyServiceEvent::OpusFramesReceived {
                profile: decoded_profile,
                frames: decoded,
                ..
            } => {
                assert_eq!(decoded_profile, profile, "decoded in the active profile");
                for frame in &decoded {
                    assert_eq!(frame.channels, profile.channels());
                    assert_eq!(frame.sample_frames(), profile.sample_frames_per_packet());
                }
                tally.decoded += decoded.len();
            }
            TelephonyServiceEvent::Error { message } => panic!("Rust Telephone error: {message}"),
            _ => {}
        }
    }
    tally
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_voice_media_crosses_python_relay_both_ways() {
    let mut pair = RustCallPair::connect().await;
    pair.establish(Profile::QualityMedium).await;
    pair.assert_media(VOICE_MEDIA_FRAMES, Profile::QualityMedium)
        .await;
    pair.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_profile_switches_cross_python_relay() {
    let mut pair = RustCallPair::connect().await;
    pair.establish(Profile::QualityHigh).await;
    for profile in [Profile::QualityMedium, Profile::QualityMax] {
        pair.switch_profile(profile).await;
        pair.assert_media(SWITCH_MEDIA_FRAMES, profile).await;
    }
    pair.shutdown().await;
}
