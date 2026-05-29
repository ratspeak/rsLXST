//! Telephony runtime core for LXST.
//!
//! This crate owns the deterministic call-control boundary between Reticulum
//! link events and the pure LXST signalling planner. It deliberately avoids
//! spawning tasks or touching audio devices so live runtime code can be a thin
//! adapter around tested protocol behavior.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use lxst_core::{
    CallRole, CodecKind, Frame, FrameStreamEvent, LxstPacket, OpusCodecError, OpusDecoderState,
    OpusEncoderState, Profile, RawAudioFrame, RawBitDepth, Signal, SignallingStatus,
    TELEPHONY_DESTINATION_NAME, TelephonyAction, TelephonyCall,
};
use lxst_rns::{InboundLxstPacket, LxstLinkIngress, LxstMediaEgress, queue_lxst_link_packet};
use rns_crypto::ed25519::{Ed25519PrivateKey, Ed25519PublicKey};
use rns_identity::destination::{
    DestType, Destination, DestinationError, Direction, ProofStrategy,
};
use rns_identity::identity::Identity;
use rns_identity::name_hash::name_hash;
use rns_link::link::{CloseReason, Link};
use rns_runtime::link_manager::LinkManager;
use rns_transport::link_messages::DestinationEvent;
use rns_transport::messages::{
    AnnounceHandlerEvent, AnnounceRpcEntry, OutboundRequest, TransportMessage, TransportQuery,
    TransportQueryResponse,
};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant, timeout};

pub type LinkId = [u8; 16];
pub type IdentityHash = [u8; 16];

pub const TELEPHONY_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(60 * 60 * 3);
pub const TELEPHONY_STARTUP_ANNOUNCE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
pub const TELEPHONY_STARTUP_ANNOUNCE_RETRIES: u8 = 6;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Reticulum destination error: {0}")]
    Destination(#[from] DestinationError),
    #[error("LXST Reticulum boundary error: {0}")]
    Rns(#[from] lxst_rns::Error),
    #[error("LXST Opus codec error: {0}")]
    Opus(#[from] OpusCodecError),
    #[error("unknown LXST link")]
    UnknownLink,
    #[error("line is busy")]
    LineBusy,
    #[error("no active call")]
    NoActiveCall,
    #[error("active call is not established")]
    CallNotEstablished,
    #[error("requested media profile {requested:?} does not match active call profile {active:?}")]
    MediaProfileMismatch { active: Profile, requested: Profile },
    #[error("active call is on a different link")]
    WrongActiveLink,
    #[error("local identity does not contain a signing key")]
    NoSigningKey,
    #[error("Reticulum transport queue is closed")]
    TransportClosed,
    #[error("Reticulum transport queue is full")]
    TransportFull,
    #[error("LXST telephony service event queue is closed")]
    ServiceEventClosed,
    #[error("LXST telephony service event queue is full")]
    ServiceEventFull,
    #[error("Reticulum transport query channel closed")]
    TransportQueryClosed,
    #[error("unexpected Reticulum transport query response")]
    UnexpectedTransportQueryResponse,
    #[error("Reticulum link operation failed: {0}")]
    LinkOperation(String),
    #[error("Reticulum link proof validation failed: {0}")]
    LinkProofInvalid(String),
    #[error("Reticulum link destination channel closed")]
    LinkDestinationClosed,
    #[error("unexpected Reticulum destination event for link")]
    UnexpectedLinkEvent,
    #[error("remote LXST telephony announce did not include a Reticulum public key")]
    RemotePublicKeyMissing,
    #[error("remote LXST telephony announce was not discovered before timeout")]
    RemoteTelephonyPeerNotDiscovered,
    #[error("remote LXST telephony Reticulum path was not discovered before timeout")]
    RemotePathNotDiscovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteTelephonyPeer {
    pub identity_hash: IdentityHash,
    pub destination_hash: [u8; 16],
    pub public_key: [u8; 64],
    pub hops: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerAllowPolicy {
    All,
    None,
    List(Vec<IdentityHash>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerAccessPolicy {
    pub allowed: CallerAllowPolicy,
    pub blocked: Vec<IdentityHash>,
}

impl Default for CallerAccessPolicy {
    fn default() -> Self {
        Self {
            allowed: CallerAllowPolicy::All,
            blocked: Vec::new(),
        }
    }
}

impl CallerAccessPolicy {
    pub fn is_allowed(&self, identity_hash: &IdentityHash) -> bool {
        if self.blocked.iter().any(|blocked| blocked == identity_hash) {
            return false;
        }

        match &self.allowed {
            CallerAllowPolicy::All => true,
            CallerAllowPolicy::None => false,
            CallerAllowPolicy::List(allowed) => {
                allowed.iter().any(|allowed| allowed == identity_hash)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyCommand {
    SendSignal {
        link_id: LinkId,
        signal: Signal,
    },
    IdentifyLocalIdentity {
        link_id: LinkId,
    },
    SelectProfile {
        link_id: LinkId,
        profile: Profile,
    },
    SwitchProfile {
        link_id: LinkId,
        profile: Profile,
    },
    PrepareDialingPipelines {
        link_id: LinkId,
    },
    ResetDialingPipelines {
        link_id: LinkId,
    },
    OpenAudioPipelines {
        link_id: LinkId,
    },
    StartAudioPipelines {
        link_id: LinkId,
    },
    StopAudioPipelines {
        link_id: LinkId,
    },
    StartDialTone {
        link_id: LinkId,
    },
    RingIncomingCall {
        link_id: LinkId,
        remote_identity: IdentityHash,
    },
    CallTerminated {
        link_id: LinkId,
        reason: Option<SignallingStatus>,
    },
    TeardownLink {
        link_id: LinkId,
    },
    IgnoredSignal {
        link_id: LinkId,
        signal: Signal,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelephonyCommandEffect {
    Noop,
    QueuedLinkPacket {
        link_id: LinkId,
        kind: QueuedLinkPacketKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedLinkPacketKind {
    LxstData,
    LinkIdentify,
    LinkClose,
}

impl TelephonyCommand {
    pub const fn link_id(&self) -> LinkId {
        match self {
            Self::SendSignal { link_id, .. }
            | Self::IdentifyLocalIdentity { link_id }
            | Self::SelectProfile { link_id, .. }
            | Self::SwitchProfile { link_id, .. }
            | Self::PrepareDialingPipelines { link_id }
            | Self::ResetDialingPipelines { link_id }
            | Self::OpenAudioPipelines { link_id }
            | Self::StartAudioPipelines { link_id }
            | Self::StopAudioPipelines { link_id }
            | Self::StartDialTone { link_id }
            | Self::RingIncomingCall { link_id, .. }
            | Self::CallTerminated { link_id, .. }
            | Self::TeardownLink { link_id }
            | Self::IgnoredSignal { link_id, .. } => *link_id,
        }
    }

    pub const fn signal(&self) -> Option<Signal> {
        match self {
            Self::SendSignal { signal, .. } => Some(*signal),
            _ => None,
        }
    }

    pub fn to_lxst_packet(&self) -> Option<LxstPacket> {
        self.signal().map(signalling_packet)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelephonyStep {
    pub inbound: Option<InboundLxstPacket>,
    pub commands: Vec<TelephonyCommand>,
}

impl TelephonyStep {
    pub fn commands(commands: Vec<TelephonyCommand>) -> Self {
        Self {
            inbound: None,
            commands,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelephonyDriveStep {
    pub step: TelephonyStep,
    pub effects: Vec<TelephonyCommandEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyLinkEvent {
    LinkEstablished {
        link_id: LinkId,
    },
    IncomingLinkEstablished {
        link_id: LinkId,
    },
    OutgoingLinkEstablished {
        link_id: LinkId,
    },
    RemoteIdentified {
        link_id: LinkId,
        remote_identity: IdentityHash,
    },
    LinkPacket {
        link_id: LinkId,
        plaintext: Vec<u8>,
    },
    LinkClosed {
        link_id: LinkId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCall {
    pub link_id: LinkId,
    pub remote_identity: IdentityHash,
    pub call: TelephonyCall,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveCallSnapshot {
    pub link_id: LinkId,
    pub remote_identity: IdentityHash,
    pub role: CallRole,
    pub status: SignallingStatus,
    pub profile: Option<Profile>,
    pub answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelephonyRuntimeSnapshot {
    pub external_busy: bool,
    pub pending_link_count: usize,
    pub active_call: Option<ActiveCallSnapshot>,
}

#[derive(Debug, Default)]
pub struct TelephonyRuntimeCore {
    access: CallerAccessPolicy,
    external_busy: bool,
    pending_links: HashSet<LinkId>,
    ingress: HashMap<LinkId, LxstLinkIngress>,
    active_call: Option<ActiveCall>,
}

impl TelephonyRuntimeCore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_access_policy(access: CallerAccessPolicy) -> Self {
        Self {
            access,
            ..Self::default()
        }
    }

    pub const fn access_policy(&self) -> &CallerAccessPolicy {
        &self.access
    }

    pub fn set_access_policy(&mut self, access: CallerAccessPolicy) {
        self.access = access;
    }

    pub const fn external_busy(&self) -> bool {
        self.external_busy
    }

    pub fn set_external_busy(&mut self, busy: bool) {
        self.external_busy = busy;
    }

    pub fn line_busy(&self) -> bool {
        self.external_busy || self.active_call.is_some()
    }

    pub fn active_call(&self) -> Option<&ActiveCall> {
        self.active_call.as_ref()
    }

    pub fn pending_link_count(&self) -> usize {
        self.pending_links.len()
    }

    pub fn snapshot(&self) -> TelephonyRuntimeSnapshot {
        TelephonyRuntimeSnapshot {
            external_busy: self.external_busy,
            pending_link_count: self.pending_links.len(),
            active_call: self.active_call.as_ref().map(|active| ActiveCallSnapshot {
                link_id: active.link_id,
                remote_identity: active.remote_identity,
                role: active.call.role(),
                status: active.call.status(),
                profile: active.call.profile(),
                answered: active.call.answered(),
            }),
        }
    }

    pub fn incoming_link_established(&mut self, link_id: LinkId) -> Vec<TelephonyCommand> {
        let actions = TelephonyCall::incoming_link_established(self.line_busy());
        if !self.line_busy() {
            self.pending_links.insert(link_id);
            self.ingress.entry(link_id).or_default();
        }

        self.commands_from_actions(link_id, None, actions)
    }

    pub fn caller_identified(
        &mut self,
        link_id: LinkId,
        remote_identity: IdentityHash,
    ) -> Result<Vec<TelephonyCommand>, Error> {
        if !self.pending_links.contains(&link_id) && !self.ingress.contains_key(&link_id) {
            return Err(Error::UnknownLink);
        }

        let allowed = self.access.is_allowed(&remote_identity);
        let busy = self.line_busy();
        let mut call = TelephonyCall::incoming();
        let actions = call.caller_identified(busy, allowed);

        if busy || !allowed {
            self.pending_links.remove(&link_id);
            self.ingress.remove(&link_id);
            return Ok(self.commands_from_actions(link_id, Some(remote_identity), actions));
        }

        self.pending_links.remove(&link_id);
        self.active_call = Some(ActiveCall {
            link_id,
            remote_identity,
            call,
        });

        Ok(self.commands_from_actions(link_id, Some(remote_identity), actions))
    }

    pub fn start_outgoing_call(
        &mut self,
        link_id: LinkId,
        remote_identity: IdentityHash,
        profile: Option<Profile>,
    ) -> Result<(), Error> {
        if self.line_busy() {
            return Err(Error::LineBusy);
        }

        self.ingress.entry(link_id).or_default();
        self.active_call = Some(ActiveCall {
            link_id,
            remote_identity,
            call: TelephonyCall::outgoing(profile),
        });
        Ok(())
    }

    pub fn outgoing_link_established(&mut self, link_id: LinkId) -> Result<(), Error> {
        let active = self.active_call.as_ref().ok_or(Error::NoActiveCall)?;
        if active.link_id != link_id {
            return Err(Error::WrongActiveLink);
        }
        self.ingress.entry(link_id).or_default();
        Ok(())
    }

    pub fn answer_active(&mut self) -> Result<Vec<TelephonyCommand>, Error> {
        let active = self.active_call.as_mut().ok_or(Error::NoActiveCall)?;
        let link_id = active.link_id;
        let remote_identity = active.remote_identity;
        let actions = active.call.answer();
        Ok(self.commands_from_actions(link_id, Some(remote_identity), actions))
    }

    pub fn switch_active_profile(
        &mut self,
        profile: Profile,
    ) -> Result<Vec<TelephonyCommand>, Error> {
        let active = self.active_call.as_mut().ok_or(Error::NoActiveCall)?;
        if active.call.status() != SignallingStatus::Established {
            return Err(Error::CallNotEstablished);
        }

        let link_id = active.link_id;
        let remote_identity = active.remote_identity;
        let actions = active.call.switch_profile(profile);
        Ok(self.commands_from_actions(link_id, Some(remote_identity), actions))
    }

    pub fn hangup_active(&mut self, ring_timeout: bool) -> Result<Vec<TelephonyCommand>, Error> {
        let Some(mut active) = self.active_call.take() else {
            return Err(Error::NoActiveCall);
        };
        let link_id = active.link_id;
        let remote_identity = active.remote_identity;
        let mut commands = self.commands_from_actions(
            link_id,
            Some(remote_identity),
            active.call.hangup(ring_timeout),
        );
        commands.push(TelephonyCommand::StopAudioPipelines { link_id });
        commands.push(TelephonyCommand::CallTerminated {
            link_id,
            reason: None,
        });
        self.pending_links.remove(&link_id);
        self.ingress.remove(&link_id);
        Ok(commands)
    }

    pub fn shutdown(&mut self) -> Vec<TelephonyCommand> {
        let mut commands = Vec::new();

        if let Some(mut active) = self.active_call.take() {
            let link_id = active.link_id;
            let remote_identity = active.remote_identity;
            commands.extend(self.commands_from_actions(
                link_id,
                Some(remote_identity),
                active.call.hangup(false),
            ));
            commands.push(TelephonyCommand::StopAudioPipelines { link_id });
            commands.push(TelephonyCommand::CallTerminated {
                link_id,
                reason: None,
            });
            self.pending_links.remove(&link_id);
            self.ingress.remove(&link_id);
        }

        for link_id in self.pending_links.drain().collect::<Vec<_>>() {
            self.ingress.remove(&link_id);
            commands.push(TelephonyCommand::TeardownLink { link_id });
        }
        self.ingress.clear();

        commands
    }

    pub fn link_closed(&mut self, link_id: LinkId) -> Vec<TelephonyCommand> {
        self.pending_links.remove(&link_id);
        self.ingress.remove(&link_id);

        if self
            .active_call
            .as_ref()
            .is_some_and(|active| active.link_id == link_id)
        {
            self.active_call.take();
            vec![
                TelephonyCommand::StopAudioPipelines { link_id },
                TelephonyCommand::CallTerminated {
                    link_id,
                    reason: None,
                },
            ]
        } else {
            Vec::new()
        }
    }

    pub fn accept_lxst_plaintext(
        &mut self,
        link_id: LinkId,
        payload: &[u8],
    ) -> Result<TelephonyStep, Error> {
        let ingress = self.ingress.get_mut(&link_id).ok_or(Error::UnknownLink)?;
        let inbound = ingress.accept_plaintext(link_id, payload)?;
        let mut commands = Vec::new();

        for signal in inbound.packet.signals.clone() {
            commands.extend(self.receive_signal(link_id, signal)?);
        }

        Ok(TelephonyStep {
            inbound: Some(inbound),
            commands,
        })
    }

    pub fn accept_link_event(&mut self, event: TelephonyLinkEvent) -> Result<TelephonyStep, Error> {
        match event {
            TelephonyLinkEvent::LinkEstablished { link_id } => {
                if self.active_call.as_ref().is_some_and(|active| {
                    active.link_id == link_id && active.call.role() == CallRole::Outgoing
                }) {
                    self.outgoing_link_established(link_id)?;
                    Ok(TelephonyStep::commands(Vec::new()))
                } else {
                    Ok(TelephonyStep::commands(
                        self.incoming_link_established(link_id),
                    ))
                }
            }
            TelephonyLinkEvent::IncomingLinkEstablished { link_id } => Ok(TelephonyStep::commands(
                self.incoming_link_established(link_id),
            )),
            TelephonyLinkEvent::OutgoingLinkEstablished { link_id } => {
                self.outgoing_link_established(link_id)?;
                Ok(TelephonyStep::commands(Vec::new()))
            }
            TelephonyLinkEvent::RemoteIdentified {
                link_id,
                remote_identity,
            } => Ok(TelephonyStep::commands(
                self.caller_identified(link_id, remote_identity)?,
            )),
            TelephonyLinkEvent::LinkPacket { link_id, plaintext } => {
                self.accept_lxst_plaintext(link_id, &plaintext)
            }
            TelephonyLinkEvent::LinkClosed { link_id } => {
                Ok(TelephonyStep::commands(self.link_closed(link_id)))
            }
        }
    }

    pub fn receive_signal(
        &mut self,
        link_id: LinkId,
        signal: Signal,
    ) -> Result<Vec<TelephonyCommand>, Error> {
        let active = self.active_call.as_mut().ok_or(Error::NoActiveCall)?;
        if active.link_id != link_id {
            return Err(Error::WrongActiveLink);
        }

        let remote_identity = active.remote_identity;
        let actions = active.call.receive_signal(signal);
        let terminates = actions.iter().find_map(|action| match action {
            TelephonyAction::Terminate(reason) => Some(*reason),
            _ => None,
        });
        let mut commands = self.commands_from_actions(link_id, Some(remote_identity), actions);

        if let Some(reason) = terminates {
            commands.push(TelephonyCommand::StopAudioPipelines { link_id });
            commands.push(TelephonyCommand::TeardownLink { link_id });
            commands.push(TelephonyCommand::CallTerminated { link_id, reason });
            self.active_call.take();
            self.pending_links.remove(&link_id);
            self.ingress.remove(&link_id);
        }

        Ok(commands)
    }

    fn commands_from_actions(
        &self,
        link_id: LinkId,
        remote_identity: Option<IdentityHash>,
        actions: Vec<TelephonyAction>,
    ) -> Vec<TelephonyCommand> {
        actions
            .into_iter()
            .filter_map(|action| match action {
                TelephonyAction::SendSignal(signal) => {
                    Some(TelephonyCommand::SendSignal { link_id, signal })
                }
                TelephonyAction::IdentifyLocalIdentity => {
                    Some(TelephonyCommand::IdentifyLocalIdentity { link_id })
                }
                TelephonyAction::SelectProfile(profile) => {
                    Some(TelephonyCommand::SelectProfile { link_id, profile })
                }
                TelephonyAction::PrepareDialingPipelines => {
                    Some(TelephonyCommand::PrepareDialingPipelines { link_id })
                }
                TelephonyAction::ResetDialingPipelines => {
                    Some(TelephonyCommand::ResetDialingPipelines { link_id })
                }
                TelephonyAction::OpenAudioPipelines => {
                    Some(TelephonyCommand::OpenAudioPipelines { link_id })
                }
                TelephonyAction::StartAudioPipelines => {
                    Some(TelephonyCommand::StartAudioPipelines { link_id })
                }
                TelephonyAction::StartDialTone => Some(TelephonyCommand::StartDialTone { link_id }),
                TelephonyAction::TeardownLink => Some(TelephonyCommand::TeardownLink { link_id }),
                TelephonyAction::RingIncomingCall => {
                    remote_identity.map(|remote_identity| TelephonyCommand::RingIncomingCall {
                        link_id,
                        remote_identity,
                    })
                }
                TelephonyAction::SwitchProfile(profile) => {
                    Some(TelephonyCommand::SwitchProfile { link_id, profile })
                }
                TelephonyAction::IgnoreSignal(signal) => {
                    Some(TelephonyCommand::IgnoredSignal { link_id, signal })
                }
                TelephonyAction::Terminate(_) => None,
            })
            .collect()
    }
}

#[derive(Debug)]
pub enum TelephonyControl {
    Announce,
    Call {
        remote_identity: IdentityHash,
        profile: Option<Profile>,
        discovery_timeout: Duration,
    },
    Answer,
    Hangup {
        ring_timeout: bool,
    },
    SendRawFrames {
        bit_depth: RawBitDepth,
        frames: Vec<RawAudioFrame>,
    },
    SendOpusFrames {
        profile: Profile,
        frames: Vec<RawAudioFrame>,
    },
    StartOpusStream {
        profile: Profile,
        frames: mpsc::Receiver<RawAudioFrame>,
    },
    StopOpusStream,
    StartOpusReceiveStream {
        frames: mpsc::Sender<RawAudioFrame>,
    },
    StopOpusReceiveStream,
    SwitchProfile {
        profile: Profile,
    },
    SetExternalBusy(bool),
    SetAccessPolicy(CallerAccessPolicy),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusTransmitStreamStopReason {
    Requested,
    Replaced,
    SourceClosed,
    CallEnded,
    ProfileChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusReceiveStreamStopReason {
    Requested,
    Replaced,
    SinkClosed,
    CallEnded,
    ProfileChanged,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TelephonyServiceEvent {
    OutgoingCallPending {
        remote_identity: IdentityHash,
    },
    OutgoingCallStarted {
        link_id: LinkId,
        remote_identity: IdentityHash,
    },
    OutgoingCallFailed {
        remote_identity: IdentityHash,
        message: String,
    },
    IncomingCall {
        link_id: LinkId,
        remote_identity: IdentityHash,
    },
    CallTerminated {
        link_id: LinkId,
        reason: Option<SignallingStatus>,
    },
    MediaSent {
        link_id: LinkId,
        frames: usize,
        packets: usize,
    },
    MediaReceived {
        link_id: LinkId,
        frames: usize,
    },
    OpusFramesReceived {
        link_id: LinkId,
        profile: Profile,
        frames: Vec<RawAudioFrame>,
    },
    OpusTransmitStreamStarted {
        link_id: LinkId,
        profile: Profile,
    },
    OpusTransmitStreamStopped {
        link_id: LinkId,
        profile: Profile,
        reason: OpusTransmitStreamStopReason,
    },
    OpusReceiveStreamStarted {
        link_id: LinkId,
        profile: Profile,
    },
    OpusReceiveStreamStopped {
        link_id: LinkId,
        profile: Profile,
        reason: OpusReceiveStreamStopReason,
    },
    OpusReceiveStreamFrames {
        link_id: LinkId,
        profile: Profile,
        frames: usize,
        dropped: usize,
    },
    Snapshot(TelephonyRuntimeSnapshot),
    Drive(TelephonyDriveStep),
    Error {
        message: String,
    },
    Stopped,
}

#[derive(Debug)]
enum TelephonyInternalEvent {
    OutgoingDiscoveryCompleted {
        remote_identity: IdentityHash,
        profile: Option<Profile>,
        result: Result<RemoteTelephonyPeer, Error>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutgoingDiscoveryState {
    remote_identity: IdentityHash,
    profile: Option<Profile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelephonyServiceConfig {
    pub poll_interval: Duration,
    pub incoming_ring_timeout: Option<Duration>,
    pub outgoing_call_timeout: Option<Duration>,
    pub media_frames_per_tick: usize,
    pub announce_on_start: bool,
    pub announce_interval: Option<Duration>,
    pub startup_announce_retry_interval: Option<Duration>,
    pub startup_announce_retries: u8,
}

impl Default for TelephonyServiceConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(20),
            incoming_ring_timeout: Some(Duration::from_secs(60)),
            outgoing_call_timeout: Some(Duration::from_secs(70)),
            media_frames_per_tick: 4,
            announce_on_start: true,
            announce_interval: Some(TELEPHONY_ANNOUNCE_INTERVAL),
            startup_announce_retry_interval: Some(TELEPHONY_STARTUP_ANNOUNCE_RETRY_INTERVAL),
            startup_announce_retries: TELEPHONY_STARTUP_ANNOUNCE_RETRIES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelephonyServiceChannelConfig {
    pub control_capacity: usize,
    pub event_capacity: usize,
}

impl Default for TelephonyServiceChannelConfig {
    fn default() -> Self {
        Self {
            control_capacity: 32,
            event_capacity: 128,
        }
    }
}

pub struct TelephonyServiceParts {
    pub service: TelephonyService,
    pub control_tx: mpsc::Sender<TelephonyControl>,
    pub event_rx: mpsc::Receiver<TelephonyServiceEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TelephonyServiceTimeoutKind {
    IncomingRing,
    OutgoingCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TelephonyServiceTimeout {
    link_id: LinkId,
    kind: TelephonyServiceTimeoutKind,
    expires_at: Instant,
}

#[derive(Default)]
struct TelephonyServiceMedia {
    opus_encoder: Option<ActiveOpusEncoder>,
    opus_encoder_generation: u64,
    opus_decoder: Option<ActiveOpusDecoder>,
    opus_decoder_generation: u64,
    opus_transmit_stream: Option<ActiveOpusTransmitStream>,
    opus_receive_stream: Option<ActiveOpusReceiveStream>,
}

struct ActiveOpusEncoder {
    link_id: LinkId,
    profile: Profile,
    encoder: OpusEncoderState,
}

struct ActiveOpusDecoder {
    link_id: LinkId,
    profile: Profile,
    decoder: OpusDecoderState,
}

struct ActiveOpusTransmitStream {
    link_id: LinkId,
    profile: Profile,
    frames_rx: mpsc::Receiver<RawAudioFrame>,
}

struct ActiveOpusReceiveStream {
    link_id: LinkId,
    profile: Profile,
    frames_tx: mpsc::Sender<RawAudioFrame>,
}

impl TelephonyServiceMedia {
    fn clear_unless_established(
        &mut self,
        active: Option<&ActiveCallSnapshot>,
    ) -> Vec<TelephonyServiceEvent> {
        let mut events = Vec::new();
        let Some(active) = active.filter(|active| active.status == SignallingStatus::Established)
        else {
            self.opus_encoder = None;
            self.opus_decoder = None;
            if let Some(stream) = self.opus_transmit_stream.take() {
                events.push(TelephonyServiceEvent::OpusTransmitStreamStopped {
                    link_id: stream.link_id,
                    profile: stream.profile,
                    reason: OpusTransmitStreamStopReason::CallEnded,
                });
            }
            if let Some(stream) = self.opus_receive_stream.take() {
                events.push(TelephonyServiceEvent::OpusReceiveStreamStopped {
                    link_id: stream.link_id,
                    profile: stream.profile,
                    reason: OpusReceiveStreamStopReason::CallEnded,
                });
            }
            return events;
        };

        if self
            .opus_encoder
            .as_ref()
            .is_some_and(|encoder| encoder.link_id != active.link_id)
        {
            self.opus_encoder = None;
        }
        if self
            .opus_decoder
            .as_ref()
            .is_some_and(|decoder| decoder.link_id != active.link_id)
        {
            self.opus_decoder = None;
        }
        if self.opus_transmit_stream.as_ref().is_some_and(|stream| {
            stream.link_id != active.link_id || Some(stream.profile) != active.profile
        }) {
            if let Some(stream) = self.opus_transmit_stream.take() {
                let reason = if stream.link_id == active.link_id {
                    OpusTransmitStreamStopReason::ProfileChanged
                } else {
                    OpusTransmitStreamStopReason::CallEnded
                };
                events.push(TelephonyServiceEvent::OpusTransmitStreamStopped {
                    link_id: stream.link_id,
                    profile: stream.profile,
                    reason,
                });
            }
        }
        if self.opus_receive_stream.as_ref().is_some_and(|stream| {
            stream.link_id != active.link_id || Some(stream.profile) != active.profile
        }) {
            if let Some(stream) = self.opus_receive_stream.take() {
                let reason = if stream.link_id == active.link_id {
                    OpusReceiveStreamStopReason::ProfileChanged
                } else {
                    OpusReceiveStreamStopReason::CallEnded
                };
                events.push(TelephonyServiceEvent::OpusReceiveStreamStopped {
                    link_id: stream.link_id,
                    profile: stream.profile,
                    reason,
                });
            }
        }
        events
    }

    fn opus_encoder_for(
        &mut self,
        link_id: LinkId,
        profile: Profile,
    ) -> Result<&mut OpusEncoderState, OpusCodecError> {
        let needs_new = self
            .opus_encoder
            .as_ref()
            .is_none_or(|encoder| encoder.link_id != link_id || encoder.profile != profile);

        if needs_new {
            self.opus_encoder_generation += 1;
            self.opus_encoder = Some(ActiveOpusEncoder {
                link_id,
                profile,
                encoder: OpusEncoderState::new(profile)?,
            });
        }

        Ok(&mut self
            .opus_encoder
            .as_mut()
            .expect("Opus encoder exists after creation")
            .encoder)
    }

    fn opus_decoder_for(
        &mut self,
        link_id: LinkId,
        profile: Profile,
    ) -> Result<&mut OpusDecoderState, OpusCodecError> {
        let needs_new = self
            .opus_decoder
            .as_ref()
            .is_none_or(|decoder| decoder.link_id != link_id || decoder.profile != profile);

        if needs_new {
            self.opus_decoder_generation += 1;
            self.opus_decoder = Some(ActiveOpusDecoder {
                link_id,
                profile,
                decoder: OpusDecoderState::new(profile)?,
            });
        }

        Ok(&mut self
            .opus_decoder
            .as_mut()
            .expect("Opus decoder exists after creation")
            .decoder)
    }
}

pub struct TelephonyService {
    endpoint: TelephonyRnsEndpoint,
    core: TelephonyRuntimeCore,
    control_rx: mpsc::Receiver<TelephonyControl>,
    internal_tx: mpsc::Sender<TelephonyInternalEvent>,
    internal_rx: mpsc::Receiver<TelephonyInternalEvent>,
    event_tx: mpsc::Sender<TelephonyServiceEvent>,
    config: TelephonyServiceConfig,
    active_timeout: Option<TelephonyServiceTimeout>,
    next_announce_at: Option<Instant>,
    startup_announce_retries_remaining: u8,
    outgoing_discovery: Option<OutgoingDiscoveryState>,
    media: TelephonyServiceMedia,
}

impl TelephonyService {
    pub fn registered(
        transport_tx: mpsc::Sender<TransportMessage>,
        identity: &Identity,
    ) -> Result<TelephonyServiceParts, Error> {
        Self::registered_with_config(
            transport_tx,
            identity,
            TelephonyServiceConfig::default(),
            TelephonyServiceChannelConfig::default(),
        )
    }

    pub fn registered_with_config(
        transport_tx: mpsc::Sender<TransportMessage>,
        identity: &Identity,
        config: TelephonyServiceConfig,
        channels: TelephonyServiceChannelConfig,
    ) -> Result<TelephonyServiceParts, Error> {
        let endpoint = TelephonyRnsEndpoint::register(transport_tx, identity)?;
        let (control_tx, control_rx) = mpsc::channel(channels.control_capacity.max(1));
        let (event_tx, event_rx) = mpsc::channel(channels.event_capacity.max(1));
        let service = Self::with_config(
            endpoint,
            TelephonyRuntimeCore::new(),
            control_rx,
            event_tx,
            config,
        );

        Ok(TelephonyServiceParts {
            service,
            control_tx,
            event_rx,
        })
    }

    pub fn new(
        endpoint: TelephonyRnsEndpoint,
        core: TelephonyRuntimeCore,
        control_rx: mpsc::Receiver<TelephonyControl>,
        event_tx: mpsc::Sender<TelephonyServiceEvent>,
    ) -> Self {
        Self::with_config(
            endpoint,
            core,
            control_rx,
            event_tx,
            TelephonyServiceConfig::default(),
        )
    }

    pub fn with_config(
        endpoint: TelephonyRnsEndpoint,
        core: TelephonyRuntimeCore,
        control_rx: mpsc::Receiver<TelephonyControl>,
        event_tx: mpsc::Sender<TelephonyServiceEvent>,
        config: TelephonyServiceConfig,
    ) -> Self {
        let next_announce_at = if config.announce_on_start {
            Some(Instant::now())
        } else {
            config
                .announce_interval
                .map(|interval| Instant::now() + interval)
        };
        let startup_announce_retries_remaining =
            if config.announce_on_start && config.startup_announce_retry_interval.is_some() {
                config.startup_announce_retries
            } else {
                0
            };
        let (internal_tx, internal_rx) = mpsc::channel(32);

        Self {
            endpoint,
            core,
            control_rx,
            internal_tx,
            internal_rx,
            event_tx,
            config,
            active_timeout: None,
            next_announce_at,
            startup_announce_retries_remaining,
            outgoing_discovery: None,
            media: TelephonyServiceMedia::default(),
        }
    }

    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(self.config.poll_interval);

        loop {
            tokio::select! {
                control = self.control_rx.recv() => {
                    let Some(control) = control else {
                        break;
                    };
                    if !self.handle_control(control).await {
                        break;
                    }
                }
                internal = self.internal_rx.recv() => {
                    let Some(internal) = internal else {
                        break;
                    };
                    if !self.handle_internal(internal).await {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if !self.handle_due_announce().await {
                        break;
                    }
                    if !self.drive_ready().await {
                        break;
                    }
                    if !self.handle_due_timeout().await {
                        break;
                    }
                    if !self.pump_opus_stream().await {
                        break;
                    }
                }
            }
        }

        self.shutdown().await;
        self.deregister_destinations();
        let _ = self.event_tx.send(TelephonyServiceEvent::Stopped).await;
    }

    async fn handle_due_announce(&mut self) -> bool {
        let Some(next_announce_at) = self.next_announce_at else {
            return true;
        };
        let now = Instant::now();
        if now < next_announce_at {
            return true;
        }

        match self.endpoint.announce() {
            Ok(()) => {
                self.next_announce_at = if self.startup_announce_retries_remaining > 0 {
                    self.startup_announce_retries_remaining -= 1;
                    self.config
                        .startup_announce_retry_interval
                        .map(|interval| Instant::now() + interval)
                } else {
                    self.config
                        .announce_interval
                        .map(|interval| Instant::now() + interval)
                };
                true
            }
            Err(err) => {
                let retry_interval = self.config.poll_interval.max(Duration::from_secs(1));
                self.next_announce_at = Some(Instant::now() + retry_interval);
                emit_service_error(self.event_tx.clone(), err).await
            }
        }
    }

    async fn shutdown(&mut self) {
        self.outgoing_discovery = None;
        let commands = self.core.shutdown();
        if !commands.is_empty() {
            let _ = self.control_commands(Ok(commands)).await;
        }

        let stream_events = self.media.clear_unless_established(None);
        let _ = emit_service_events(self.event_tx.clone(), stream_events).await;
        self.active_timeout = None;
    }

    fn deregister_destinations(&self) {
        for link_id in self
            .endpoint
            .outgoing_attempts
            .keys()
            .chain(self.endpoint.outgoing_links.keys())
        {
            let _ = self.endpoint.deregister_link_destination(*link_id);
        }
        let _ = self.endpoint.deregister_destination();
    }

    async fn handle_control(&mut self, control: TelephonyControl) -> bool {
        let result = match control {
            TelephonyControl::Announce => {
                let result = self.endpoint.announce();
                if result.is_ok() {
                    self.startup_announce_retries_remaining = 0;
                    self.next_announce_at = self
                        .config
                        .announce_interval
                        .map(|interval| Instant::now() + interval);
                }
                result
            }
            TelephonyControl::Call {
                remote_identity,
                profile,
                discovery_timeout,
            } => {
                return self
                    .start_outgoing_discovery(remote_identity, profile, discovery_timeout)
                    .await;
            }
            TelephonyControl::Answer => {
                let commands = self.core.answer_active();
                self.control_commands(commands).await
            }
            TelephonyControl::Hangup { ring_timeout } => {
                if let Some(discovery) = self.outgoing_discovery.take() {
                    return emit_service_event(
                        self.event_tx.clone(),
                        TelephonyServiceEvent::OutgoingCallFailed {
                            remote_identity: discovery.remote_identity,
                            message: "cancelled".to_string(),
                        },
                    )
                    .await;
                }
                let commands = self.core.hangup_active(ring_timeout);
                self.control_commands(commands).await
            }
            TelephonyControl::SendRawFrames { bit_depth, frames } => {
                return match self.send_raw_frames(bit_depth, frames).await {
                    Ok(()) => true,
                    Err(err) => emit_service_error(self.event_tx.clone(), err).await,
                };
            }
            TelephonyControl::SendOpusFrames { profile, frames } => {
                return match self.send_opus_frames(profile, frames).await {
                    Ok(()) => true,
                    Err(err) => emit_service_error(self.event_tx.clone(), err).await,
                };
            }
            TelephonyControl::StartOpusStream { profile, frames } => {
                return match self.start_opus_stream(profile, frames).await {
                    Ok(()) => true,
                    Err(err) => emit_service_error(self.event_tx.clone(), err).await,
                };
            }
            TelephonyControl::StopOpusStream => {
                return self
                    .stop_opus_stream(OpusTransmitStreamStopReason::Requested)
                    .await;
            }
            TelephonyControl::StartOpusReceiveStream { frames } => {
                return match self.start_opus_receive_stream(frames).await {
                    Ok(()) => true,
                    Err(err) => emit_service_error(self.event_tx.clone(), err).await,
                };
            }
            TelephonyControl::StopOpusReceiveStream => {
                return self
                    .stop_opus_receive_stream(OpusReceiveStreamStopReason::Requested)
                    .await;
            }
            TelephonyControl::SwitchProfile { profile } => {
                let commands = self.core.switch_active_profile(profile);
                self.control_commands(commands).await
            }
            TelephonyControl::SetExternalBusy(busy) => {
                self.core.set_external_busy(busy);
                let stream_events = self.refresh_active_timeout();
                if !emit_service_events(self.event_tx.clone(), stream_events).await {
                    return false;
                }
                return emit_snapshot(self.event_tx.clone(), self.core.snapshot()).await;
            }
            TelephonyControl::SetAccessPolicy(access) => {
                self.core.set_access_policy(access);
                Ok(())
            }
            TelephonyControl::Shutdown => return false,
        };

        match result {
            Ok(()) => true,
            Err(err) => emit_service_error(self.event_tx.clone(), err).await,
        }
    }

    async fn start_outgoing_discovery(
        &mut self,
        remote_identity: IdentityHash,
        profile: Option<Profile>,
        discovery_timeout: Duration,
    ) -> bool {
        if self.core.line_busy() || self.outgoing_discovery.is_some() {
            return emit_service_error(self.event_tx.clone(), Error::LineBusy).await;
        }

        self.outgoing_discovery = Some(OutgoingDiscoveryState {
            remote_identity,
            profile,
        });

        if !emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::OutgoingCallPending { remote_identity },
        )
        .await
        {
            return false;
        }

        let transport_tx = self.endpoint.transport_tx.clone();
        let internal_tx = self.internal_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let peer = discover_remote_telephony_peer_on_transport(
                    transport_tx.clone(),
                    remote_identity,
                    discovery_timeout,
                )
                .await?;
                await_path_on_transport(
                    transport_tx.clone(),
                    peer.destination_hash,
                    discovery_timeout,
                )
                .await?;
                let peer = refresh_peer_hops_from_transport(transport_tx, peer).await?;
                Ok(peer)
            }
            .await;

            let _ = internal_tx
                .send(TelephonyInternalEvent::OutgoingDiscoveryCompleted {
                    remote_identity,
                    profile,
                    result,
                })
                .await;
        });

        true
    }

    async fn handle_internal(&mut self, event: TelephonyInternalEvent) -> bool {
        match event {
            TelephonyInternalEvent::OutgoingDiscoveryCompleted {
                remote_identity,
                profile,
                result,
            } => {
                let expected = OutgoingDiscoveryState {
                    remote_identity,
                    profile,
                };
                if self.outgoing_discovery != Some(expected) {
                    return true;
                }
                self.outgoing_discovery = None;

                match result.and_then(|peer| {
                    self.endpoint.begin_outgoing_link_with_remote_pubkey(
                        &mut self.core,
                        remote_identity,
                        peer.public_key,
                        profile,
                        peer.hops,
                    )
                }) {
                    Ok(link_id) => {
                        let stream_events = self.refresh_active_timeout();
                        if !emit_service_events(self.event_tx.clone(), stream_events).await {
                            return false;
                        }
                        if !emit_service_event(
                            self.event_tx.clone(),
                            TelephonyServiceEvent::OutgoingCallStarted {
                                link_id,
                                remote_identity,
                            },
                        )
                        .await
                        {
                            return false;
                        }
                        emit_snapshot(self.event_tx.clone(), self.core.snapshot()).await
                    }
                    Err(err) => {
                        let message = err.to_string();
                        emit_service_event(
                            self.event_tx.clone(),
                            TelephonyServiceEvent::OutgoingCallFailed {
                                remote_identity,
                                message,
                            },
                        )
                        .await
                    }
                }
            }
        }
    }

    async fn start_opus_receive_stream(
        &mut self,
        frames_tx: mpsc::Sender<RawAudioFrame>,
    ) -> Result<(), Error> {
        let active = self.core.active_call().ok_or(Error::NoActiveCall)?;
        if active.call.status() != SignallingStatus::Established {
            return Err(Error::CallNotEstablished);
        }

        let link_id = active.link_id;
        let profile = active.call.profile().unwrap_or(Profile::DEFAULT);
        self.stop_opus_receive_stream(OpusReceiveStreamStopReason::Replaced)
            .await;
        self.media.opus_receive_stream = Some(ActiveOpusReceiveStream {
            link_id,
            profile,
            frames_tx,
        });

        if emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::OpusReceiveStreamStarted { link_id, profile },
        )
        .await
        {
            Ok(())
        } else {
            Err(Error::ServiceEventClosed)
        }
    }

    async fn stop_opus_receive_stream(&mut self, reason: OpusReceiveStreamStopReason) -> bool {
        let Some(stream) = self.media.opus_receive_stream.take() else {
            return true;
        };
        emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::OpusReceiveStreamStopped {
                link_id: stream.link_id,
                profile: stream.profile,
                reason,
            },
        )
        .await
    }

    async fn start_opus_stream(
        &mut self,
        profile: Profile,
        frames_rx: mpsc::Receiver<RawAudioFrame>,
    ) -> Result<(), Error> {
        let active = self.core.active_call().ok_or(Error::NoActiveCall)?;
        if active.call.status() != SignallingStatus::Established {
            return Err(Error::CallNotEstablished);
        }
        let active_profile = active.call.profile().unwrap_or(Profile::DEFAULT);
        if profile != active_profile {
            return Err(Error::MediaProfileMismatch {
                active: active_profile,
                requested: profile,
            });
        }

        let link_id = active.link_id;
        self.stop_opus_stream(OpusTransmitStreamStopReason::Replaced)
            .await;
        self.media.opus_transmit_stream = Some(ActiveOpusTransmitStream {
            link_id,
            profile,
            frames_rx,
        });

        if emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::OpusTransmitStreamStarted { link_id, profile },
        )
        .await
        {
            Ok(())
        } else {
            Err(Error::ServiceEventClosed)
        }
    }

    async fn stop_opus_stream(&mut self, reason: OpusTransmitStreamStopReason) -> bool {
        let Some(stream) = self.media.opus_transmit_stream.take() else {
            return true;
        };
        emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::OpusTransmitStreamStopped {
                link_id: stream.link_id,
                profile: stream.profile,
                reason,
            },
        )
        .await
    }

    async fn pump_opus_stream(&mut self) -> bool {
        let Some((profile, frames, source_closed)) = self.drain_opus_stream_frames() else {
            return true;
        };

        if !frames.is_empty()
            && let Err(err) = self.send_opus_frames(profile, frames).await
        {
            self.media.opus_transmit_stream = None;
            return emit_service_error(self.event_tx.clone(), err).await;
        }

        if source_closed {
            self.stop_opus_stream(OpusTransmitStreamStopReason::SourceClosed)
                .await
        } else {
            true
        }
    }

    fn drain_opus_stream_frames(&mut self) -> Option<(Profile, Vec<RawAudioFrame>, bool)> {
        let stream = self.media.opus_transmit_stream.as_mut()?;
        let mut frames = Vec::new();
        let mut source_closed = false;
        let max_frames = self.config.media_frames_per_tick.max(1);

        for _ in 0..max_frames {
            match stream.frames_rx.try_recv() {
                Ok(frame) => frames.push(frame),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    source_closed = true;
                    break;
                }
            }
        }

        if frames.is_empty() && !source_closed {
            None
        } else {
            Some((stream.profile, frames, source_closed))
        }
    }

    async fn control_commands(
        &mut self,
        commands: Result<Vec<TelephonyCommand>, Error>,
    ) -> Result<(), Error> {
        let commands = commands?;
        let effects = self.endpoint.execute_commands(&commands)?;
        let service_events = service_events_from_commands(&commands);
        let step = TelephonyStep::commands(commands);
        let stream_events = self.refresh_active_timeout();
        if !emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::Drive(TelephonyDriveStep { step, effects }),
        )
        .await
        {
            return Err(Error::ServiceEventClosed);
        }

        for event in service_events {
            if !emit_service_event(self.event_tx.clone(), event).await {
                return Err(Error::ServiceEventClosed);
            }
        }
        for event in stream_events {
            if !emit_service_event(self.event_tx.clone(), event).await {
                return Err(Error::ServiceEventClosed);
            }
        }

        if emit_snapshot(self.event_tx.clone(), self.core.snapshot()).await {
            Ok(())
        } else {
            Err(Error::ServiceEventClosed)
        }
    }

    async fn send_raw_frames(
        &mut self,
        bit_depth: RawBitDepth,
        frames: Vec<RawAudioFrame>,
    ) -> Result<(), Error> {
        let active = self.core.active_call().ok_or(Error::NoActiveCall)?;
        if active.call.status() != SignallingStatus::Established {
            return Err(Error::CallNotEstablished);
        }

        let link_id = active.link_id;
        let frame_count = frames.len();
        let packet_count = self.endpoint.queue_raw_frames(link_id, bit_depth, frames)?;
        if emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::MediaSent {
                link_id,
                frames: frame_count,
                packets: packet_count,
            },
        )
        .await
        {
            Ok(())
        } else {
            Err(Error::ServiceEventClosed)
        }
    }

    async fn send_opus_frames(
        &mut self,
        profile: Profile,
        frames: Vec<RawAudioFrame>,
    ) -> Result<(), Error> {
        let active = self.core.active_call().ok_or(Error::NoActiveCall)?;
        if active.call.status() != SignallingStatus::Established {
            return Err(Error::CallNotEstablished);
        }
        let active_profile = active.call.profile().unwrap_or(Profile::DEFAULT);
        if profile != active_profile {
            return Err(Error::MediaProfileMismatch {
                active: active_profile,
                requested: profile,
            });
        }

        let link_id = active.link_id;
        let frame_count = frames.len();
        let encoded = {
            let encoder = self.media.opus_encoder_for(link_id, profile)?;
            frames
                .iter()
                .map(|frame| encoder.encode_frame(frame))
                .collect::<Result<Vec<_>, _>>()?
        };
        let packet_count = self.endpoint.queue_frames(link_id, encoded)?;
        if emit_service_event(
            self.event_tx.clone(),
            TelephonyServiceEvent::MediaSent {
                link_id,
                frames: frame_count,
                packets: packet_count,
            },
        )
        .await
        {
            Ok(())
        } else {
            Err(Error::ServiceEventClosed)
        }
    }

    async fn drive_ready(&mut self) -> bool {
        match self.endpoint.try_drive_ready(&mut self.core) {
            Ok(steps) => {
                let emit_snapshot_after_steps = !steps.is_empty();
                for step in steps {
                    let service_events = service_events_from_commands(&step.step.commands);
                    let media_received = media_received_event(&step.step);
                    let opus_received_events = match self.opus_received_events(&step.step) {
                        Ok(events) => events,
                        Err(err) => {
                            if !emit_service_error(self.event_tx.clone(), err).await {
                                return false;
                            }
                            Vec::new()
                        }
                    };
                    if !emit_service_event(
                        self.event_tx.clone(),
                        TelephonyServiceEvent::Drive(step),
                    )
                    .await
                    {
                        return false;
                    }
                    for event in service_events {
                        if !emit_service_event(self.event_tx.clone(), event).await {
                            return false;
                        }
                    }
                    if let Some(event) = media_received
                        && !emit_service_event(self.event_tx.clone(), event).await
                    {
                        return false;
                    }
                    for event in opus_received_events {
                        if !emit_service_event(self.event_tx.clone(), event).await {
                            return false;
                        }
                    }
                }
                if emit_snapshot_after_steps {
                    let stream_events = self.refresh_active_timeout();
                    for event in stream_events {
                        if !emit_service_event(self.event_tx.clone(), event).await {
                            return false;
                        }
                    }
                }
                if emit_snapshot_after_steps
                    && !emit_snapshot(self.event_tx.clone(), self.core.snapshot()).await
                {
                    return false;
                }
                true
            }
            Err(err) => emit_service_error(self.event_tx.clone(), err).await,
        }
    }

    fn opus_received_events(
        &mut self,
        step: &TelephonyStep,
    ) -> Result<Vec<TelephonyServiceEvent>, Error> {
        let Some(inbound) = step.inbound.as_ref() else {
            return Ok(Vec::new());
        };
        let opus_frames = inbound
            .frame_events
            .iter()
            .filter_map(|event| match event {
                FrameStreamEvent::Frame(frame) if frame.codec == CodecKind::Opus => Some(frame),
                _ => None,
            })
            .collect::<Vec<_>>();
        if opus_frames.is_empty() {
            return Ok(Vec::new());
        }

        let active = self.core.active_call().ok_or(Error::NoActiveCall)?;
        if active.link_id != inbound.link_id {
            return Err(Error::WrongActiveLink);
        }
        let link_id = active.link_id;
        let profile = active.call.profile().unwrap_or(Profile::DEFAULT);
        let decoded = {
            let decoder = self.media.opus_decoder_for(active.link_id, profile)?;
            opus_frames
                .iter()
                .map(|frame| decoder.decode_frame(frame))
                .collect::<Result<Vec<_>, _>>()?
        };

        let receive_stream_events = self.deliver_opus_receive_stream(link_id, profile, &decoded);
        let mut events = Vec::with_capacity(1 + receive_stream_events.len());
        events.push(TelephonyServiceEvent::OpusFramesReceived {
            link_id,
            profile,
            frames: decoded,
        });
        events.extend(receive_stream_events);
        Ok(events)
    }

    fn deliver_opus_receive_stream(
        &mut self,
        link_id: LinkId,
        profile: Profile,
        frames: &[RawAudioFrame],
    ) -> Vec<TelephonyServiceEvent> {
        let Some(stream) = self.media.opus_receive_stream.as_ref() else {
            return Vec::new();
        };
        if stream.link_id != link_id || stream.profile != profile {
            self.media.opus_receive_stream = None;
            return Vec::new();
        }

        let mut delivered = 0;
        let mut dropped = 0;
        let mut sink_closed = false;
        for frame in frames {
            let Some(stream) = self.media.opus_receive_stream.as_ref() else {
                break;
            };
            match stream.frames_tx.try_send(frame.clone()) {
                Ok(()) => delivered += 1,
                Err(mpsc::error::TrySendError::Full(_)) => dropped += 1,
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    sink_closed = true;
                    break;
                }
            }
        }

        let mut events = Vec::new();
        if delivered > 0 || dropped > 0 {
            events.push(TelephonyServiceEvent::OpusReceiveStreamFrames {
                link_id,
                profile,
                frames: delivered,
                dropped,
            });
        }
        if sink_closed && let Some(stream) = self.media.opus_receive_stream.take() {
            events.push(TelephonyServiceEvent::OpusReceiveStreamStopped {
                link_id: stream.link_id,
                profile: stream.profile,
                reason: OpusReceiveStreamStopReason::SinkClosed,
            });
        }
        events
    }

    fn refresh_active_timeout(&mut self) -> Vec<TelephonyServiceEvent> {
        let snapshot = self.core.snapshot();
        let stream_events = self
            .media
            .clear_unless_established(snapshot.active_call.as_ref());
        let Some(active) = snapshot.active_call else {
            self.active_timeout = None;
            return stream_events;
        };

        let desired = match (active.role, active.status) {
            (CallRole::Incoming, SignallingStatus::Ringing) => self
                .config
                .incoming_ring_timeout
                .map(|duration| (TelephonyServiceTimeoutKind::IncomingRing, duration)),
            (CallRole::Outgoing, status) if status != SignallingStatus::Established => self
                .config
                .outgoing_call_timeout
                .map(|duration| (TelephonyServiceTimeoutKind::OutgoingCall, duration)),
            _ => None,
        };

        let Some((kind, duration)) = desired else {
            self.active_timeout = None;
            return stream_events;
        };

        if self
            .active_timeout
            .is_some_and(|timeout| timeout.link_id == active.link_id && timeout.kind == kind)
        {
            return stream_events;
        }

        self.active_timeout = Some(TelephonyServiceTimeout {
            link_id: active.link_id,
            kind,
            expires_at: Instant::now() + duration,
        });
        stream_events
    }

    async fn handle_due_timeout(&mut self) -> bool {
        let Some(timeout_state) = self.active_timeout else {
            return true;
        };
        if Instant::now() < timeout_state.expires_at {
            return true;
        }

        let Some(active) = self.core.snapshot().active_call else {
            self.active_timeout = None;
            return true;
        };
        if active.link_id != timeout_state.link_id {
            let stream_events = self.refresh_active_timeout();
            if !emit_service_events(self.event_tx.clone(), stream_events).await {
                return false;
            }
            return true;
        }

        let should_timeout = match timeout_state.kind {
            TelephonyServiceTimeoutKind::IncomingRing => {
                active.role == CallRole::Incoming && active.status == SignallingStatus::Ringing
            }
            TelephonyServiceTimeoutKind::OutgoingCall => {
                active.role == CallRole::Outgoing && active.status != SignallingStatus::Established
            }
        };
        if !should_timeout {
            let stream_events = self.refresh_active_timeout();
            if !emit_service_events(self.event_tx.clone(), stream_events).await {
                return false;
            }
            return true;
        }

        self.active_timeout = None;
        let commands = self
            .core
            .hangup_active(timeout_state.kind == TelephonyServiceTimeoutKind::IncomingRing);
        match self.control_commands(commands).await {
            Ok(()) => true,
            Err(err) => emit_service_error(self.event_tx.clone(), err).await,
        }
    }
}

fn service_events_from_commands(commands: &[TelephonyCommand]) -> Vec<TelephonyServiceEvent> {
    commands
        .iter()
        .filter_map(|command| match command {
            TelephonyCommand::RingIncomingCall {
                link_id,
                remote_identity,
            } => Some(TelephonyServiceEvent::IncomingCall {
                link_id: *link_id,
                remote_identity: *remote_identity,
            }),
            TelephonyCommand::CallTerminated { link_id, reason } => {
                Some(TelephonyServiceEvent::CallTerminated {
                    link_id: *link_id,
                    reason: *reason,
                })
            }
            _ => None,
        })
        .collect()
}

fn media_received_event(step: &TelephonyStep) -> Option<TelephonyServiceEvent> {
    let inbound = step.inbound.as_ref()?;
    let frames = inbound
        .frame_events
        .iter()
        .filter(|event| matches!(event, FrameStreamEvent::Frame(_)))
        .count();
    (frames > 0).then_some(TelephonyServiceEvent::MediaReceived {
        link_id: inbound.link_id,
        frames,
    })
}

async fn emit_service_error(event_tx: mpsc::Sender<TelephonyServiceEvent>, err: Error) -> bool {
    emit_service_event(
        event_tx,
        TelephonyServiceEvent::Error {
            message: err.to_string(),
        },
    )
    .await
}

async fn emit_snapshot(
    event_tx: mpsc::Sender<TelephonyServiceEvent>,
    snapshot: TelephonyRuntimeSnapshot,
) -> bool {
    emit_service_event(event_tx, TelephonyServiceEvent::Snapshot(snapshot)).await
}

async fn emit_service_events(
    event_tx: mpsc::Sender<TelephonyServiceEvent>,
    events: Vec<TelephonyServiceEvent>,
) -> bool {
    for event in events {
        if !emit_service_event(event_tx.clone(), event).await {
            return false;
        }
    }
    true
}

async fn emit_service_event(
    event_tx: mpsc::Sender<TelephonyServiceEvent>,
    event: TelephonyServiceEvent,
) -> bool {
    event_tx.send(event).await.is_ok()
}

pub struct TelephonyRnsEndpoint {
    pub destination_hash: [u8; 16],
    pub manager: LinkManager,
    pub link_established_rx: mpsc::Receiver<LinkId>,
    pub link_identified_rx: mpsc::Receiver<(LinkId, IdentityHash)>,
    pub link_packet_rx: mpsc::Receiver<(Vec<u8>, LinkId)>,
    pub link_closed_rx: mpsc::Receiver<LinkId>,
    transport_tx: mpsc::Sender<TransportMessage>,
    identity_pub_key: [u8; 64],
    identity_signing_key: Ed25519PrivateKey,
    outgoing_attempts: HashMap<LinkId, OutgoingLinkAttempt>,
    outgoing_links: HashMap<LinkId, OutgoingLinkState>,
}

struct OutgoingLinkAttempt {
    link: Link,
    remote_public_key: [u8; 64],
    event_rx: mpsc::Receiver<DestinationEvent>,
}

struct OutgoingLinkState {
    link: Link,
    event_rx: mpsc::Receiver<DestinationEvent>,
}

impl TelephonyRnsEndpoint {
    pub fn register(
        transport_tx: mpsc::Sender<TransportMessage>,
        identity: &Identity,
    ) -> Result<Self, Error> {
        let manager_signing_key = identity.get_signing_key().ok_or(Error::NoSigningKey)?;
        let identity_signing_key = identity.get_signing_key().ok_or(Error::NoSigningKey)?;
        let identity_pub_key = identity.get_public_key();
        let destination_hash = telephony_destination_hash(&identity.hash);
        let (destination_tx, destination_rx) = mpsc::channel::<DestinationEvent>(256);

        try_send_transport(
            &transport_tx,
            TransportMessage::RegisterDestination {
                hash: destination_hash,
                app_name: TELEPHONY_DESTINATION_NAME.to_string(),
                delivery_tx: Some(destination_tx),
            },
        )?;

        let mut manager = LinkManager::with_destination(
            transport_tx.clone(),
            destination_rx,
            identity,
            TELEPHONY_DESTINATION_NAME,
            Some(manager_signing_key),
        );
        let (established_tx, link_established_rx) = mpsc::channel(64);
        let (identified_tx, link_identified_rx) = mpsc::channel(64);
        let (packet_tx, link_packet_rx) = mpsc::channel(256);
        let (closed_tx, link_closed_rx) = mpsc::channel(64);
        manager.set_link_established_channel(established_tx);
        manager.set_link_identified_channel(identified_tx);
        manager.set_link_packet_channel(packet_tx);
        manager.set_link_closed_channel(closed_tx);

        Ok(Self {
            destination_hash,
            manager,
            link_established_rx,
            link_identified_rx,
            link_packet_rx,
            link_closed_rx,
            transport_tx,
            identity_pub_key,
            identity_signing_key,
            outgoing_attempts: HashMap::new(),
            outgoing_links: HashMap::new(),
        })
    }

    pub fn request_path_to_identity(&self, identity_hash: &IdentityHash) -> Result<(), Error> {
        try_send_transport(
            &self.transport_tx,
            TransportMessage::RequestPath {
                destination_hash: telephony_destination_hash(identity_hash),
            },
        )
    }

    pub fn deregister_destination(&self) -> Result<(), Error> {
        try_send_transport(
            &self.transport_tx,
            TransportMessage::DeregisterDestination {
                hash: self.destination_hash,
            },
        )
    }

    pub fn announce(&self) -> Result<(), Error> {
        let raw = self.announce_packet();
        try_send_transport(
            &self.transport_tx,
            TransportMessage::Outbound(OutboundRequest {
                raw: Bytes::from(raw),
                destination_hash: self.destination_hash,
            }),
        )
    }

    fn announce_packet(&self) -> Vec<u8> {
        let announce_name_hash = name_hash(TELEPHONY_DESTINATION_NAME);
        let random_bytes = rns_crypto::random::random_bytes(5);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let timestamp_bytes = timestamp.to_be_bytes();
        let mut random_hash = [0u8; 10];
        random_hash[..5].copy_from_slice(&random_bytes);
        random_hash[5..].copy_from_slice(&timestamp_bytes[3..8]);

        let mut signed_data = Vec::with_capacity(16 + 64 + 10 + 10);
        signed_data.extend_from_slice(&self.destination_hash);
        signed_data.extend_from_slice(&self.identity_pub_key);
        signed_data.extend_from_slice(&announce_name_hash);
        signed_data.extend_from_slice(&random_hash);
        let signature = self.identity_signing_key.sign(&signed_data);

        let mut announce_data = Vec::with_capacity(64 + 10 + 10 + 64);
        announce_data.extend_from_slice(&self.identity_pub_key);
        announce_data.extend_from_slice(&announce_name_hash);
        announce_data.extend_from_slice(&random_hash);
        announce_data.extend_from_slice(&signature);

        let flags = rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Single,
            packet_type: rns_wire::flags::PacketType::Announce,
        };
        let header = rns_wire::header::PacketHeader {
            flags,
            hops: 0,
            transport_id: None,
            destination_hash: self.destination_hash,
            context: rns_wire::context::PacketContext::None,
        };
        let mut raw = header.pack();
        raw.extend_from_slice(&announce_data);
        raw
    }

    pub fn deregister_link_destination(&self, link_id: LinkId) -> Result<(), Error> {
        try_send_transport(
            &self.transport_tx,
            TransportMessage::DeregisterDestination { hash: link_id },
        )
    }

    pub fn discover_remote_telephony_peer(
        &self,
        remote_identity: IdentityHash,
        discovery_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<RemoteTelephonyPeer, Error>> + Send + 'static
    {
        let transport_tx = self.transport_tx.clone();
        async move {
            discover_remote_telephony_peer_on_transport(
                transport_tx,
                remote_identity,
                discovery_timeout,
            )
            .await
        }
    }

    pub fn await_path_to_identity(
        &self,
        remote_identity: IdentityHash,
        path_timeout: Duration,
    ) -> impl std::future::Future<Output = Result<(), Error>> + Send + 'static {
        let transport_tx = self.transport_tx.clone();
        async move {
            await_path_on_transport(
                transport_tx,
                telephony_destination_hash(&remote_identity),
                path_timeout,
            )
            .await
        }
    }

    pub async fn begin_outgoing_link(
        &mut self,
        core: &mut TelephonyRuntimeCore,
        remote_identity: IdentityHash,
        profile: Option<Profile>,
        discovery_timeout: Duration,
    ) -> Result<LinkId, Error> {
        let peer = discover_remote_telephony_peer_on_transport(
            self.transport_tx.clone(),
            remote_identity,
            discovery_timeout,
        )
        .await?;
        await_path_on_transport(
            self.transport_tx.clone(),
            peer.destination_hash,
            discovery_timeout,
        )
        .await?;
        let peer = refresh_peer_hops_from_transport(self.transport_tx.clone(), peer).await?;
        self.begin_outgoing_link_with_remote_pubkey(
            core,
            remote_identity,
            peer.public_key,
            profile,
            peer.hops,
        )
    }

    pub fn begin_outgoing_link_with_remote_pubkey(
        &mut self,
        core: &mut TelephonyRuntimeCore,
        remote_identity: IdentityHash,
        remote_public_key: [u8; 64],
        profile: Option<Profile>,
        hops: u8,
    ) -> Result<LinkId, Error> {
        let destination_hash = telephony_destination_hash(&remote_identity);
        self.request_path_to_identity(&remote_identity)?;

        let (link, request_data) = Link::new_initiator(destination_hash, hops.max(1));
        let link_id = link.link_id;
        core.start_outgoing_call(link_id, remote_identity, profile)?;

        let (event_tx, event_rx) = mpsc::channel(128);
        try_send_transport(
            &self.transport_tx,
            TransportMessage::RegisterDestination {
                hash: link_id,
                app_name: TELEPHONY_DESTINATION_NAME.to_string(),
                delivery_tx: Some(event_tx),
            },
        )?;

        try_send_transport(
            &self.transport_tx,
            TransportMessage::Outbound(OutboundRequest {
                raw: build_link_request_packet(destination_hash, &request_data),
                destination_hash,
            }),
        )?;

        self.outgoing_attempts.insert(
            link_id,
            OutgoingLinkAttempt {
                link,
                remote_public_key,
                event_rx,
            },
        );

        Ok(link_id)
    }

    pub fn try_recv_link_event(&mut self) -> Result<Option<TelephonyLinkEvent>, Error> {
        if let Ok(link_id) = self.link_established_rx.try_recv() {
            return Ok(Some(TelephonyLinkEvent::LinkEstablished { link_id }));
        }
        if let Ok((link_id, remote_identity)) = self.link_identified_rx.try_recv() {
            return Ok(Some(TelephonyLinkEvent::RemoteIdentified {
                link_id,
                remote_identity,
            }));
        }
        if let Ok((plaintext, link_id)) = self.link_packet_rx.try_recv() {
            return Ok(Some(TelephonyLinkEvent::LinkPacket { link_id, plaintext }));
        }
        if let Ok(link_id) = self.link_closed_rx.try_recv() {
            return Ok(Some(TelephonyLinkEvent::LinkClosed { link_id }));
        }
        self.try_recv_outgoing_event()
    }

    pub async fn recv_link_event(&mut self) -> Result<Option<TelephonyLinkEvent>, Error> {
        if let Some(event) = self.try_recv_outgoing_event()? {
            return Ok(Some(event));
        }

        tokio::select! {
            event = self.link_established_rx.recv() => {
                Ok(event.map(|link_id| TelephonyLinkEvent::LinkEstablished { link_id }))
            }
            event = self.link_identified_rx.recv() => {
                Ok(event.map(|(link_id, remote_identity)| TelephonyLinkEvent::RemoteIdentified {
                    link_id,
                    remote_identity,
                }))
            }
            event = self.link_packet_rx.recv() => {
                Ok(event.map(|(plaintext, link_id)| TelephonyLinkEvent::LinkPacket {
                    link_id,
                    plaintext,
                }))
            }
            event = self.link_closed_rx.recv() => {
                Ok(event.map(|link_id| TelephonyLinkEvent::LinkClosed { link_id }))
            }
        }
    }

    pub fn try_step(
        &mut self,
        core: &mut TelephonyRuntimeCore,
    ) -> Result<Option<TelephonyStep>, Error> {
        self.try_recv_link_event()?
            .map(|event| core.accept_link_event(event))
            .transpose()
    }

    pub async fn step(
        &mut self,
        core: &mut TelephonyRuntimeCore,
    ) -> Result<Option<TelephonyStep>, Error> {
        self.recv_link_event()
            .await?
            .map(|event| core.accept_link_event(event))
            .transpose()
    }

    pub fn try_drive_once(
        &mut self,
        core: &mut TelephonyRuntimeCore,
    ) -> Result<Option<TelephonyDriveStep>, Error> {
        let Some(step) = self.try_step(core)? else {
            return Ok(None);
        };
        let effects = self.execute_commands(&step.commands)?;
        Ok(Some(TelephonyDriveStep { step, effects }))
    }

    pub async fn drive_once(
        &mut self,
        core: &mut TelephonyRuntimeCore,
    ) -> Result<Option<TelephonyDriveStep>, Error> {
        let Some(step) = self.step(core).await? else {
            return Ok(None);
        };
        let effects = self.execute_commands(&step.commands)?;
        Ok(Some(TelephonyDriveStep { step, effects }))
    }

    pub fn try_pump_reticulum(&mut self) -> bool {
        self.manager.try_step()
    }

    pub fn tick_reticulum(&mut self) {
        self.manager.tick();
    }

    pub fn try_drive_ready(
        &mut self,
        core: &mut TelephonyRuntimeCore,
    ) -> Result<Vec<TelephonyDriveStep>, Error> {
        let mut driven = Vec::new();

        loop {
            let mut progressed = false;
            while self.try_pump_reticulum() {
                progressed = true;
            }
            while let Some(step) = self.try_drive_once(core)? {
                driven.push(step);
                progressed = true;
            }
            if !progressed {
                return Ok(driven);
            }
        }
    }

    pub fn queue_raw_frames(
        &mut self,
        link_id: LinkId,
        bit_depth: RawBitDepth,
        frames: impl IntoIterator<Item = RawAudioFrame>,
    ) -> Result<usize, Error> {
        let frames = frames.into_iter().collect::<Vec<_>>();
        let packets = if let Some(link) = self.manager.get_link_mut(&link_id) {
            LxstMediaEgress::PYTHON_COMPATIBLE.pack_raw_frames(link, bit_depth, frames)?
        } else {
            let link = self
                .outgoing_links
                .get_mut(&link_id)
                .map(|state| &mut state.link)
                .ok_or(Error::UnknownLink)?;
            LxstMediaEgress::PYTHON_COMPATIBLE.pack_raw_frames(link, bit_depth, frames)?
        };
        self.queue_packed_media_packets(packets)
    }

    pub fn queue_frames(
        &mut self,
        link_id: LinkId,
        frames: impl IntoIterator<Item = Frame>,
    ) -> Result<usize, Error> {
        let frames = frames.into_iter().collect::<Vec<_>>();
        let packets = if let Some(link) = self.manager.get_link_mut(&link_id) {
            LxstMediaEgress::PYTHON_COMPATIBLE.pack_frames(link, frames)?
        } else {
            let link = self
                .outgoing_links
                .get_mut(&link_id)
                .map(|state| &mut state.link)
                .ok_or(Error::UnknownLink)?;
            LxstMediaEgress::PYTHON_COMPATIBLE.pack_frames(link, frames)?
        };
        self.queue_packed_media_packets(packets)
    }

    fn queue_packed_media_packets(
        &mut self,
        packets: Vec<lxst_rns::PackedLinkPacket>,
    ) -> Result<usize, Error> {
        let packet_count = packets.len();

        for packet in packets {
            try_send_transport(
                &self.transport_tx,
                TransportMessage::Outbound(OutboundRequest {
                    raw: packet.raw,
                    destination_hash: packet.destination_hash,
                }),
            )?;
        }

        Ok(packet_count)
    }

    pub fn execute_command(
        &mut self,
        command: &TelephonyCommand,
    ) -> Result<TelephonyCommandEffect, Error> {
        if !matches!(
            command,
            TelephonyCommand::SendSignal { .. }
                | TelephonyCommand::IdentifyLocalIdentity { .. }
                | TelephonyCommand::TeardownLink { .. }
        ) {
            return Ok(TelephonyCommandEffect::Noop);
        }

        let link_id = command.link_id();
        if let Some(link) = self.manager.get_link_mut(&link_id) {
            return execute_command_with_link(
                &self.transport_tx,
                &self.identity_pub_key,
                &self.identity_signing_key,
                link,
                command,
            );
        }

        if matches!(command, TelephonyCommand::TeardownLink { .. })
            && self.outgoing_attempts.remove(&link_id).is_some()
        {
            self.deregister_link_destination(link_id)?;
            return Ok(TelephonyCommandEffect::Noop);
        }

        let effect = {
            let link = self
                .outgoing_links
                .get_mut(&link_id)
                .map(|state| &mut state.link)
                .ok_or(Error::UnknownLink)?;
            execute_command_with_link(
                &self.transport_tx,
                &self.identity_pub_key,
                &self.identity_signing_key,
                link,
                command,
            )?
        };

        if matches!(command, TelephonyCommand::TeardownLink { .. }) {
            self.outgoing_links.remove(&link_id);
            self.deregister_link_destination(link_id)?;
        }

        Ok(effect)
    }

    pub fn execute_commands(
        &mut self,
        commands: &[TelephonyCommand],
    ) -> Result<Vec<TelephonyCommandEffect>, Error> {
        commands
            .iter()
            .map(|command| self.execute_command(command))
            .collect()
    }

    fn try_recv_outgoing_event(&mut self) -> Result<Option<TelephonyLinkEvent>, Error> {
        let attempt_ids = self.outgoing_attempts.keys().copied().collect::<Vec<_>>();
        for link_id in attempt_ids {
            let Some(attempt) = self.outgoing_attempts.get_mut(&link_id) else {
                continue;
            };
            match attempt.event_rx.try_recv() {
                Ok(event) => {
                    if let Some(event) = self.handle_outgoing_attempt_event(link_id, event)? {
                        return Ok(Some(event));
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.outgoing_attempts.remove(&link_id);
                    return Err(Error::LinkDestinationClosed);
                }
            }
        }

        let active_ids = self.outgoing_links.keys().copied().collect::<Vec<_>>();
        for link_id in active_ids {
            let Some(state) = self.outgoing_links.get_mut(&link_id) else {
                continue;
            };
            match state.event_rx.try_recv() {
                Ok(event) => {
                    return self.handle_outgoing_active_event(link_id, event);
                }
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.outgoing_links.remove(&link_id);
                    return Ok(Some(TelephonyLinkEvent::LinkClosed { link_id }));
                }
            }
        }

        Ok(None)
    }

    fn handle_outgoing_attempt_event(
        &mut self,
        link_id: LinkId,
        event: DestinationEvent,
    ) -> Result<Option<TelephonyLinkEvent>, Error> {
        let raw = match event {
            DestinationEvent::LinkClosed { link_id: closed_id } if closed_id == link_id => {
                self.outgoing_attempts.remove(&link_id);
                self.deregister_link_destination(link_id)?;
                return Ok(Some(TelephonyLinkEvent::LinkClosed { link_id }));
            }
            DestinationEvent::InboundPacket { raw, .. } => raw,
            DestinationEvent::AnnounceRequested(_)
            | DestinationEvent::DeliveryProof { .. }
            | DestinationEvent::LinkEstablished { .. }
            | DestinationEvent::LinkRequest { .. }
            | DestinationEvent::LinkClosed { .. } => return Ok(None),
        };
        let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&raw)
            .map_err(|err| Error::LinkOperation(err.to_string()))?;
        if header.destination_hash != link_id
            || header.flags.packet_type != rns_wire::flags::PacketType::Proof
            || raw.len() <= data_offset
        {
            return Ok(None);
        }

        let mut attempt = self
            .outgoing_attempts
            .remove(&link_id)
            .ok_or(Error::UnknownLink)?;
        let mut identity_ed25519_pub = [0u8; 32];
        identity_ed25519_pub.copy_from_slice(&attempt.remote_public_key[32..64]);
        let verify_key = Ed25519PublicKey::from_bytes(&identity_ed25519_pub)
            .map_err(|err| Error::LinkProofInvalid(err.to_string()))?;
        let rtt_data = attempt
            .link
            .validate_proof(&raw[data_offset..], &verify_key, &identity_ed25519_pub)
            .map_err(|err| Error::LinkProofInvalid(format!("{err:?}")))?;

        queue_link_context_packet(
            &self.transport_tx,
            link_id,
            rns_wire::context::PacketContext::Lrrtt,
            rtt_data,
        )?;
        self.outgoing_links.insert(
            link_id,
            OutgoingLinkState {
                link: attempt.link,
                event_rx: attempt.event_rx,
            },
        );

        Ok(Some(TelephonyLinkEvent::LinkEstablished { link_id }))
    }

    fn handle_outgoing_active_event(
        &mut self,
        link_id: LinkId,
        event: DestinationEvent,
    ) -> Result<Option<TelephonyLinkEvent>, Error> {
        let raw = match event {
            DestinationEvent::LinkClosed { link_id: closed_id } if closed_id == link_id => {
                self.outgoing_links.remove(&link_id);
                self.deregister_link_destination(link_id)?;
                return Ok(Some(TelephonyLinkEvent::LinkClosed { link_id }));
            }
            DestinationEvent::InboundPacket { raw, .. } => raw,
            DestinationEvent::AnnounceRequested(_)
            | DestinationEvent::DeliveryProof { .. }
            | DestinationEvent::LinkEstablished { .. }
            | DestinationEvent::LinkRequest { .. }
            | DestinationEvent::LinkClosed { .. } => return Ok(None),
        };
        let (header, data_offset) = rns_wire::header::PacketHeader::unpack(&raw)
            .map_err(|err| Error::LinkOperation(err.to_string()))?;
        if header.destination_hash != link_id || raw.len() < data_offset {
            return Ok(None);
        }
        let body = &raw[data_offset..];
        let state = self
            .outgoing_links
            .get_mut(&link_id)
            .ok_or(Error::UnknownLink)?;

        match header.context {
            rns_wire::context::PacketContext::LinkClose => {
                if state.link.receive_teardown(body) {
                    self.outgoing_links.remove(&link_id);
                    self.deregister_link_destination(link_id)?;
                    Ok(Some(TelephonyLinkEvent::LinkClosed { link_id }))
                } else {
                    Ok(None)
                }
            }
            rns_wire::context::PacketContext::None => {
                let plaintext = state
                    .link
                    .decrypt(body)
                    .map_err(|err| Error::LinkOperation(err.to_string()))?;
                Ok(Some(TelephonyLinkEvent::LinkPacket { link_id, plaintext }))
            }
            rns_wire::context::PacketContext::Keepalive => {
                state.link.record_inbound();
                Ok(None)
            }
            _ => Ok(None),
        }
    }
}

pub fn execute_command_with_link(
    transport_tx: &mpsc::Sender<TransportMessage>,
    identity_pub_key: &[u8; 64],
    identity_signing_key: &Ed25519PrivateKey,
    link: &mut Link,
    command: &TelephonyCommand,
) -> Result<TelephonyCommandEffect, Error> {
    match command {
        TelephonyCommand::SendSignal { link_id, signal } => {
            let packet = signalling_packet(*signal);
            queue_lxst_link_packet(transport_tx, link, &packet)?;
            Ok(TelephonyCommandEffect::QueuedLinkPacket {
                link_id: *link_id,
                kind: QueuedLinkPacketKind::LxstData,
            })
        }
        TelephonyCommand::IdentifyLocalIdentity { link_id } => {
            let encrypted = link
                .identify(identity_pub_key, identity_signing_key)
                .map_err(|err| Error::LinkOperation(err.to_string()))?;
            queue_link_context_packet(
                transport_tx,
                *link_id,
                rns_wire::context::PacketContext::LinkIdentify,
                encrypted,
            )?;
            Ok(TelephonyCommandEffect::QueuedLinkPacket {
                link_id: *link_id,
                kind: QueuedLinkPacketKind::LinkIdentify,
            })
        }
        TelephonyCommand::TeardownLink { link_id } => {
            let reason = if link.is_initiator {
                CloseReason::InitiatorClosed
            } else {
                CloseReason::DestinationClosed
            };
            let Some(encrypted) = link.teardown(reason) else {
                return Ok(TelephonyCommandEffect::Noop);
            };
            queue_link_context_packet(
                transport_tx,
                *link_id,
                rns_wire::context::PacketContext::LinkClose,
                encrypted,
            )?;
            Ok(TelephonyCommandEffect::QueuedLinkPacket {
                link_id: *link_id,
                kind: QueuedLinkPacketKind::LinkClose,
            })
        }
        _ => Ok(TelephonyCommandEffect::Noop),
    }
}

fn queue_link_context_packet(
    transport_tx: &mpsc::Sender<TransportMessage>,
    link_id: LinkId,
    context: rns_wire::context::PacketContext,
    encrypted: Vec<u8>,
) -> Result<(), Error> {
    let header = rns_wire::header::PacketHeader {
        flags: rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Link,
            packet_type: rns_wire::flags::PacketType::Data,
        },
        hops: 0,
        transport_id: None,
        destination_hash: link_id,
        context,
    };
    let mut raw = header.pack();
    raw.extend_from_slice(&encrypted);
    try_send_transport(
        transport_tx,
        TransportMessage::Outbound(OutboundRequest {
            raw: Bytes::from(raw),
            destination_hash: link_id,
        }),
    )
}

fn build_link_request_packet(dest_hash: [u8; 16], request_data: &[u8]) -> Bytes {
    let header = rns_wire::header::PacketHeader {
        flags: rns_wire::flags::PacketFlags {
            header_type: rns_wire::flags::HeaderType::Header1,
            context_flag: false,
            transport_type: rns_wire::flags::TransportType::Broadcast,
            destination_type: rns_wire::flags::DestinationType::Single,
            packet_type: rns_wire::flags::PacketType::LinkRequest,
        },
        hops: 0,
        transport_id: None,
        destination_hash: dest_hash,
        context: rns_wire::context::PacketContext::None,
    };
    let mut raw = header.pack();
    raw.extend_from_slice(request_data);
    Bytes::from(raw)
}

async fn discover_remote_telephony_peer_on_transport(
    transport_tx: mpsc::Sender<TransportMessage>,
    remote_identity: IdentityHash,
    discovery_timeout: Duration,
) -> Result<RemoteTelephonyPeer, Error> {
    let destination_hash = telephony_destination_hash(&remote_identity);
    let aspect_filter = TELEPHONY_DESTINATION_NAME.to_string();
    let (announce_tx, mut announce_rx) = mpsc::channel(64);

    send_transport_async(
        transport_tx.clone(),
        TransportMessage::RegisterAnnounceHandler {
            aspect_filter: Some(aspect_filter.clone()),
            receive_path_responses: true,
            callback_tx: announce_tx,
        },
    )
    .await?;

    let result = async {
        if let Some(peer) =
            recent_telephony_peer(transport_tx.clone(), remote_identity, destination_hash).await?
        {
            send_transport_async(
                transport_tx.clone(),
                TransportMessage::RequestPath { destination_hash },
            )
            .await?;
            return Ok(peer);
        }

        drop_path_on_transport(transport_tx.clone(), destination_hash).await?;
        send_transport_async(
            transport_tx.clone(),
            TransportMessage::RequestPath { destination_hash },
        )
        .await?;

        wait_for_telephony_announce(
            &mut announce_rx,
            remote_identity,
            destination_hash,
            discovery_timeout,
        )
        .await
    }
    .await;

    let _ = transport_tx.try_send(TransportMessage::DeregisterAnnounceHandler {
        aspect_filter: Some(aspect_filter),
    });

    result
}

async fn recent_telephony_peer(
    transport_tx: mpsc::Sender<TransportMessage>,
    remote_identity: IdentityHash,
    destination_hash: [u8; 16],
) -> Result<Option<RemoteTelephonyPeer>, Error> {
    let entries = query_recent_announces(transport_tx).await?;
    for entry in entries {
        if entry.dest_hash == destination_hash {
            if entry.public_key.is_none() {
                continue;
            }
            return announce_entry_to_peer(remote_identity, destination_hash, entry).map(Some);
        }
    }
    Ok(None)
}

async fn query_recent_announces(
    transport_tx: mpsc::Sender<TransportMessage>,
) -> Result<Vec<AnnounceRpcEntry>, Error> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    send_transport_async(
        transport_tx,
        TransportMessage::Rpc {
            query: TransportQuery::GetRecentAnnounces,
            response_tx,
        },
    )
    .await?;

    match response_rx.await.map_err(|_| Error::TransportQueryClosed)? {
        TransportQueryResponse::Announces(entries) => Ok(entries),
        TransportQueryResponse::Error(_) => Err(Error::UnexpectedTransportQueryResponse),
        _ => Err(Error::UnexpectedTransportQueryResponse),
    }
}

async fn drop_path_on_transport(
    transport_tx: mpsc::Sender<TransportMessage>,
    destination_hash: [u8; 16],
) -> Result<(), Error> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    send_transport_async(
        transport_tx,
        TransportMessage::Rpc {
            query: TransportQuery::DropPath {
                dest: destination_hash,
            },
            response_tx,
        },
    )
    .await?;

    match response_rx.await.map_err(|_| Error::TransportQueryClosed)? {
        TransportQueryResponse::Ok => Ok(()),
        TransportQueryResponse::Error(_) => Err(Error::UnexpectedTransportQueryResponse),
        _ => Err(Error::UnexpectedTransportQueryResponse),
    }
}

async fn await_path_on_transport(
    transport_tx: mpsc::Sender<TransportMessage>,
    destination_hash: [u8; 16],
    path_timeout: Duration,
) -> Result<(), Error> {
    let (reply, response) = tokio::sync::oneshot::channel();
    send_transport_async(
        transport_tx,
        TransportMessage::AwaitPath {
            dest: destination_hash,
            reply,
        },
    )
    .await?;

    match timeout(path_timeout, response).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) | Ok(Err(_)) | Err(_) => Err(Error::RemotePathNotDiscovered),
    }
}

async fn refresh_peer_hops_from_transport(
    transport_tx: mpsc::Sender<TransportMessage>,
    mut peer: RemoteTelephonyPeer,
) -> Result<RemoteTelephonyPeer, Error> {
    if let Some(hops) = path_hops_on_transport(transport_tx, peer.destination_hash).await? {
        peer.hops = hops;
    }
    Ok(peer)
}

async fn path_hops_on_transport(
    transport_tx: mpsc::Sender<TransportMessage>,
    destination_hash: [u8; 16],
) -> Result<Option<u8>, Error> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    send_transport_async(
        transport_tx,
        TransportMessage::Rpc {
            query: TransportQuery::GetPathTable,
            response_tx,
        },
    )
    .await?;

    match response_rx.await.map_err(|_| Error::TransportQueryClosed)? {
        TransportQueryResponse::PathTable(entries) => Ok(entries
            .into_iter()
            .find(|entry| entry.hash == destination_hash)
            .map(|entry| entry.hops)),
        TransportQueryResponse::Error(_) => Err(Error::UnexpectedTransportQueryResponse),
        _ => Err(Error::UnexpectedTransportQueryResponse),
    }
}

async fn send_transport_async(
    transport_tx: mpsc::Sender<TransportMessage>,
    message: TransportMessage,
) -> Result<(), Error> {
    transport_tx
        .send(message)
        .await
        .map_err(|_| Error::TransportClosed)
}

async fn wait_for_telephony_announce(
    announce_rx: &mut mpsc::Receiver<AnnounceHandlerEvent>,
    remote_identity: IdentityHash,
    destination_hash: [u8; 16],
    discovery_timeout: Duration,
) -> Result<RemoteTelephonyPeer, Error> {
    timeout(discovery_timeout, async {
        while let Some(event) = announce_rx.recv().await {
            if event.destination_hash == destination_hash {
                match announce_event_to_peer(remote_identity, destination_hash, event) {
                    Ok(peer) => return Ok(peer),
                    Err(Error::RemotePublicKeyMissing) => continue,
                    Err(err) => return Err(err),
                }
            }
        }
        Err(Error::RemoteTelephonyPeerNotDiscovered)
    })
    .await
    .map_err(|_| Error::RemoteTelephonyPeerNotDiscovered)?
}

fn announce_event_to_peer(
    remote_identity: IdentityHash,
    destination_hash: [u8; 16],
    event: AnnounceHandlerEvent,
) -> Result<RemoteTelephonyPeer, Error> {
    let public_key = event.public_key.ok_or(Error::RemotePublicKeyMissing)?;
    Ok(RemoteTelephonyPeer {
        identity_hash: remote_identity,
        destination_hash,
        public_key,
        hops: event.hops,
    })
}

fn announce_entry_to_peer(
    remote_identity: IdentityHash,
    destination_hash: [u8; 16],
    entry: AnnounceRpcEntry,
) -> Result<RemoteTelephonyPeer, Error> {
    let public_key = entry.public_key.ok_or(Error::RemotePublicKeyMissing)?;
    Ok(RemoteTelephonyPeer {
        identity_hash: remote_identity,
        destination_hash,
        public_key,
        hops: entry.hops,
    })
}

fn try_send_transport(
    transport_tx: &mpsc::Sender<TransportMessage>,
    message: TransportMessage,
) -> Result<(), Error> {
    transport_tx.try_send(message).map_err(|err| match err {
        mpsc::error::TrySendError::Full(_) => Error::TransportFull,
        mpsc::error::TrySendError::Closed(_) => Error::TransportClosed,
    })
}

pub fn telephony_destination_hash(identity_hash: &IdentityHash) -> [u8; 16] {
    Destination::hash_from_name_and_identity(TELEPHONY_DESTINATION_NAME, Some(identity_hash))
}

pub fn telephony_inbound_destination(identity: &Identity) -> Result<Destination, Error> {
    let mut destination = Destination::new(
        Some(identity),
        Direction::In,
        DestType::Single,
        TELEPHONY_DESTINATION_NAME,
    )?;
    destination.set_proof_strategy(ProofStrategy::ProveNone);
    Ok(destination)
}

pub fn signalling_packet(signal: Signal) -> LxstPacket {
    LxstPacket::signalling([signal])
}

pub fn signalling_packets_from_commands(
    commands: &[TelephonyCommand],
) -> Vec<(LinkId, LxstPacket)> {
    commands
        .iter()
        .filter_map(|command| {
            command
                .to_lxst_packet()
                .map(|packet| (command.link_id(), packet))
        })
        .collect()
}

#[cfg(test)]
mod tests;
