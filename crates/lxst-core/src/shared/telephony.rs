use crate::actions::Actions;
use crate::{CallMode, CallRole, Profile, Signal, SignallingStatus, TelephonyAction};

macro_rules! actions {
    ($($value:expr),* $(,)?) => { Actions::from_array([$($value),*]) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallState {
    role: CallRole,
    status: SignallingStatus,
    profile: Option<Profile>,
    /// Raw mode value; Python stores unknown modes without acting on them.
    mode: Option<u8>,
    answered: bool,
    pipelines_started: bool,
    transmit_squelched: bool,
    remote_mode_follow_disabled: bool,
}

impl CallState {
    pub fn outgoing(profile: Option<Profile>) -> Self {
        Self::outgoing_with_mode(profile, None)
    }

    pub fn outgoing_with_mode(profile: Option<Profile>, mode: Option<CallMode>) -> Self {
        Self {
            role: CallRole::Outgoing,
            status: SignallingStatus::Calling,
            profile,
            mode: mode.map(|mode| mode.wire_value()),
            answered: false,
            pipelines_started: false,
            transmit_squelched: false,
            remote_mode_follow_disabled: false,
        }
    }

    pub fn incoming() -> Self {
        Self {
            role: CallRole::Incoming,
            status: SignallingStatus::Available,
            profile: None,
            mode: None,
            answered: false,
            pipelines_started: false,
            transmit_squelched: false,
            remote_mode_follow_disabled: false,
        }
    }

    pub const fn role(&self) -> CallRole {
        self.role
    }

    pub const fn status(&self) -> SignallingStatus {
        self.status
    }

    pub const fn profile(&self) -> Option<Profile> {
        self.profile
    }

    pub const fn answered(&self) -> bool {
        self.answered
    }

    /// Whether audio pipelines were started; a caller only starts them on the callee's ESTABLISHED.
    pub const fn audio_started(&self) -> bool {
        self.pipelines_started
    }

    /// Selected call mode, or `None` before selection or for an unknown remote mode.
    pub const fn mode(&self) -> Option<CallMode> {
        match self.mode {
            Some(value) => CallMode::from_wire(value),
            None => None,
        }
    }

    /// Whether outbound media frames are dropped, as Python's squelched Packetizer.
    pub const fn transmit_squelched(&self) -> bool {
        self.transmit_squelched
    }

    /// Squelch or unsquelch transmit locally without signalling, as Python `squelch_transmit`.
    pub fn set_transmit_squelched(&mut self, squelched: bool) {
        self.transmit_squelched = squelched;
    }

    /// Ignore later remote mode switches, as Python `disable_remote_mode_follow`.
    pub fn disable_remote_mode_follow(&mut self) {
        self.remote_mode_follow_disabled = true;
    }

    pub fn incoming_link_established(line_busy: bool) -> Actions {
        if line_busy {
            actions![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Busy)),
                TelephonyAction::TeardownLink,
            ]
        } else {
            actions![TelephonyAction::SendSignal(Signal::from(
                SignallingStatus::Available,
            ))]
        }
    }

    pub fn caller_identified(&mut self, line_busy: bool, allowed: bool) -> Actions {
        if line_busy || !allowed {
            return actions![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Busy)),
                TelephonyAction::TeardownLink,
            ];
        }

        let mut actions = Actions::new();
        actions.push(TelephonyAction::ResetDialingPipelines);
        self.push_status_signal(SignallingStatus::Ringing, &mut actions);
        actions.push(TelephonyAction::RingIncomingCall);
        actions
    }

    pub fn answer(&mut self) -> Actions {
        if self.role != CallRole::Incoming || self.status != SignallingStatus::Ringing {
            return Actions::new();
        }

        let mut actions = Actions::new();
        self.answered = true;
        self.ensure_profile(&mut actions);
        self.ensure_mode(&mut actions);
        self.squelch_for_opened_pipelines();
        self.push_status_signal(SignallingStatus::Connecting, &mut actions);
        actions.push(TelephonyAction::OpenAudioPipelines);
        self.push_status_signal(SignallingStatus::Established, &mut actions);
        actions.push(TelephonyAction::StartAudioPipelines);
        self.pipelines_started = true;
        actions
    }

    pub fn receive_signal(&mut self, signal: Signal) -> Actions {
        if self.role == CallRole::Incoming && !self.answered && matches!(signal, Signal::Status(_))
        {
            return actions![TelephonyAction::IgnoreSignal(signal)];
        }

        match signal {
            Signal::Status(SignallingStatus::Busy) => {
                actions![TelephonyAction::Terminate(Some(SignallingStatus::Busy))]
            }
            Signal::Status(SignallingStatus::Rejected) => {
                actions![TelephonyAction::Terminate(Some(SignallingStatus::Rejected))]
            }
            Signal::Status(SignallingStatus::Calling) => {
                actions![TelephonyAction::IgnoreSignal(signal)]
            }
            Signal::Status(SignallingStatus::Available) => {
                if self.status.wire_value() >= SignallingStatus::Available.wire_value() {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                self.status = SignallingStatus::Available;
                actions![TelephonyAction::IdentifyLocalIdentity]
            }
            Signal::Status(SignallingStatus::Ringing) => {
                if self.status.wire_value() >= SignallingStatus::Ringing.wire_value() {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                let mut actions = Actions::new();
                self.status = SignallingStatus::Ringing;
                let profile = self.ensure_profile(&mut actions);
                let mode = self.ensure_mode(&mut actions);
                actions.push(TelephonyAction::PrepareDialingPipelines);
                actions.push(TelephonyAction::SendSignalPair(
                    Signal::from(profile),
                    Signal::PreferredMode(mode),
                ));
                actions.push(TelephonyAction::StartDialTone);
                actions
            }
            Signal::Status(SignallingStatus::Connecting) => {
                if self.role == CallRole::Outgoing
                    && self.status.wire_value() >= SignallingStatus::Connecting.wire_value()
                {
                    // Python answers every CONNECTING with ESTABLISHED, and callees
                    // from rsLXST 0.3.1 and earlier resend CONNECTING until they see it.
                    return actions![TelephonyAction::SendSignal(Signal::from(
                        SignallingStatus::Established,
                    ))];
                }
                if self.status.wire_value() >= SignallingStatus::Connecting.wire_value() {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                self.status = SignallingStatus::Connecting;
                self.squelch_for_opened_pipelines();
                let mut actions = actions![
                    TelephonyAction::ResetDialingPipelines,
                    TelephonyAction::OpenAudioPipelines,
                ];
                if self.role == CallRole::Outgoing {
                    // Python's caller signals ESTABLISHED once its pipelines are open,
                    // which makes its own call status ESTABLISHED.
                    self.push_status_signal(SignallingStatus::Established, &mut actions);
                }
                actions
            }
            Signal::Status(SignallingStatus::Established) => {
                if self.role != CallRole::Outgoing || self.pipelines_started {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                self.status = SignallingStatus::Established;
                self.pipelines_started = true;
                actions![TelephonyAction::StartAudioPipelines]
            }
            Signal::PreferredProfile(profile) => {
                if self.profile == Some(profile) {
                    return Actions::new();
                }

                self.profile = Some(profile);
                if self.status == SignallingStatus::Established {
                    actions![TelephonyAction::SwitchProfile(profile)]
                } else {
                    actions![TelephonyAction::SelectProfile(profile)]
                }
            }
            Signal::PreferredMode(mode) => {
                if self.status == SignallingStatus::Established {
                    if self.remote_mode_follow_disabled || self.mode == Some(mode) {
                        return Actions::new();
                    }
                    let Some(known) = CallMode::from_wire(mode) else {
                        return Actions::new();
                    };
                    self.mode = Some(mode);
                    self.apply_mode_squelch(known);
                    actions![TelephonyAction::SwitchMode(known)]
                } else {
                    self.mode = Some(mode);
                    match CallMode::from_wire(mode) {
                        Some(known) => {
                            self.apply_mode_squelch(known);
                            actions![TelephonyAction::SelectMode(known)]
                        }
                        None => Actions::new(),
                    }
                }
            }
            Signal::Raw(_) => actions![TelephonyAction::IgnoreSignal(signal)],
        }
    }

    /// Switch the mode of an established call and signal it, as Python `switch_mode`.
    pub fn switch_mode(&mut self, mode: CallMode) -> Actions {
        if self.status != SignallingStatus::Established || self.mode == Some(mode.wire_value()) {
            return Actions::new();
        }
        self.mode = Some(mode.wire_value());
        self.apply_mode_squelch(mode);
        actions![
            TelephonyAction::SendSignal(Signal::from(mode)),
            TelephonyAction::SwitchMode(mode),
        ]
    }

    pub fn switch_profile(&mut self, profile: Profile) -> Actions {
        if self.profile == Some(profile) {
            return Actions::new();
        }

        self.profile = Some(profile);
        if self.status == SignallingStatus::Established {
            actions![
                TelephonyAction::SendSignal(Signal::from(profile)),
                TelephonyAction::SwitchProfile(profile),
            ]
        } else {
            actions![TelephonyAction::SelectProfile(profile)]
        }
    }

    pub fn hangup(&mut self, ring_timeout: bool) -> Actions {
        let mut actions = Actions::new();

        if self.role == CallRole::Incoming
            && self.status == SignallingStatus::Ringing
            && !ring_timeout
        {
            actions.push(TelephonyAction::SendSignal(Signal::from(
                SignallingStatus::Rejected,
            )));
        }

        actions.push(TelephonyAction::TeardownLink);
        self.status = SignallingStatus::Available;
        self.answered = false;
        self.pipelines_started = false;
        actions
    }

    fn ensure_profile(&mut self, actions: &mut Actions) -> Profile {
        let profile = self.profile.unwrap_or(Profile::DEFAULT);
        self.profile = Some(profile);
        actions.push(TelephonyAction::SelectProfile(profile));
        profile
    }

    /// Python `__select_call_mode`: an unset mode becomes the default, an unknown one is kept.
    fn ensure_mode(&mut self, actions: &mut Actions) -> u8 {
        let mode = self.mode.unwrap_or(CallMode::DEFAULT.wire_value());
        self.mode = Some(mode);
        if let Some(known) = CallMode::from_wire(mode) {
            self.apply_mode_squelch(known);
            actions.push(TelephonyAction::SelectMode(known));
        }
        mode
    }

    fn apply_mode_squelch(&mut self, mode: CallMode) {
        self.transmit_squelched = mode == CallMode::HalfDuplex;
    }

    /// Python builds a new Packetizer when pipelines open and squelches it only for half duplex.
    fn squelch_for_opened_pipelines(&mut self) {
        self.transmit_squelched = self.mode == Some(CallMode::HalfDuplex.wire_value());
    }

    fn push_status_signal(&mut self, status: SignallingStatus, actions: &mut Actions) {
        if status.is_auto_status() {
            self.status = status;
        }
        actions.push(TelephonyAction::SendSignal(Signal::from(status)));
    }
}
