//! Allocation-free LXST wire/session boundary for constrained clients.
//! Shared profile, wire identifiers and telephony transitions come directly from
//! lxst-core. Authentication, permissions, audio devices and routing are owned by
//! the embedding runtime; parsing bytes never establishes a trusted identity.
#![no_std]

#[path = "../../lxst-core/src/shared/actions.rs"]
mod actions;
#[path = "../../lxst-core/src/shared/telephony.rs"]
mod call_state;
#[path = "../../lxst-core/src/shared/profile.rs"]
mod profile;
pub mod wire;
#[path = "../../lxst-core/src/shared/wire_types.rs"]
mod wire_types;

pub use actions::Actions;
pub use call_state::{CallRole, CallState, TelephonyAction};
pub use profile::{AudioCodec, OpusApplication, OpusProfile, Profile, SignallingStatus};
pub use wire_types::{Codec2Mode, CodecKind, Signal};

#[path = "../../lxst-core/src/shared/codec2.rs"]
mod codec2;
pub use codec2::{Codec2Backend, Codec2PacketCodec, Codec2PacketError};
mod session;
pub use session::{ConfigError, EndReason, Event, Events, ProfileSet, Session, SessionConfig};

impl Codec2Backend for &mut lxst_codec2::Codec {
    type Error = lxst_codec2::Error;
    fn mode(&self) -> Codec2Mode {
        match lxst_codec2::Codec::mode(self) {
            lxst_codec2::Mode::Rate1600 => Codec2Mode::Mode1600,
            lxst_codec2::Mode::Rate3200 => Codec2Mode::Mode3200,
        }
    }
    fn encode_native(&mut self, pcm: &[i16], encoded: &mut [u8]) -> Result<(), Self::Error> {
        self.encode(pcm, encoded)
    }
    fn decode_native(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<(), Self::Error> {
        self.decode(encoded, pcm)
    }
}

mod queue;
pub use queue::{MediaQueue, QueueError};
