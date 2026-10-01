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
