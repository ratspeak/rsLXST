//! Local admission and microphone policy around the trusted call transitions.
//! No extra wire extension, turn token or directional capability is invented.
use crate::{Actions, CallRole, CallState, Profile, Signal, SignallingStatus, TelephonyAction};

const SETUP_MS: u64 = 30_000;
const RING_MS: u64 = 90_000;
const ANSWER_MS: u64 = 10_000;
const PROFILE_MS: u64 = 5_000;
pub const TALK_LIMIT_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProfileSet(u8);
impl ProfileSet {
    pub const CODEC2: Self = Self(0b0000_0110);
    pub const OPUS_MEDIUM: Self = Self(0b0000_1000);
    pub const fn contains(self, profile: Profile) -> bool {
        self.0 & (1 << (profile.wire_value() / 16 - 1)) != 0
    }
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub const fn only(profile: Profile) -> Self {
        Self(1 << (profile.wire_value() / 16 - 1))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionConfig {
    /// The embedder must qualify each selected codec and route before admission.
    pub allowed: ProfileSet,
    pub preferred: Profile,
    pub capture: bool,
    pub playback: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    NoAudioDirection,
    UnsupportedPreferredProfile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Local,
    Remote,
    Rejected,
    Busy,
    Timeout,
    ProfileUnsupported,
    AudioUnavailable,
    RouteLost,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Transition(TelephonyAction),
    Capture(bool),
    FlushMedia,
    Ended(EndReason),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Events {
    items: [Event; 8],
    length: usize,
}
impl Events {
    fn new() -> Self {
        Self {
            items: [const { Event::FlushMedia }; 8],
            length: 0,
        }
    }
    fn push(&mut self, event: Event) {
        assert!(self.length < self.items.len());
        self.items[self.length] = event;
        self.length += 1;
    }
    fn transitions(&mut self, actions: Actions) {
        for action in actions.as_slice() {
            self.push(Event::Transition(action.clone()));
        }
    }
    pub fn as_slice(&self) -> &[Event] {
        &self.items[..self.length]
    }
}

#[derive(Debug)]
pub struct Session {
    call: CallState,
    config: SessionConfig,
    verified: bool,
    linked: bool,
    ready: bool,
    transmitting: bool,
    input_down: bool,
    audio_generation: u32,
    ended: Option<EndReason>,
    deadline: u64,
    talk_started: u64,
    suggested: bool,
    profile_deadline: Option<u64>,
    answer_retry: u64,
    retries: u8,
}
impl Session {
    pub fn new(role: CallRole, config: SessionConfig, now_ms: u64) -> Result<Self, ConfigError> {
        if !config.capture && !config.playback {
            return Err(ConfigError::NoAudioDirection);
        }
        if !config.allowed.contains(config.preferred) {
            return Err(ConfigError::UnsupportedPreferredProfile);
        }
        let mut call = match role {
            CallRole::Incoming => CallState::incoming(),
            CallRole::Outgoing => CallState::outgoing(Some(config.preferred)),
        };
        call.switch_profile(config.preferred);
        Ok(Self {
            call,
            config,
            verified: false,
            linked: false,
            ready: false,
            transmitting: false,
            input_down: false,
            audio_generation: 1,
            ended: None,
            deadline: now_ms.saturating_add(SETUP_MS),
            talk_started: 0,
            suggested: false,
            profile_deadline: None,
            answer_retry: 0,
            retries: 0,
        })
    }
    pub const fn status(&self) -> SignallingStatus {
        self.call.status()
    }
    pub const fn profile(&self) -> Option<Profile> {
        self.call.profile()
    }
    pub const fn ended(&self) -> Option<EndReason> {
        self.ended
    }
    pub const fn audio_generation(&self) -> u32 {
        self.audio_generation
    }
    pub const fn transmitting(&self) -> bool {
        self.transmitting
    }
    pub fn media_ready(&self) -> bool {
        self.ended.is_none()
            && self.linked
            && self.verified
            && self.ready
            && self.profile_deadline.is_none()
            && self.call.status() == SignallingStatus::Established
    }
    pub fn playback_allowed(&self) -> bool {
        self.media_ready() && self.config.playback && !self.transmitting
    }

    pub fn link_established(&mut self) -> Events {
        let mut events = Events::new();
        if self.ended.is_some() || self.linked {
            return events;
        }
        self.linked = true;
        if self.call.role() == CallRole::Incoming {
            events.transitions(CallState::incoming_link_established(false));
        }
        events
    }
    /// Call only after the Link owner verified the exact peer identity for this
    /// session and immutable route. Parsing a signal cannot invoke this authority.
    pub fn peer_verified(&mut self, now_ms: u64) -> Events {
        let mut events = Events::new();
        if self.ended.is_some() || !self.linked || self.verified {
            return events;
        }
        self.verified = true;
        if self.call.role() == CallRole::Incoming {
            self.deadline = now_ms.saturating_add(RING_MS);
            events.transitions(self.call.caller_identified(false, true));
            events.push(Event::Transition(TelephonyAction::SendSignal(
                self.config.preferred.into(),
            )));
        }
        events
    }
    pub fn answer(&mut self, now_ms: u64) -> Events {
        let mut events = Events::new();
        if self.ended.is_some()
            || !self.verified
            || self.call.role() != CallRole::Incoming
            || self.call.status() != SignallingStatus::Ringing
        {
            return events;
        }
        self.deadline = now_ms.saturating_add(ANSWER_MS);
        self.answer_retry = now_ms.saturating_add(1000);
        events.transitions(self.call.answer());
        events
    }
    pub fn receive_signal(&mut self, signal: Signal, now_ms: u64) -> Events {
        let mut events = Events::new();
        if self.ended.is_some() || !self.linked || !self.verified {
            return events;
        }
        if let Signal::PreferredProfile(profile) = signal {
            if !self.config.allowed.contains(profile) {
                if self.suggested {
                    return self.end(EndReason::ProfileUnsupported);
                }
                self.suggested = true;
                self.profile_deadline = Some(now_ms.saturating_add(PROFILE_MS));
                self.stop_capture(&mut events);
                events.push(Event::FlushMedia);
                events.push(Event::Transition(TelephonyAction::SendSignal(
                    self.call.profile().unwrap_or(self.config.preferred).into(),
                )));
                return events;
            }
            self.profile_deadline = None;
            if self.call.profile() != Some(profile) {
                self.stop_capture(&mut events);
                self.ready = false;
                self.audio_generation = self.audio_generation.wrapping_add(1);
                events.push(Event::FlushMedia);
            }
        }
        if let Signal::Status(status) = signal {
            match status {
                SignallingStatus::Busy => return self.end(EndReason::Busy),
                SignallingStatus::Rejected => return self.end(EndReason::Rejected),
                SignallingStatus::Calling => return events,
                _ => {}
            }
            let current = self.call.status();
            let allowed = match self.call.role() {
                CallRole::Incoming => {
                    self.call.answered() && status == SignallingStatus::Established
                }
                CallRole::Outgoing => match status {
                    SignallingStatus::Available => matches!(
                        current,
                        SignallingStatus::Calling | SignallingStatus::Available
                    ),
                    SignallingStatus::Ringing => matches!(
                        current,
                        SignallingStatus::Available | SignallingStatus::Ringing
                    ),
                    SignallingStatus::Connecting => matches!(
                        current,
                        SignallingStatus::Ringing
                            | SignallingStatus::Connecting
                            | SignallingStatus::Established
                    ),
                    SignallingStatus::Established => matches!(
                        current,
                        SignallingStatus::Connecting | SignallingStatus::Established
                    ),
                    _ => false,
                },
            };
            if !allowed {
                return events;
            }
            if status == SignallingStatus::Ringing && current != status {
                self.deadline = now_ms.saturating_add(RING_MS);
            }
            if status == SignallingStatus::Connecting && current == SignallingStatus::Ringing {
                self.deadline = now_ms.saturating_add(ANSWER_MS);
            }
        }
        events.transitions(self.call.receive_signal(signal));
        events
    }
    /// A successfully configured device/codec is necessary but never starts capture.
    pub fn audio_ready(&mut self, generation: u32, success: bool) -> Events {
        if self.ended.is_some() || generation != self.audio_generation {
            return Events::new();
        }
        if !success {
            return self.end(EndReason::AudioUnavailable);
        }
        self.ready = true;
        Events::new()
    }
    /// The embedder must fence input edges by session and view generation.
    pub fn set_transmitting(&mut self, pressed: bool, now_ms: u64) -> Events {
        let mut events = Events::new();
        if !pressed {
            self.input_down = false;
            self.stop_capture(&mut events);
            events.push(Event::FlushMedia);
        } else if !self.input_down {
            self.input_down = true;
            if !self.media_ready() || !self.config.capture {
                return events;
            }
            self.transmitting = true;
            self.talk_started = now_ms;
            events.push(Event::FlushMedia);
            events.push(Event::Capture(true));
        }
        events
    }
    fn stop_capture(&mut self, events: &mut Events) {
        if self.transmitting {
            self.transmitting = false;
            events.push(Event::Capture(false));
        }
    }
    pub fn tick(&mut self, now_ms: u64) -> Events {
        let mut events = Events::new();
        if self.ended.is_some() {
            return events;
        }
        if self
            .profile_deadline
            .is_some_and(|deadline| now_ms >= deadline)
        {
            return self.end(EndReason::ProfileUnsupported);
        }
        if self.status() != SignallingStatus::Established && now_ms >= self.deadline {
            return self.end(EndReason::Timeout);
        }
        if self.transmitting && now_ms.saturating_sub(self.talk_started) >= TALK_LIMIT_MS {
            self.stop_capture(&mut events);
            events.push(Event::FlushMedia);
        }
        if self.call.role() == CallRole::Incoming
            && self.call.answered()
            && self.status() == SignallingStatus::Connecting
            && now_ms >= self.answer_retry
            && self.retries < 3
        {
            self.retries += 1;
            self.answer_retry = now_ms.saturating_add(1000);
            events.transitions(self.call.retry_answer());
        }
        events
    }
    pub fn end(&mut self, reason: EndReason) -> Events {
        let mut events = Events::new();
        if self.ended.is_some() {
            return events;
        }
        self.stop_capture(&mut events);
        events.push(Event::FlushMedia);
        self.ready = false;
        self.ended = Some(reason);
        events.transitions(self.call.hangup(reason == EndReason::Timeout));
        events.push(Event::Ended(reason));
        events
    }
}
