use half::f16;

use crate::{CodecKind, Error, Frame, RawBitDepth, RawFrameHeader};

#[derive(Debug, Clone, PartialEq)]
pub struct RawAudioFrame {
    pub channels: u8,
    pub samples: Vec<f32>,
}

impl RawAudioFrame {
    pub fn new(channels: u8, samples: impl Into<Vec<f32>>) -> Result<Self, Error> {
        let samples = samples.into();
        validate_sample_count(channels, samples.len())?;
        Ok(Self { channels, samples })
    }

    pub fn sample_frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }

    pub fn from_frame(frame: &Frame) -> Result<Self, Error> {
        if frame.codec != CodecKind::Raw {
            return Err(Error::InvalidRawFrameCodec(frame.codec));
        }

        Self::from_payload(&frame.payload)
    }

    pub fn to_frame(&self, bit_depth: RawBitDepth) -> Result<Frame, Error> {
        Ok(Frame::new(CodecKind::Raw, self.to_payload(bit_depth)?))
    }

    pub fn from_payload(payload: &[u8]) -> Result<Self, Error> {
        let Some((&header_byte, sample_bytes)) = payload.split_first() else {
            return Err(Error::EmptyRawPayload);
        };

        let header = RawFrameHeader::parse(header_byte)?;
        let samples = decode_samples(sample_bytes, header.bit_depth)?;
        validate_sample_count(header.channels, samples.len())?;

        Ok(Self {
            channels: header.channels,
            samples,
        })
    }

    /// Convert to `channels`: mono is duplicated, extra channels are averaged to mono,
    /// otherwise the leading channels are kept and the last one repeated.
    pub fn with_channels(self, channels: u8) -> Result<Self, Error> {
        if channels == self.channels {
            return Ok(self);
        }
        let source = usize::from(self.channels);
        let target = usize::from(channels);
        let mut samples = Vec::with_capacity(self.sample_frames() * target);
        for frame in self.samples.chunks_exact(source) {
            if target == 1 {
                samples.push(frame.iter().sum::<f32>() / source as f32);
            } else {
                for channel in 0..target {
                    samples.push(frame[channel.min(source - 1)]);
                }
            }
        }
        Self::new(channels, samples)
    }

    /// Linearly resample from `input_rate_hz` to `output_rate_hz`.
    pub fn resampled(&self, input_rate_hz: u32, output_rate_hz: u32) -> Self {
        if input_rate_hz == output_rate_hz || input_rate_hz == 0 {
            return self.clone();
        }
        let input_frames = self.sample_frames();
        let output_frames =
            (input_frames as u64 * u64::from(output_rate_hz) / u64::from(input_rate_hz)) as usize;
        Self {
            channels: self.channels,
            samples: crate::opus::resample_interleaved_linear(
                &self.samples,
                input_frames,
                output_frames,
                usize::from(self.channels),
            ),
        }
    }

    pub fn to_payload(&self, bit_depth: RawBitDepth) -> Result<Vec<u8>, Error> {
        validate_sample_count(self.channels, self.samples.len())?;
        let header = RawFrameHeader::new(self.channels, bit_depth)?;
        let mut out = Vec::with_capacity(1 + self.samples.len() * bit_depth.bytes_per_sample());
        out.push(header.encode());
        encode_samples(&self.samples, bit_depth, &mut out);
        Ok(out)
    }
}

fn validate_sample_count(channels: u8, samples: usize) -> Result<(), Error> {
    RawFrameHeader::new(channels, RawBitDepth::Float16)?;
    if !samples.is_multiple_of(usize::from(channels)) {
        Err(Error::InvalidRawSampleCount { samples, channels })
    } else {
        Ok(())
    }
}

fn decode_samples(bytes: &[u8], bit_depth: RawBitDepth) -> Result<Vec<f32>, Error> {
    let bytes_per_sample = bit_depth.bytes_per_sample();
    if !bytes.len().is_multiple_of(bytes_per_sample) {
        return Err(Error::InvalidRawSampleBytes { bytes_per_sample });
    }

    let mut samples = Vec::with_capacity(bytes.len() / bytes_per_sample);
    match bit_depth {
        RawBitDepth::Float16 => {
            for chunk in bytes.as_chunks::<2>().0 {
                samples.push(f16::from_le_bytes([chunk[0], chunk[1]]).to_f32());
            }
        }
        RawBitDepth::Float32 => {
            for chunk in bytes.as_chunks::<4>().0 {
                samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        RawBitDepth::Float64 => {
            for chunk in bytes.as_chunks::<8>().0 {
                samples.push(f64::from_le_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ]) as f32);
            }
        }
        RawBitDepth::Float128 => {
            for chunk in bytes.as_chunks::<16>().0 {
                samples.push(decode_float128(chunk));
            }
        }
    }

    Ok(samples)
}

fn encode_samples(samples: &[f32], bit_depth: RawBitDepth, out: &mut Vec<u8>) {
    match bit_depth {
        RawBitDepth::Float16 => {
            for sample in samples {
                out.extend_from_slice(&f16::from_f32(*sample).to_le_bytes());
            }
        }
        RawBitDepth::Float32 => {
            for sample in samples {
                out.extend_from_slice(&sample.to_le_bytes());
            }
        }
        RawBitDepth::Float64 => {
            for sample in samples {
                out.extend_from_slice(&f64::from(*sample).to_le_bytes());
            }
        }
        RawBitDepth::Float128 => {
            for sample in samples {
                encode_float128_from_f32(*sample, out);
            }
        }
    }
}

// numpy float128 is IEEE binary128 on Linux aarch64; x86_64 pads an 80-bit value instead.
const BINARY128_EXPONENT_BIAS: i32 = 16_383;
const BINARY128_MANTISSA_BITS: u32 = 112;

fn decode_float128(bytes: &[u8]) -> f32 {
    let mut le = [0u8; 16];
    le.copy_from_slice(bytes);
    let bits = u128::from_le_bytes(le);
    let negative = bits >> 127 != 0;
    let exponent = ((bits >> BINARY128_MANTISSA_BITS) & 0x7FFF) as i32;
    let mantissa = bits & ((1u128 << BINARY128_MANTISSA_BITS) - 1);
    let magnitude = if exponent == 0x7FFF {
        if mantissa == 0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else if exponent == 0 {
        // Subnormal binary128 values are far below the f32 range.
        0.0
    } else {
        let fraction = (mantissa >> (BINARY128_MANTISSA_BITS - 52)) as f64 / (1u64 << 52) as f64;
        (1.0 + fraction) * 2f64.powi(exponent - BINARY128_EXPONENT_BIAS)
    };
    (if negative { -magnitude } else { magnitude }) as f32
}

fn encode_float128_from_f32(sample: f32, out: &mut Vec<u8>) {
    let value = f64::from(sample);
    let f64_bits = value.to_bits();
    let sign = u128::from(f64_bits >> 63) << 127;
    let exponent = ((f64_bits >> 52) & 0x7FF) as i32;
    let mantissa = u128::from(f64_bits & ((1u64 << 52) - 1));
    let bits = if value == 0.0 {
        sign
    } else if exponent == 0x7FF {
        let nan_mantissa = if mantissa == 0 { 0 } else { 1u128 << 111 };
        sign | (0x7FFFu128 << BINARY128_MANTISSA_BITS) | nan_mantissa
    } else {
        // Every f32 widens to a normal f64.
        let exponent = (exponent - 1023 + BINARY128_EXPONENT_BIAS) as u128;
        sign | (exponent << BINARY128_MANTISSA_BITS) | (mantissa << (BINARY128_MANTISSA_BITS - 52))
    };
    out.extend_from_slice(&bits.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_audio_frame_encodes_python_default_float16_payload() {
        let raw = RawAudioFrame::new(2, vec![0.0, 0.5, -0.25, 1.0]).unwrap();
        let payload = raw.to_payload(RawBitDepth::Float16).unwrap();
        assert_eq!(
            payload,
            vec![0x01, 0x00, 0x00, 0x00, 0x38, 0x00, 0xB4, 0x00, 0x3C]
        );
        assert_eq!(RawAudioFrame::from_payload(&payload).unwrap(), raw);
    }

    #[test]
    fn raw_audio_frame_encodes_float32_and_float64() {
        let raw = RawAudioFrame::new(1, vec![0.25, -1.5]).unwrap();

        let f32_payload = raw.to_payload(RawBitDepth::Float32).unwrap();
        assert_eq!(f32_payload[0], 0x40);
        assert_eq!(RawAudioFrame::from_payload(&f32_payload).unwrap(), raw);

        let f64_payload = raw.to_payload(RawBitDepth::Float64).unwrap();
        assert_eq!(f64_payload[0], 0x80);
        assert_eq!(RawAudioFrame::from_payload(&f64_payload).unwrap(), raw);
    }

    #[test]
    fn raw_audio_frame_rejects_misaligned_payloads() {
        assert_eq!(
            RawAudioFrame::from_payload(&[]),
            Err(Error::EmptyRawPayload)
        );
        assert_eq!(
            RawAudioFrame::from_payload(&[0x40, 0x00]),
            Err(Error::InvalidRawSampleBytes {
                bytes_per_sample: 4,
            })
        );
        assert_eq!(
            RawAudioFrame::new(2, vec![0.0]),
            Err(Error::InvalidRawSampleCount {
                samples: 1,
                channels: 2,
            })
        );
    }

    #[test]
    fn raw_audio_frame_converts_to_and_from_lxst_frame() {
        let raw = RawAudioFrame::new(1, vec![0.0, 1.0]).unwrap();
        let frame = raw.to_frame(RawBitDepth::Float16).unwrap();
        assert_eq!(frame.codec, CodecKind::Raw);
        assert_eq!(RawAudioFrame::from_frame(&frame).unwrap(), raw);
    }
}
