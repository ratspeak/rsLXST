//! Core LXST wire types.
//!
//! This crate owns the byte-level contract with Python LXST. It deliberately
//! avoids audio and Reticulum runtime dependencies so packet/profile parity can
//! be tested in isolation.

mod opus;
mod profile;
mod raw;
mod stream;
mod synthetic;
mod telephony;
mod wire;

pub use opus::{
    OPUS_ENCODED_FRAME_MAX_BYTES, OPUS_ENCODED_PACKET_MAX_BYTES, OPUS_MONO_DECODE_SAMPLE_RATE_HZ,
    OpusCodecError, OpusDecoderState, OpusEncoderState, OpusMonoDecoder,
    opus_packet_duration_samples_48k,
};
pub use profile::{AudioCodec, OpusApplication, OpusProfile, Profile, SignallingStatus};
pub use raw::RawAudioFrame;
pub use stream::{
    DropPolicy, FramePacketizer, FrameStreamEvent, FrameStreamState, JitterBuffer, JitterPush,
    JitterStats, StreamError,
};
pub use synthetic::{RawFrameCollector, SyntheticError, SyntheticSource, SyntheticSourceKind};
pub use telephony::{CallRole, TelephonyAction, TelephonyCall};
pub use wire::{
    Codec2Mode, CodecKind, Error, FIELD_FRAMES, FIELD_SIGNALLING, Frame, LxstPacket, RawBitDepth,
    RawFrameHeader, Signal,
};

pub const APP_NAME: &str = "lxst";
pub const TELEPHONY_PRIMITIVE_NAME: &str = "telephony";
pub const TELEPHONY_DESTINATION_NAME: &str = "lxst.telephony";
