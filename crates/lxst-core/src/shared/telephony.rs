use crate::actions::Actions;
use crate::{Profile, Signal, SignallingStatus};

macro_rules! actions {
    ($($value:expr),* $(,)?) => { Actions::from_array([$($value),*]) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallRole {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyAction {
    SendSignal(Signal),
    IdentifyLocalIdentity,
    SelectProfile(Profile),
    PrepareDialingPipelines,
    ResetDialingPipelines,
    OpenAudioPipelines,
    StartAudioPipelines,
    StartDialTone,
    Terminate(Option<SignallingStatus>),
    TeardownLink,
    RingIncomingCall,
    SwitchProfile(Profile),
    IgnoreSignal(Signal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallState {
    role: CallRole,
    status: SignallingStatus,
    profile: Option<Profile>,
    answered: bool,
}

impl CallState {
    pub fn outgoing(profile: Option<Profile>) -> Self {
        Self {
            role: CallRole::Outgoing,
            status: SignallingStatus::Calling,
            profile,
            answered: false,
        }
    }

    pub fn incoming() -> Self {
        Self {
            role: CallRole::Incoming,
            status: SignallingStatus::Available,
            profile: None,
            answered: false,
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
        self.push_status_signal(SignallingStatus::Connecting, &mut actions);
        actions.push(TelephonyAction::OpenAudioPipelines);
        // Python LXST sends ESTABLISHED as the final half of the answer offer,
        // then the outgoing peer echoes ESTABLISHED while opening its own
        // pipelines. Keep the incoming call in CONNECTING until that remote
        // echo is observed: local queue admission alone is not proof that the
        // caller received the answer.
        actions.push(TelephonyAction::SendSignal(Signal::from(
            SignallingStatus::Established,
        )));
        actions
    }

    /// Re-send the wire-compatible answer offer without reopening pipelines
    /// or changing local state. The service bounds these retries and stops as
    /// soon as the outgoing peer's ESTABLISHED echo is observed.
    pub fn retry_answer(&self) -> Actions {
        if self.role != CallRole::Incoming
            || !self.answered
            || self.status != SignallingStatus::Connecting
        {
            return Actions::new();
        }

        actions![
            TelephonyAction::SendSignal(Signal::from(SignallingStatus::Connecting)),
            TelephonyAction::SendSignal(Signal::from(SignallingStatus::Established)),
        ]
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
                self.ensure_profile(&mut actions);
                actions.push(TelephonyAction::PrepareDialingPipelines);
                if let Some(profile) = self.profile {
                    actions.push(TelephonyAction::SendSignal(Signal::from(profile)));
                }
                actions.push(TelephonyAction::StartDialTone);
                actions
            }
            Signal::Status(SignallingStatus::Connecting) => {
                if self.role == CallRole::Outgoing
                    && self.status.wire_value() >= SignallingStatus::Connecting.wire_value()
                {
                    // A delayed/retried CONNECTING must not regress an already
                    // established call. Re-echo ESTABLISHED so an incoming
                    // peer can recover if its first acknowledgement was lost.
                    return actions![TelephonyAction::SendSignal(Signal::from(
                        SignallingStatus::Established,
                    ))];
                }
                if self.status.wire_value() >= SignallingStatus::Connecting.wire_value() {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                self.status = SignallingStatus::Connecting;
                let mut actions = actions![
                    TelephonyAction::ResetDialingPipelines,
                    TelephonyAction::OpenAudioPipelines,
                ];
                if self.role == CallRole::Outgoing {
                    // This mirrors Python Telephone.__open_pipelines(): the
                    // caller echoes ESTABLISHED after observing CONNECTING.
                    actions.push(TelephonyAction::SendSignal(Signal::from(
                        SignallingStatus::Established,
                    )));
                }
                actions
            }
            Signal::Status(SignallingStatus::Established) => {
                if self.status == SignallingStatus::Established {
                    return actions![TelephonyAction::IgnoreSignal(signal)];
                }
                self.status = SignallingStatus::Established;
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
            Signal::Raw(_) => actions![TelephonyAction::IgnoreSignal(signal)],
        }
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
        actions
    }

    fn ensure_profile(&mut self, actions: &mut Actions) {
        let profile = self.profile.unwrap_or(Profile::DEFAULT);
        self.profile = Some(profile);
        actions.push(TelephonyAction::SelectProfile(profile));
    }

    fn push_status_signal(&mut self, status: SignallingStatus, actions: &mut Actions) {
        if status.is_auto_status() {
            self.status = status;
        }
        actions.push(TelephonyAction::SendSignal(Signal::from(status)));
    }
}
