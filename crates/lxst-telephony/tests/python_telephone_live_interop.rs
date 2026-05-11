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
use rns_transport::constants::{ANNOUNCE_CAP, InterfaceDirection as TransportInterfaceDirection};
use rns_transport::ingress::IngressController;
use rns_transport::messages::{
    AnnounceHandlerEvent, InterfaceEntry, InterfaceRole, OutboundRequest, TransportMessage,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use lxst_core::{
    CallRole, FrameStreamEvent, Profile, RawAudioFrame, RawBitDepth, SignallingStatus,
    SyntheticSource, SyntheticSourceKind, TELEPHONY_DESTINATION_NAME,
};
use lxst_telephony::{
    ActiveCallSnapshot, TelephonyCommand, TelephonyControl, TelephonyDriveStep,
    TelephonyRnsEndpoint, TelephonyRuntimeCore, TelephonyService, TelephonyServiceConfig,
    TelephonyServiceEvent, telephony_inbound_destination,
};

const SKIP_ENV: &str = "SKIP_PYTHON_LXST_INTEROP";
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

fn should_skip() -> bool {
    std::env::var(SKIP_ENV).map(|v| v == "1").unwrap_or(false)
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
        let storage = temp_storage("rs-lxst-python-telephone-live");
        std::fs::create_dir_all(&storage).expect("create Python helper storage");
        let log_path = storage.join("helper.stderr.log");
        let log_file = std::fs::File::create(&log_path).expect("create helper stderr log");

        let mut command = Command::new("python3");
        command
            .arg(helper_script())
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
                Ok(event) if event["event"] == name => return Some(event),
                Ok(event) if event["event"] == "FATAL" || event["event"] == "ERROR" => {
                    panic!("Python helper emitted failure event: {event}")
                }
                Ok(event) => self.stashed_events.lock().expect("event stash").push(event),
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
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
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
    let (actor, actor_tx) = TransportActor::new();
    tokio::spawn(actor.run());

    let (handle_tx, handle_rx) = mpsc::channel(8);
    let id_gen = Arc::new(AtomicU64::new(9_000));
    let server_id = id_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let cfg = TcpServerConfig::new("rs-lxst-python-telephone-live", "127.0.0.1", port);
    let server = spawn_tcp_server(cfg, server_id, id_gen, actor_tx.clone(), handle_tx)
        .await
        .expect("spawn Rust TCP server");

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
    let (actor, actor_tx) = TransportActor::new();
    tokio::spawn(actor.run());
    actor_tx
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

    let handle = spawn_tcp_client(cfg, interface_id, actor_tx.clone(), None)
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
    let entry = InterfaceEntry {
        name: handle.name,
        mode,
        role: InterfaceRole::Normal,
        direction: TransportInterfaceDirection {
            inbound: handle.direction.inbound,
            outbound: handle.direction.outbound,
        },
        bitrate: handle.bitrate,
        mtu: handle.mtu,
        tx: handle.tx,
        ifac_key: None,
        ifac_size: 0,
        announce_cap: ANNOUNCE_CAP,
        announce_allowed_at: 0.0,
        announce_rate_target: None,
        announce_rate_grace: None,
        announce_rate_penalty: None,
        online: Some(handle.online),
        rxb: handle.rxb,
        txb: handle.txb,
        tx_drops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        ingress: IngressController::new(),
        announce_queue: Vec::new(),
    };
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
        let (actor_tx, read_task) = spawn_rust_actor_and_tcp_client(name, port, interface_id).await;
        Self::spawn_with_actor(
            actor_tx,
            Some(read_task),
            TelephonyServiceConfig {
                poll_interval: Duration::from_millis(20),
                incoming_ring_timeout: Some(Duration::from_secs(15)),
                outgoing_call_timeout: Some(Duration::from_secs(15)),
                media_frames_per_tick: 4,
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

        if let Some(call) = core.snapshot().active_call {
            if call.status == status {
                return call;
            }
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
async fn python_telephone_announce_reaches_rust_transport() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python LXST Telephone announce interop");
        return;
    }

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
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust/Rust Python transport-hub interop");
        return;
    }

    let port = free_tcp_port();
    let hub = PythonTelephoneHost::spawn_transport_hub(port);
    let ready = hub.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);

    let mut caller = RustHubNode::spawn("rust-hub-caller", port, 11_001).await;
    let mut caller_announces = register_telephony_announce_handler(&caller.actor_tx).await;
    let mut callee = RustHubNode::spawn("rust-hub-callee", port, 12_001).await;

    let announce = wait_for_announce(
        &mut caller_announces,
        callee.destination_hash,
        Duration::from_secs(20),
        "callee through Python hub",
    )
    .await;
    assert_eq!(announce.name_hash, name_hash(TELEPHONY_DESTINATION_NAME));
    assert!(
        announce.public_key.is_some(),
        "Rust callee announce through Python hub did not include a public key"
    );
    assert!(
        announce.hops >= 1,
        "callee announce should arrive through at least one transport hop"
    );

    caller
        .control_tx
        .send(TelephonyControl::Call {
            remote_identity: callee.identity.hash,
            profile: Some(Profile::QualityMedium),
            discovery_timeout: Duration::from_secs(10),
        })
        .await
        .expect("send caller Call control");

    wait_matching_service_event(
        &mut caller.event_rx,
        Duration::from_secs(10),
        "outgoing call started through Python hub",
        |event| matches!(event, TelephonyServiceEvent::OutgoingCallStarted { .. }),
    )
    .await;

    let incoming = wait_matching_service_event(
        &mut callee.event_rx,
        Duration::from_secs(15),
        "incoming call through Python hub",
        |event| matches!(event, TelephonyServiceEvent::IncomingCall { .. }),
    )
    .await;
    match incoming {
        TelephonyServiceEvent::IncomingCall {
            remote_identity, ..
        } => assert_eq!(remote_identity, caller.identity.hash),
        other => panic!("expected IncomingCall, got {other:?}"),
    }

    callee
        .control_tx
        .send(TelephonyControl::Answer)
        .await
        .expect("send callee Answer control");

    wait_matching_service_event(
        &mut caller.event_rx,
        Duration::from_secs(15),
        "caller established through Python hub",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| {
                        call.status == SignallingStatus::Established
                            && call.role == CallRole::Outgoing
                            && call.remote_identity == callee.identity.hash
                    })
            )
        },
    )
    .await;

    wait_matching_service_event(
        &mut callee.event_rx,
        Duration::from_secs(15),
        "callee established through Python hub",
        |event| {
            matches!(
                event,
                TelephonyServiceEvent::Snapshot(snapshot)
                    if snapshot.active_call.as_ref().is_some_and(|call| {
                        call.status == SignallingStatus::Established
                            && call.role == CallRole::Incoming
                            && call.remote_identity == caller.identity.hash
                    })
            )
        },
    )
    .await;

    caller.shutdown().await;
    callee.shutdown().await;
    drop(hub);
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rust_telephony_startup_retry_announces_after_late_tcp_hub_connection() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust/Rust delayed TCP-hub interop");
        return;
    }

    let port = free_tcp_port();
    let hub = PythonTelephoneHost::spawn_transport_hub(port);
    let ready = hub.wait_event("READY", Duration::from_secs(10));
    assert_eq!(ready["headless_audio"], true);

    let caller = RustHubNode::spawn("rust-hub-caller-late", port, 13_001).await;
    let mut caller_announces = register_telephony_announce_handler(&caller.actor_tx).await;
    let mut callee = RustHubNode::spawn_before_tcp().await;

    tokio::time::sleep(Duration::from_millis(150)).await;
    callee
        .attach_tcp("rust-hub-callee-late", port, 14_001)
        .await;

    let announce = wait_for_announce(
        &mut caller_announces,
        callee.destination_hash,
        Duration::from_secs(5),
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
async fn rust_outgoing_call_reaches_python_ringing_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python LXST Telephone call interop");
        return;
    }

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
async fn rust_outgoing_call_establishes_when_python_answers_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust caller/Python answer interop");
        return;
    }

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
async fn python_outgoing_call_reaches_rust_incoming_ringing_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python LXST Telephone caller interop");
        return;
    }

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
async fn python_outgoing_call_establishes_when_rust_answers_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python caller/Rust answer interop");
        return;
    }

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

    let commands = core
        .answer_active()
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

    endpoint
        .try_drive_ready(&mut core)
        .expect("drive Rust endpoint after Python establishment response");
    let snapshot = core.snapshot();
    let call = snapshot
        .active_call
        .expect("active incoming established call");
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
async fn rust_hangup_after_python_answer_ends_python_call_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust hangup/Python ended interop");
        return;
    }

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
async fn python_hangup_after_rust_answer_ends_rust_call_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python hangup/Rust ended interop");
        return;
    }

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

    let commands = core
        .answer_active()
        .expect("answer active Rust incoming call");
    endpoint
        .execute_commands(&commands)
        .expect("send Rust answer signalling");
    helper.wait_event("ESTABLISHED", Duration::from_secs(5));

    endpoint
        .try_drive_ready(&mut core)
        .expect("drive Rust endpoint after Python establishment response");
    assert_eq!(
        core.snapshot()
            .active_call
            .as_ref()
            .expect("active incoming established call")
            .status,
        SignallingStatus::Established
    );

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
async fn rust_outgoing_call_receives_python_busy_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python busy/Rust caller interop");
        return;
    }

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
async fn python_outgoing_call_receives_rust_busy_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust busy/Python caller interop");
        return;
    }

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
async fn rust_outgoing_call_receives_python_reject_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python reject/Rust caller interop");
        return;
    }

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
async fn python_outgoing_call_receives_rust_reject_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust reject/Python caller interop");
        return;
    }

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
async fn rust_service_outgoing_timeout_ends_python_ringing_call_without_audio() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust outgoing timeout/Python callee interop");
        return;
    }

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
async fn rust_service_incoming_timeout_ends_python_outgoing_call_without_reject() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust incoming timeout/Python caller interop");
        return;
    }

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
async fn rust_service_sends_raw_media_to_python_established_call() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust Raw media/Python receiver interop");
        return;
    }

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

    let first = helper.wait_event("MEDIA_FRAME", Duration::from_secs(5));
    assert_eq!(first["shape"], json!([3, 1]));
    assert_eq!(first["samples"], json!([0.0, 0.25, -0.5]));
    let second = helper.wait_event("MEDIA_FRAME", Duration::from_secs(5));
    assert_eq!(second["shape"], json!([2, 1]));
    assert_eq!(second["samples"], json!([1.0, -1.0]));

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
async fn rust_service_sends_opus_voice_profile_matrix_to_python_established_calls() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust Opus media/Python receiver interop");
        return;
    }

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
async fn rust_profile_switch_reaches_python_and_media_uses_new_profile() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Rust-to-Python profile-switch interop");
        return;
    }

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
async fn python_profile_switch_reaches_rust_and_media_uses_new_profile() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python-to-Rust profile-switch interop");
        return;
    }

    let Some(mut session) = RustCallerSession::establish(Profile::QualityHigh).await else {
        return;
    };
    session.switch_python_profile(Profile::QualityMax).await;
    session.send_python_opus_to_rust().await;
    session.shutdown().await;
}

#[serial_test::serial(python_lxst_live)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rust_service_receives_python_raw_media_on_established_call() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python Raw media/Rust receiver interop");
        return;
    }

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
async fn rust_service_decodes_python_opus_voice_profile_matrix() {
    if should_skip() {
        eprintln!("{SKIP_ENV}=1 -> skipping live Python Opus media/Rust decoder interop");
        return;
    }

    for profile in OPUS_VOICE_PROFILES {
        let Some(mut session) = RustCallerSession::establish(profile).await else {
            return;
        };
        session.send_python_opus_to_rust().await;
        session.shutdown().await;
    }
}
