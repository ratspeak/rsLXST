//! Bounded LXST Codec2 packet processing over caller-owned PCM and byte buffers.
//!
//! A backend processes native Codec2 frames. This module owns the LXST mode
//! header and profile aggregation; it does not own LXMF memo containers or their
//! different audio-mode identifiers. Backend memory, math, timing and supported
//! modes must be qualified separately before selecting one for a device.

use crate::{AudioCodec, Codec2Mode, Profile};
use thiserror::Error;

/// A stateful native Codec2 implementation at one fixed mode, mono 8 kHz PCM16.
///
/// Each operation receives exactly one native frame and an exactly sized output.
/// Implementations must not retain the borrowed buffers. A backend error may
/// advance its state: the caller must discard that packet and replace the codec
/// before continuing. Construction, scratch storage and reset belong to the
/// backend owner, allowing host and embedded implementations to differ.
pub trait Codec2Backend {
    type Error;

    fn mode(&self) -> Codec2Mode;
    fn encode_native(&mut self, pcm: &[i16], encoded: &mut [u8]) -> Result<(), Self::Error>;
    fn decode_native(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<(), Self::Error>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Codec2PacketError<E> {
    #[error("profile {0:?} does not use Codec2")]
    NonCodec2Profile(Profile),
    #[error("Codec2 backend mode {actual:?} does not match {expected:?}")]
    BackendModeMismatch {
        expected: Codec2Mode,
        actual: Codec2Mode,
    },
    #[error("Codec2 PCM sample count {actual} does not match {expected}")]
    SampleCount { expected: usize, actual: usize },
    #[error("Codec2 output capacity {actual} is below {required}")]
    OutputCapacity { required: usize, actual: usize },
    #[error("Codec2 payload length {actual} does not match {expected}")]
    PayloadLength { expected: usize, actual: usize },
    #[error("Codec2 mode header {actual:#04x} does not match {expected:#04x}")]
    ModeHeader { expected: u8, actual: u8 },
    #[error("Codec2 backend failed: {0}")]
    Backend(E),
    #[error("Codec2 must be replaced after a backend failure")]
    Failed,
}

/// One fixed-profile LXST Codec2 stream. Wrapper operations allocate no memory.
///
/// The native backend may allocate; this type makes no claim about its footprint.
/// Use separate instances for independent streams. Mode/profile changes require
/// a new instance so predictor state cannot leak between differently framed media.
pub struct Codec2PacketCodec<B> {
    backend: B,
    profile: Profile,
    mode: Codec2Mode,
    native_samples: usize,
    native_bytes: usize,
    failed: bool,
}

impl<B: Codec2Backend> Codec2PacketCodec<B> {
    pub fn new(profile: Profile, backend: B) -> Result<Self, Codec2PacketError<B::Error>> {
        let AudioCodec::Codec2(mode) = profile.audio_codec() else {
            return Err(Codec2PacketError::NonCodec2Profile(profile));
        };
        if backend.mode() != mode {
            return Err(Codec2PacketError::BackendModeMismatch {
                expected: mode,
                actual: backend.mode(),
            });
        }
        // 700C carries 28 bits in FOUR bytes per native frame. Padding is per
        // native frame, not once over the aggregated 400 ms packet.
        let (native_samples, native_bytes) = match profile {
            Profile::BandwidthUltraLow => (320, 4),
            Profile::BandwidthVeryLow => (320, 8),
            Profile::BandwidthLow => (160, 8),
            _ => return Err(Codec2PacketError::NonCodec2Profile(profile)),
        };
        Ok(Self {
            backend,
            profile,
            mode,
            native_samples,
            native_bytes,
            failed: false,
        })
    }

    pub const fn profile(&self) -> Profile {
        self.profile
    }

    pub const fn mode(&self) -> Codec2Mode {
        self.mode
    }

    pub const fn sample_frames(&self) -> usize {
        self.profile.sample_frames_per_packet()
    }

    /// Payload bytes including the mode header, excluding the outer codec byte.
    pub const fn payload_bytes(&self) -> usize {
        1 + self.sample_frames() / self.native_samples * self.native_bytes
    }

    fn ready(&self) -> Result<(), Codec2PacketError<B::Error>> {
        if self.failed {
            return Err(Codec2PacketError::Failed);
        }
        let actual = self.backend.mode();
        if actual != self.mode {
            return Err(Codec2PacketError::BackendModeMismatch {
                expected: self.mode,
                actual,
            });
        }
        Ok(())
    }

    /// Encode exactly one profile packet. PCM is mono 8 kHz, signed 16-bit.
    ///
    /// Capacity/shape errors leave backend and output unchanged. A backend error
    /// invalidates this stream and the partially written output; replace it before
    /// reuse. Bytes beyond the returned length remain untouched.
    pub fn encode_into(
        &mut self,
        pcm: &[i16],
        output: &mut [u8],
    ) -> Result<usize, Codec2PacketError<B::Error>> {
        self.ready()?;
        if pcm.len() != self.sample_frames() {
            return Err(Codec2PacketError::SampleCount {
                expected: self.sample_frames(),
                actual: pcm.len(),
            });
        }
        let required = self.payload_bytes();
        if output.len() < required {
            return Err(Codec2PacketError::OutputCapacity {
                required,
                actual: output.len(),
            });
        }
        for (samples, bytes) in pcm
            .chunks_exact(self.native_samples)
            .zip(output[1..required].chunks_exact_mut(self.native_bytes))
        {
            if let Err(error) = self.backend.encode_native(samples, bytes) {
                self.failed = true;
                return Err(Codec2PacketError::Backend(error));
            }
        }
        output[0] = self.mode.header();
        Ok(required)
    }

    /// Decode exactly one negotiated-profile payload, including its mode header.
    ///
    /// A peer cannot trigger mode changes, unbounded frame counts or allocations
    /// through this wrapper. The backend is never called on malformed payloads.
    /// On backend failure discard the partial PCM and replace this stream.
    pub fn decode_into(
        &mut self,
        payload: &[u8],
        output: &mut [i16],
    ) -> Result<usize, Codec2PacketError<B::Error>> {
        self.ready()?;
        if payload.len() != self.payload_bytes() {
            return Err(Codec2PacketError::PayloadLength {
                expected: self.payload_bytes(),
                actual: payload.len(),
            });
        }
        if payload[0] != self.mode.header() {
            return Err(Codec2PacketError::ModeHeader {
                expected: self.mode.header(),
                actual: payload[0],
            });
        }
        let required = self.sample_frames();
        if output.len() < required {
            return Err(Codec2PacketError::OutputCapacity {
                required,
                actual: output.len(),
            });
        }
        for (bytes, samples) in payload[1..]
            .chunks_exact(self.native_bytes)
            .zip(output[..required].chunks_exact_mut(self.native_samples))
        {
            if let Err(error) = self.backend.decode_native(bytes, samples) {
                self.failed = true;
                return Err(Codec2PacketError::Backend(error));
            }
        }
        Ok(required)
    }
}

#[cfg(test)]
mod tests;
