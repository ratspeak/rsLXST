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
    if samples % usize::from(channels) != 0 {
        Err(Error::InvalidRawSampleCount { samples, channels })
    } else {
        Ok(())
    }
}

fn decode_samples(bytes: &[u8], bit_depth: RawBitDepth) -> Result<Vec<f32>, Error> {
    let bytes_per_sample = bit_depth.bytes_per_sample();
    if bytes.len() % bytes_per_sample != 0 {
        return Err(Error::InvalidRawSampleBytes { bytes_per_sample });
    }

    let mut samples = Vec::with_capacity(bytes.len() / bytes_per_sample);
    match bit_depth {
        RawBitDepth::Float16 => {
            for chunk in bytes.chunks_exact(2) {
                samples.push(f16::from_le_bytes([chunk[0], chunk[1]]).to_f32());
            }
        }
        RawBitDepth::Float32 => {
            for chunk in bytes.chunks_exact(4) {
                samples.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
        }
        RawBitDepth::Float64 => {
            for chunk in bytes.chunks_exact(8) {
                samples.push(f64::from_le_bytes([
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ]) as f32);
            }
        }
        RawBitDepth::Float128 => {
            for chunk in bytes.chunks_exact(16) {
                samples.push(decode_float128_lossy(chunk));
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

fn decode_float128_lossy(bytes: &[u8]) -> f32 {
    // Python/Numpy names this dtype "float128", but on common little-endian
    // platforms it may be backed by an 80-bit extended value padded to 16 bytes.
    // We preserve finite zero exactly and otherwise use the leading f64 lane as
    // a conservative lossy fallback until a platform-specific long-double codec
    // is introduced.
    if bytes.iter().all(|byte| *byte == 0) {
        0.0
    } else {
        f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]) as f32
    }
}

fn encode_float128_from_f32(sample: f32, out: &mut Vec<u8>) {
    out.extend_from_slice(&f64::from(sample).to_le_bytes());
    out.extend_from_slice(&[0u8; 8]);
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
