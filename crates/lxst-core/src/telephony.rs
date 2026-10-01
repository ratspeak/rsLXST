use crate::call_state::CallState;
include!("shared/call_types.rs");

/// Allocating host facade over the same bounded transition core used on MCUs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelephonyCall {
    state: CallState,
}
impl TelephonyCall {
    pub fn outgoing(profile: Option<Profile>) -> Self {
        Self {
            state: CallState::outgoing(profile),
        }
    }
    pub fn incoming() -> Self {
        Self {
            state: CallState::incoming(),
        }
    }
    pub const fn role(&self) -> CallRole {
        self.state.role()
    }
    pub const fn status(&self) -> SignallingStatus {
        self.state.status()
    }
    pub const fn profile(&self) -> Option<Profile> {
        self.state.profile()
    }
    pub const fn answered(&self) -> bool {
        self.state.answered()
    }
    pub fn incoming_link_established(line_busy: bool) -> Vec<TelephonyAction> {
        CallState::incoming_link_established(line_busy)
            .as_slice()
            .to_vec()
    }
    pub fn caller_identified(&mut self, line_busy: bool, allowed: bool) -> Vec<TelephonyAction> {
        self.state
            .caller_identified(line_busy, allowed)
            .as_slice()
            .to_vec()
    }
    pub fn answer(&mut self) -> Vec<TelephonyAction> {
        self.state.answer().as_slice().to_vec()
    }
    pub fn retry_answer(&self) -> Vec<TelephonyAction> {
        self.state.retry_answer().as_slice().to_vec()
    }
    pub fn receive_signal(&mut self, signal: Signal) -> Vec<TelephonyAction> {
        self.state.receive_signal(signal).as_slice().to_vec()
    }
    pub fn switch_profile(&mut self, profile: Profile) -> Vec<TelephonyAction> {
        self.state.switch_profile(profile).as_slice().to_vec()
    }
    pub fn hangup(&mut self, ring_timeout: bool) -> Vec<TelephonyAction> {
        self.state.hangup(ring_timeout).as_slice().to_vec()
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
        assert_eq!(call.status(), SignallingStatus::Connecting);
        assert!(call.answered());
        assert_eq!(call.profile(), Some(Profile::DEFAULT));
        assert_eq!(
            answer,
            vec![
                TelephonyAction::SelectProfile(Profile::DEFAULT),
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Connecting)),
                TelephonyAction::OpenAudioPipelines,
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Established)),
            ]
        );

        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::StartAudioPipelines]
        );
        assert_eq!(call.status(), SignallingStatus::Established);
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
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Established)),
            ]
        );
        assert_eq!(
            call.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::StartAudioPipelines]
        );
        assert_eq!(call.status(), SignallingStatus::Established);
    }

    #[test]
    fn answer_retry_and_established_echo_are_bounded_idempotent_transitions() {
        let mut incoming = TelephonyCall::incoming();
        incoming.caller_identified(false, true);
        incoming.answer();
        assert_eq!(
            incoming.retry_answer(),
            vec![
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Connecting)),
                TelephonyAction::SendSignal(Signal::from(SignallingStatus::Established)),
            ]
        );

        assert_eq!(
            incoming.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::StartAudioPipelines]
        );
        assert!(incoming.retry_answer().is_empty());
        assert_eq!(
            incoming.receive_signal(Signal::from(SignallingStatus::Established)),
            vec![TelephonyAction::IgnoreSignal(Signal::from(
                SignallingStatus::Established
            ))]
        );

        let mut outgoing = TelephonyCall::outgoing(Some(Profile::DEFAULT));
        outgoing.receive_signal(Signal::from(SignallingStatus::Available));
        outgoing.receive_signal(Signal::from(SignallingStatus::Ringing));
        outgoing.receive_signal(Signal::from(SignallingStatus::Connecting));
        outgoing.receive_signal(Signal::from(SignallingStatus::Established));
        assert_eq!(
            outgoing.receive_signal(Signal::from(SignallingStatus::Connecting)),
            vec![TelephonyAction::SendSignal(Signal::from(
                SignallingStatus::Established
            ))]
        );
        assert_eq!(outgoing.status(), SignallingStatus::Established);
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
