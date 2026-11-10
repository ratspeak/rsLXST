use crate::{CallMode, Profile, Signal, SignallingStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallRole {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelephonyAction {
    SendSignal(Signal),
    /// Send both signals in one packet, as Python sends profile and mode on RINGING.
    SendSignalPair(Signal, Signal),
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
    SelectMode(CallMode),
    SwitchMode(CallMode),
    IgnoreSignal(Signal),
}
