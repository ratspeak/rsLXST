use crate::{Profile, Signal, SignallingStatus};

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
pub struct TelephonyCall {
    role: CallRole,
    status: SignallingStatus,
    profile: Option<Profile>,
    answered: bool,
}

impl TelephonyCall {
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

    pub fn incoming_link_established(line_busy: bool) -> Vec<TelephonyAction> {
        if line_busy {
            vec![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Busy)),
                TelephonyAction::TeardownLink,
            ]
        } else {
            vec![TelephonyAction::SendSignal(Signal::from(
                SignallingStatus::Available,
            ))]
        }
    }

    pub fn caller_identified(&mut self, line_busy: bool, allowed: bool) -> Vec<TelephonyAction> {
        if line_busy || !allowed {
            return vec![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Busy)),
                TelephonyAction::TeardownLink,
            ];
        }

        let mut actions = Vec::new();
        actions.push(TelephonyAction::ResetDialingPipelines);
        self.push_status_signal(SignallingStatus::Ringing, &mut actions);
        actions.push(TelephonyAction::RingIncomingCall);
        actions
    }

    pub fn answer(&mut self) -> Vec<TelephonyAction> {
        if self.role != CallRole::Incoming || self.status != SignallingStatus::Ringing {
            return Vec::new();
        }

        let mut actions = Vec::new();
        self.answered = true;
        self.ensure_profile(&mut actions);
        self.push_status_signal(SignallingStatus::Connecting, &mut actions);
        actions.push(TelephonyAction::OpenAudioPipelines);
        self.push_status_signal(SignallingStatus::Established, &mut actions);
        actions.push(TelephonyAction::StartAudioPipelines);
        actions
    }

    pub fn receive_signal(&mut self, signal: Signal) -> Vec<TelephonyAction> {
        if self.role == CallRole::Incoming && !self.answered && matches!(signal, Signal::Status(_))
        {
            return vec![TelephonyAction::IgnoreSignal(signal)];
        }

        match signal {
            Signal::Status(SignallingStatus::Busy) => {
                vec![TelephonyAction::Terminate(Some(SignallingStatus::Busy))]
            }
            Signal::Status(SignallingStatus::Rejected) => {
                vec![TelephonyAction::Terminate(Some(SignallingStatus::Rejected))]
            }
            Signal::Status(SignallingStatus::Calling) => {
                vec![TelephonyAction::IgnoreSignal(signal)]
            }
            Signal::Status(SignallingStatus::Available) => {
                self.status = SignallingStatus::Available;
                vec![TelephonyAction::IdentifyLocalIdentity]
            }
            Signal::Status(SignallingStatus::Ringing) => {
                let mut actions = Vec::new();
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
                self.status = SignallingStatus::Connecting;
                vec![
                    TelephonyAction::ResetDialingPipelines,
                    TelephonyAction::OpenAudioPipelines,
                ]
            }
            Signal::Status(SignallingStatus::Established) => {
                self.status = SignallingStatus::Established;
                vec![TelephonyAction::StartAudioPipelines]
            }
            Signal::PreferredProfile(profile) => {
                if self.profile == Some(profile) {
                    return Vec::new();
                }

                self.profile = Some(profile);
                if self.status == SignallingStatus::Established {
                    vec![TelephonyAction::SwitchProfile(profile)]
                } else {
                    vec![TelephonyAction::SelectProfile(profile)]
                }
            }
            Signal::Raw(_) => vec![TelephonyAction::IgnoreSignal(signal)],
        }
    }

    pub fn switch_profile(&mut self, profile: Profile) -> Vec<TelephonyAction> {
        if self.profile == Some(profile) {
            return Vec::new();
        }

        self.profile = Some(profile);
        if self.status == SignallingStatus::Established {
            vec![
                TelephonyAction::SendSignal(Signal::from(profile)),
                TelephonyAction::SwitchProfile(profile),
            ]
        } else {
            vec![TelephonyAction::SelectProfile(profile)]
        }
    }

    pub fn hangup(&mut self, ring_timeout: bool) -> Vec<TelephonyAction> {
        let mut actions = Vec::new();

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

    fn ensure_profile(&mut self, actions: &mut Vec<TelephonyAction>) {
        let profile = self.profile.unwrap_or(Profile::DEFAULT);
        self.profile = Some(profile);
        actions.push(TelephonyAction::SelectProfile(profile));
    }

    fn push_status_signal(&mut self, status: SignallingStatus, actions: &mut Vec<TelephonyAction>) {
        if status.is_auto_status() {
            self.status = status;
        }
        actions.push(TelephonyAction::SendSignal(Signal::from(status)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_link_establishment_matches_python_busy_branch() {
        assert_eq!(
            TelephonyCall::incoming_link_established(false),
            vec![TelephonyAction::SendSignal(Signal::from(
                SignallingStatus::Available
            ))]
        );
        assert_eq!(
            TelephonyCall::incoming_link_established(true),
            vec![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Busy)),
                TelephonyAction::TeardownLink,
            ]
        );
    }

    #[test]
    fn incoming_identified_then_answered_sequence_matches_python() {
        let mut call = TelephonyCall::incoming();
        let ringing = call.caller_identified(false, true);
        assert_eq!(call.status(), SignallingStatus::Ringing);
        assert_eq!(
            ringing,
            vec![
                TelephonyAction::ResetDialingPipelines,
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Ringing)),
                TelephonyAction::RingIncomingCall,
            ]
        );

        let answer = call.answer();
        assert_eq!(call.status(), SignallingStatus::Established);
        assert!(call.answered());
        assert_eq!(call.profile(), Some(Profile::DEFAULT));
        assert_eq!(
            answer,
            vec![
                TelephonyAction::SelectProfile(Profile::DEFAULT),
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Connecting)),
                TelephonyAction::OpenAudioPipelines,
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Established)),
                TelephonyAction::StartAudioPipelines,
            ]
        );
    }

    #[test]
    fn outgoing_sequence_identifies_profiles_opens_and_starts_audio() {
        let mut call = TelephonyCall::outgoing(None);

        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Available)),
            vec![TelephonyAction::IdentifyLocalIdentity]
        );

        let ringing = call.receive_signal(Signal::from(SignallingStatus::Ringing));
        assert_eq!(call.profile(), Some(Profile::DEFAULT));
        assert_eq!(
            ringing,
            vec![
                TelephonyAction::SelectProfile(Profile::DEFAULT),
                TelephonyAction::PrepareDialingPipelines,
                TelephonyAction::SendSignal(Signal::from(Profile::DEFAULT)),
                TelephonyAction::StartDialTone,
            ]
        );

        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Connecting)),
            vec![
                TelephonyAction::ResetDialingPipelines,
                TelephonyAction::OpenAudioPipelines,
            ]
        );
        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::StartAudioPipelines]
        );
        assert_eq!(call.status(), SignallingStatus::Established);
    }

    #[test]
    fn profile_signals_select_or_switch_profile_by_call_status() {
        let mut call = TelephonyCall::outgoing(None);
        assert_eq!(
            call.receive_signal(Signal::from(Profile::LatencyLow)),
            vec![TelephonyAction::SelectProfile(Profile::LatencyLow)]
        );

        call.receive_signal(Signal::from(SignallingStatus::Established));
        assert_eq!(
            call.receive_signal(Signal::from(Profile::LatencyUltraLow)),
            vec![TelephonyAction::SwitchProfile(Profile::LatencyUltraLow)]
        );
    }

    #[test]
    fn duplicate_profile_signals_do_not_reconfigure_audio() {
        let mut call = TelephonyCall::outgoing(Some(Profile::QualityHigh));
        call.receive_signal(Signal::from(SignallingStatus::Established));

        assert!(
            call.receive_signal(Signal::from(Profile::QualityHigh))
                .is_empty()
        );
    }

    #[test]
    fn local_profile_switch_signals_remote_and_reconfigures_established_call() {
        let mut call = TelephonyCall::outgoing(Some(Profile::QualityMedium));
        call.receive_signal(Signal::from(SignallingStatus::Established));

        assert_eq!(
            call.switch_profile(Profile::QualityHigh),
            vec![
                TelephonyAction::SendSignal(Signal::from(Profile::QualityHigh)),
                TelephonyAction::SwitchProfile(Profile::QualityHigh),
            ]
        );
        assert_eq!(call.profile(), Some(Profile::QualityHigh));
        assert!(call.switch_profile(Profile::QualityHigh).is_empty());
    }

    #[test]
    fn incoming_call_ignores_status_signals_before_answer() {
        let mut call = TelephonyCall::incoming();
        call.caller_identified(false, true);

        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::IgnoreSignal(Signal::from(
                SignallingStatus::Established
            ))]
        );
    }

    #[test]
    fn incoming_hangup_sends_rejected_while_ringing_unless_timeout() {
        let mut call = TelephonyCall::incoming();
        call.caller_identified(false, true);
        assert_eq!(
            call.hangup(false),
            vec![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Rejected)),
                TelephonyAction::TeardownLink,
            ]
        );

        let mut timeout_call = TelephonyCall::incoming();
        timeout_call.caller_identified(false, true);
        assert_eq!(
            timeout_call.hangup(true),
            vec![TelephonyAction::TeardownLink]
        );
    }
}
