//! Fixed-storage, allocation-free Codec2 at 8 kHz mono PCM16.
//!
//! Adapted from the pinned pure-Rust Codec2 port recorded in `UPSTREAM.json`.
//! Initialise directly in caller-owned storage; the state is too large to create
//! as a temporary on an MCU task stack. Encoding/decoding and reset never allocate.
#![no_std]

mod buffer;
mod math;
use buffer::Buffer;
use math::FloatMath;

// Preserve the upstream DSP's names, numeric constants and indexing style so
// algorithm changes remain reviewable separately from the storage adaptation.
#[allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]
#[allow(clippy::all)]
mod native;

use core::mem::MaybeUninit;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Rate1600,
    Rate3200,
}

impl Mode {
    pub const fn samples(self) -> usize {
        match self {
            Self::Rate1600 => 320,
            Self::Rate3200 => 160,
        }
    }

    pub const fn bytes(self) -> usize {
        8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    PcmLength,
    EncodedLength,
}

/// Independent encoder and decoder histories at one native mode.
///
/// One serialized audio owner may alternate transmit and receive with this
/// instance; FFT scratch is reused and the histories remain independent. It
/// must never be accessed concurrently. Independent sessions need independent
/// instances. `Mode` is a codec configuration, not an LXST/LXMF wire identifier.
pub struct Codec {
    native: native::Codec2,
}

impl Codec {
    /// Initialise in final, aligned caller storage without a whole-state copy.
    pub fn initialise(storage: &mut MaybeUninit<Self>, mode: Mode) -> &mut Self {
        // SAFETY: all native fields are integers, floats, arrays, fixed buffers
        // (zero length is valid), or Codec2Mode (repr(u8), first variant is 0).
        // There are no references, pointers, heap owners or Drop implementations.
        // Thus zero bytes form a valid value. We finish logical initialisation
        // before publishing it. The exclusive borrow prevents concurrent access.
        let value = unsafe {
            storage.as_mut_ptr().write_bytes(0, 1);
            storage.assume_init_mut()
        };
        value.initialise_native(mode);
        value
    }

    fn initialise_native(&mut self, mode: Mode) {
        self.native.initialise(match mode {
            Mode::Rate1600 => native::Codec2Mode::MODE_1600,
            Mode::Rate3200 => native::Codec2Mode::MODE_3200,
        });
    }

    pub fn reset(&mut self, mode: Mode) {
        // SAFETY: same zero-valid representation as initialise; this type owns
        // no resources requiring Drop and self is exclusively borrowed.
        unsafe { core::ptr::write_bytes(self, 0, 1) };
        self.initialise_native(mode);
    }

    pub fn mode(&self) -> Mode {
        match self.native.samples_per_frame() {
            320 => Mode::Rate1600,
            _ => Mode::Rate3200,
        }
    }

    pub fn encode(&mut self, pcm: &[i16], output: &mut [u8]) -> Result<(), Error> {
        if pcm.len() != self.mode().samples() {
            return Err(Error::PcmLength);
        }
        if output.len() != self.mode().bytes() {
            return Err(Error::EncodedLength);
        }
        self.native.encode(output, pcm);
        Ok(())
    }

    pub fn decode(&mut self, encoded: &[u8], output: &mut [i16]) -> Result<(), Error> {
        if encoded.len() != self.mode().bytes() {
            return Err(Error::EncodedLength);
        }
        if output.len() != self.mode().samples() {
            return Err(Error::PcmLength);
        }
        self.native.decode(output, encoded);
        Ok(())
    }
}
