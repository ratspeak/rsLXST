use opus_rs::{Application, OpusDecoder, OpusEncoder};
use thiserror::Error;

use crate::{AudioCodec, CodecKind, Frame, OpusApplication, OpusProfile, Profile, RawAudioFrame};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OpusCodecError {
    #[error("profile {0:?} does not use Opus")]
    NonOpusProfile(Profile),
    #[error("Opus frame channel count {actual} does not match profile channel count {expected}")]
    ChannelMismatch { expected: u8, actual: u8 },
    #[error("Opus frame sample count {actual} does not match profile sample count {expected}")]
    SampleFrameMismatch { expected: usize, actual: usize },
    #[error(
        "Opus frame duration is not supported by the current encoder: {sample_rate_hz} Hz / {sample_frames} samples"
    )]
    UnsupportedFrameDuration {
        sample_rate_hz: u32,
        sample_frames: usize,
    },
    #[error("invalid Opus frame codec {0:?}")]
    InvalidFrameCodec(CodecKind),
    #[error("Opus subframe payload length {0} exceeds the supported packet length encoding")]
    UnsupportedSubframePayloadLength(usize),
    #[error("Opus encoder returned an unsupported subpacket layout")]
    UnsupportedSubpacketLayout,
    #[error("malformed Opus packet: {0}")]
    MalformedPacket(&'static str),
    #[error("Opus codec error: {0}")]
    Codec(String),
    #[error("LXST wire error: {0}")]
    Wire(#[from] crate::Error),
}

pub struct OpusEncoderState {
    profile: Profile,
    channels: u8,
    sample_frames: usize,
    subframe_count: usize,
    subframe_sample_frames: usize,
    encode_sample_frames: usize,
    encode_subframe_sample_frames: usize,
    max_payload_bytes: usize,
    encoder: OpusEncoder,
}

impl OpusEncoderState {
    pub fn new(profile: Profile) -> Result<Self, OpusCodecError> {
        let opus_profile = match profile.audio_codec() {
            AudioCodec::Opus(profile) => profile,
            AudioCodec::Codec2(_) => return Err(OpusCodecError::NonOpusProfile(profile)),
        };
        let channels = opus_profile.channels();
        let sample_rate = opus_profile.sample_rate();
        let encode_sample_rate = encode_sample_rate(opus_profile);
        let sample_frames = profile.sample_frames_per_packet();
        let encode_sample_frames =
            scale_sample_frames(sample_frames, sample_rate, encode_sample_rate)?;
        let packet_layout = PacketLayout::new(encode_sample_rate, encode_sample_frames)?;
        let subframe_sample_frames = sample_frames
            .checked_div(packet_layout.subframe_count)
            .filter(|frames| frames * packet_layout.subframe_count == sample_frames)
            .ok_or(OpusCodecError::UnsupportedFrameDuration {
                sample_rate_hz: sample_rate,
                sample_frames,
            })?;
        let mut encoder = OpusEncoder::new(
            encode_sample_rate as i32,
            usize::from(channels),
            opus_application(opus_profile.application()),
        )
        .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
        encoder.bitrate_bps = opus_profile.bitrate_ceiling() as i32;
        encoder.use_cbr = false;

        Ok(Self {
            profile,
            channels,
            sample_frames,
            subframe_count: packet_layout.subframe_count,
            subframe_sample_frames,
            encode_sample_frames,
            encode_subframe_sample_frames: packet_layout.subframe_sample_frames,
            max_payload_bytes: opus_profile.max_bytes_per_frame_ms(profile.frame_time_ms()),
            encoder,
        })
    }

    pub const fn profile(&self) -> Profile {
        self.profile
    }

    pub const fn channels(&self) -> u8 {
        self.channels
    }

    pub const fn sample_frames(&self) -> usize {
        self.sample_frames
    }

    pub const fn subframe_count(&self) -> usize {
        self.subframe_count
    }

    pub const fn subframe_sample_frames(&self) -> usize {
        self.subframe_sample_frames
    }

    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    pub fn encode_frame(&mut self, frame: &RawAudioFrame) -> Result<Frame, OpusCodecError> {
        self.validate_frame_shape(frame)?;
        if self.subframe_count > 1 {
            return self.encode_multi_subframe_packet(frame);
        }

        let resampled;
        let input = if self.encode_sample_frames == self.sample_frames {
            &frame.samples
        } else {
            resampled = resample_interleaved_linear(
                &frame.samples,
                self.sample_frames,
                self.encode_sample_frames,
                usize::from(self.channels),
            );
            &resampled
        };
        let mut encoded = vec![0u8; self.max_payload_bytes];
        let written = self
            .encoder
            .encode(input, self.encode_sample_frames, &mut encoded)
            .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
        encoded.truncate(written);
        Ok(Frame::new(CodecKind::Opus, encoded))
    }

    fn encode_multi_subframe_packet(
        &mut self,
        frame: &RawAudioFrame,
    ) -> Result<Frame, OpusCodecError> {
        let channels = usize::from(self.channels);
        let budgets = self.subframe_payload_budgets()?;
        let mut subpackets = Vec::with_capacity(self.subframe_count);

        for (subframe_index, payload_budget) in budgets.into_iter().enumerate() {
            let start = subframe_index * self.subframe_sample_frames * channels;
            let end = start + self.subframe_sample_frames * channels;
            let resampled;
            let input = if self.encode_subframe_sample_frames == self.subframe_sample_frames {
                &frame.samples[start..end]
            } else {
                resampled = resample_interleaved_linear(
                    &frame.samples[start..end],
                    self.subframe_sample_frames,
                    self.encode_subframe_sample_frames,
                    channels,
                );
                &resampled
            };
            let mut encoded = vec![0u8; payload_budget + 1];
            let written = self
                .encoder
                .encode(input, self.encode_subframe_sample_frames, &mut encoded)
                .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
            encoded.truncate(written);
            if encoded.first().is_none_or(|toc| toc & 0x03 != 0) {
                return Err(OpusCodecError::UnsupportedSubpacketLayout);
            }
            subpackets.push(encoded);
        }

        let toc = (subpackets[0][0] & !0x03) | 0x03;
        let mut payload = Vec::with_capacity(self.max_payload_bytes);
        payload.push(toc);
        payload.push(0x80 | (self.subframe_count as u8));
        for subpacket in subpackets.iter().take(self.subframe_count - 1) {
            push_subframe_payload_len(&mut payload, subpacket.len() - 1)?;
        }
        for subpacket in subpackets {
            payload.extend_from_slice(&subpacket[1..]);
        }

        debug_assert!(payload.len() <= self.max_payload_bytes);
        Ok(Frame::new(CodecKind::Opus, payload))
    }

    fn subframe_payload_budgets(&self) -> Result<Vec<usize>, OpusCodecError> {
        let header_bytes = 2 + self.subframe_count - 1;
        let payload_budget = self
            .max_payload_bytes
            .checked_sub(header_bytes)
            .ok_or(OpusCodecError::UnsupportedSubpacketLayout)?;
        let base = payload_budget / self.subframe_count;
        let extra = payload_budget % self.subframe_count;
        Ok((0..self.subframe_count)
            .map(|index| base + usize::from(index < extra))
            .collect())
    }

    fn validate_frame_shape(&self, frame: &RawAudioFrame) -> Result<(), OpusCodecError> {
        if frame.channels != self.channels {
            return Err(OpusCodecError::ChannelMismatch {
                expected: self.channels,
                actual: frame.channels,
            });
        }
        if frame.sample_frames() != self.sample_frames {
            return Err(OpusCodecError::SampleFrameMismatch {
                expected: self.sample_frames,
                actual: frame.sample_frames(),
            });
        }
        Ok(())
    }
}

pub struct OpusDecoderState {
    profile: Profile,
    channels: u8,
    sample_frames: usize,
    subframe_count: usize,
    subframe_sample_frames: usize,
    decoder: OpusDecoder,
}

impl OpusDecoderState {
    pub fn new(profile: Profile) -> Result<Self, OpusCodecError> {
        let opus_profile = match profile.audio_codec() {
            AudioCodec::Opus(profile) => profile,
            AudioCodec::Codec2(_) => return Err(OpusCodecError::NonOpusProfile(profile)),
        };
        let channels = opus_profile.channels();
        let sample_rate = opus_profile.sample_rate();
        let sample_frames = profile.sample_frames_per_packet();
        let packet_layout = PacketLayout::new(sample_rate, sample_frames)?;
        let decoder = OpusDecoder::new(sample_rate as i32, usize::from(channels))
            .map_err(|err| OpusCodecError::Codec(err.to_string()))?;

        Ok(Self {
            profile,
            channels,
            sample_frames,
            subframe_count: packet_layout.subframe_count,
            subframe_sample_frames: packet_layout.subframe_sample_frames,
            decoder,
        })
    }

    pub const fn profile(&self) -> Profile {
        self.profile
    }

    pub const fn subframe_count(&self) -> usize {
        self.subframe_count
    }

    pub const fn subframe_sample_frames(&self) -> usize {
        self.subframe_sample_frames
    }

    pub fn decode_frame(&mut self, frame: &Frame) -> Result<RawAudioFrame, OpusCodecError> {
        if frame.codec != CodecKind::Opus {
            return Err(OpusCodecError::InvalidFrameCodec(frame.codec));
        }
        if self.subframe_count > 1 && frame.payload.first().is_some_and(|toc| toc & 0x03 == 0x03) {
            return self.decode_multi_subframe_packet(frame);
        }

        self.decode_direct_packet(frame)
    }

    fn decode_direct_packet(&mut self, frame: &Frame) -> Result<RawAudioFrame, OpusCodecError> {
        let mut samples = vec![0.0f32; self.sample_frames * usize::from(self.channels)];
        let decoded = self
            .decoder
            .decode(&frame.payload, self.sample_frames, &mut samples)
            .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
        samples.truncate(decoded * usize::from(self.channels));
        Ok(RawAudioFrame::new(self.channels, samples)?)
    }

    fn decode_multi_subframe_packet(
        &mut self,
        frame: &Frame,
    ) -> Result<RawAudioFrame, OpusCodecError> {
        let subpayloads = parse_code3_subframe_payloads(&frame.payload, self.subframe_count)?;
        let channels = usize::from(self.channels);
        let mut samples = vec![0.0f32; self.sample_frames * channels];
        let toc = frame.payload[0] & !0x03;

        for (index, subpayload) in subpayloads.iter().enumerate() {
            let start = index * self.subframe_sample_frames * channels;
            let end = start + self.subframe_sample_frames * channels;
            let mut subpacket = Vec::with_capacity(subpayload.len() + 1);
            subpacket.push(toc);
            subpacket.extend_from_slice(subpayload);
            self.decoder
                .decode(
                    &subpacket,
                    self.subframe_sample_frames,
                    &mut samples[start..end],
                )
                .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
        }

        Ok(RawAudioFrame::new(self.channels, samples)?)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketLayout {
    subframe_count: usize,
    subframe_sample_frames: usize,
}

impl PacketLayout {
    fn new(sample_rate_hz: u32, sample_frames: usize) -> Result<Self, OpusCodecError> {
        if supports_direct_frame(sample_rate_hz, sample_frames) {
            return Ok(Self {
                subframe_count: 1,
                subframe_sample_frames: sample_frames,
            });
        }

        let subframe_sample_frames = (sample_rate_hz as usize) / 50;
        if sample_frames != 0
            && subframe_sample_frames != 0
            && sample_frames % subframe_sample_frames == 0
            && supports_direct_frame(sample_rate_hz, subframe_sample_frames)
        {
            return Ok(Self {
                subframe_count: sample_frames / subframe_sample_frames,
                subframe_sample_frames,
            });
        }

        Err(OpusCodecError::UnsupportedFrameDuration {
            sample_rate_hz,
            sample_frames,
        })
    }
}

fn opus_application(application: OpusApplication) -> Application {
    match application {
        OpusApplication::Voip => Application::Voip,
        OpusApplication::Audio => Application::Audio,
    }
}

fn encode_sample_rate(profile: OpusProfile) -> u32 {
    match profile {
        // Python LXST uses libopus with an output byte ceiling but no fixed
        // bitrate or bandwidth CTLs. At Medium's 8 kbps ceiling, libopus is
        // free to pick lower voice bandwidth; opus-rs otherwise forces
        // superwideband/hybrid from the 24 kHz API rate, which is poor for
        // speech at this budget.
        OpusProfile::VoiceMedium => 16_000,
        _ => profile.sample_rate(),
    }
}

fn scale_sample_frames(
    sample_frames: usize,
    source_sample_rate: u32,
    encode_sample_rate: u32,
) -> Result<usize, OpusCodecError> {
    let numerator = sample_frames
        .checked_mul(encode_sample_rate as usize)
        .ok_or(OpusCodecError::UnsupportedFrameDuration {
            sample_rate_hz: encode_sample_rate,
            sample_frames,
        })?;
    let denominator = source_sample_rate as usize;
    if numerator % denominator != 0 {
        return Err(OpusCodecError::UnsupportedFrameDuration {
            sample_rate_hz: encode_sample_rate,
            sample_frames,
        });
    }
    Ok(numerator / denominator)
}

fn supports_direct_frame(sample_rate_hz: u32, sample_frames: usize) -> bool {
    sample_frames != 0 && (sample_rate_hz as usize) % sample_frames == 0
}

fn resample_interleaved_linear(
    input: &[f32],
    input_frames: usize,
    output_frames: usize,
    channels: usize,
) -> Vec<f32> {
    if input_frames == output_frames {
        return input.to_vec();
    }
    let mut output = vec![0.0f32; output_frames * channels];
    if input_frames == 0 || output_frames == 0 || channels == 0 {
        return output;
    }
    if input_frames == 1 {
        for frame in 0..output_frames {
            let out = frame * channels;
            output[out..out + channels].copy_from_slice(&input[..channels]);
        }
        return output;
    }

    let scale = input_frames as f64 / output_frames as f64;
    let max_input_index = input_frames - 1;
    for out_frame in 0..output_frames {
        let src = ((out_frame as f64 + 0.5) * scale - 0.5).clamp(0.0, max_input_index as f64);
        let left = src.floor() as usize;
        let right = (left + 1).min(max_input_index);
        let fraction = (src - left as f64) as f32;
        let out = out_frame * channels;
        let left_offset = left * channels;
        let right_offset = right * channels;
        for channel in 0..channels {
            let a = input[left_offset + channel];
            let b = input[right_offset + channel];
            output[out + channel] = a + (b - a) * fraction;
        }
    }
    output
}

fn push_subframe_payload_len(output: &mut Vec<u8>, len: usize) -> Result<(), OpusCodecError> {
    if len < 252 {
        output.push(len as u8);
        Ok(())
    } else if len <= 1275 {
        let first = 252 + (len % 4);
        output.push(first as u8);
        output.push(((len - first) / 4) as u8);
        Ok(())
    } else {
        Err(OpusCodecError::UnsupportedSubframePayloadLength(len))
    }
}

fn parse_code3_subframe_payloads(
    packet: &[u8],
    expected_count: usize,
) -> Result<Vec<&[u8]>, OpusCodecError> {
    if packet.len() < 2 {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 packet is too short",
        ));
    }

    let count_byte = packet[1];
    let frame_count = usize::from(count_byte & 0x3F);
    if frame_count != expected_count {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 frame count does not match the active profile",
        ));
    }
    if frame_count == 0 {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 frame count is zero",
        ));
    }

    let vbr = count_byte & 0x80 != 0;
    let padding = count_byte & 0x40 != 0;
    let mut cursor = 2;
    let mut payload_end = packet.len();

    if padding {
        let mut pad_len = 0usize;
        loop {
            if cursor >= packet.len() {
                return Err(OpusCodecError::MalformedPacket("padding exceeds packet"));
            }
            let byte = usize::from(packet[cursor]);
            cursor += 1;
            if byte == 255 {
                pad_len += 254;
            } else {
                pad_len += byte;
                break;
            }
        }
        payload_end = packet
            .len()
            .checked_sub(pad_len)
            .ok_or(OpusCodecError::MalformedPacket("padding exceeds packet"))?;
        if cursor > payload_end {
            return Err(OpusCodecError::MalformedPacket("padding exceeds payload"));
        }
    }

    if vbr {
        let mut lengths = Vec::with_capacity(frame_count);
        for _ in 0..frame_count - 1 {
            let (len, consumed) = read_subframe_payload_len(&packet[cursor..payload_end])?;
            cursor += consumed;
            lengths.push(len);
        }

        let declared_payload_bytes = lengths.iter().sum::<usize>();
        let remaining = payload_end
            .checked_sub(cursor)
            .ok_or(OpusCodecError::MalformedPacket(
                "payload cursor exceeds packet",
            ))?;
        if declared_payload_bytes > remaining {
            return Err(OpusCodecError::MalformedPacket(
                "declared frame lengths exceed packet payload",
            ));
        }
        lengths.push(remaining - declared_payload_bytes);

        let mut payloads = Vec::with_capacity(frame_count);
        let mut payload_cursor = cursor;
        for len in lengths {
            let next = payload_cursor + len;
            payloads.push(&packet[payload_cursor..next]);
            payload_cursor = next;
        }
        Ok(payloads)
    } else {
        let compressed = &packet[cursor..payload_end];
        if compressed.len() % frame_count != 0 {
            return Err(OpusCodecError::MalformedPacket(
                "CBR code 3 payload is not evenly divisible",
            ));
        }
        let frame_len = compressed.len() / frame_count;
        Ok(compressed.chunks(frame_len).collect())
    }
}

fn read_subframe_payload_len(input: &[u8]) -> Result<(usize, usize), OpusCodecError> {
    let first = *input
        .first()
        .ok_or(OpusCodecError::MalformedPacket("missing frame length"))?;
    if first < 252 {
        Ok((usize::from(first), 1))
    } else {
        let second = *input
            .get(1)
            .ok_or(OpusCodecError::MalformedPacket("truncated frame length"))?;
        Ok((usize::from(first) + 4 * usize::from(second), 2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntheticSourceKind;

    fn source_for(profile: Profile) -> crate::SyntheticSource {
        crate::SyntheticSource::new(
            profile.channels(),
            profile.sample_rate_hz(),
            profile.sample_frames_per_packet(),
            SyntheticSourceKind::Sine {
                frequency_hz: 440.0,
                amplitude: 0.25,
            },
        )
        .unwrap()
    }

    #[test]
    fn opus_encoder_rejects_codec2_profiles() {
        assert!(matches!(
            OpusEncoderState::new(Profile::BandwidthLow),
            Err(OpusCodecError::NonOpusProfile(Profile::BandwidthLow))
        ));
    }

    #[test]
    fn opus_profile_encoder_caps_payload_to_python_budget() {
        let profile = Profile::LatencyLow;
        let mut encoder = OpusEncoderState::new(profile).unwrap();
        let frame = source_for(profile).next_raw_frame().unwrap();

        let encoded = encoder.encode_frame(&frame).unwrap();
        assert_eq!(encoded.codec, CodecKind::Opus);
        assert!(encoded.payload.len() <= profile.opus_payload_ceiling_bytes().unwrap());
    }

    #[test]
    fn opus_roundtrip_decodes_profile_shaped_pcm() {
        let profile = Profile::LatencyLow;
        let mut source = source_for(profile);
        let frame = source.next_raw_frame().unwrap();
        let mut encoder = OpusEncoderState::new(profile).unwrap();
        let mut decoder = OpusDecoderState::new(profile).unwrap();

        let encoded = encoder.encode_frame(&frame).unwrap();
        let decoded = decoder.decode_frame(&encoded).unwrap();

        assert_eq!(decoded.channels, profile.channels());
        assert_eq!(decoded.sample_frames(), profile.sample_frames_per_packet());
        assert_eq!(decoder.profile(), profile);
    }

    #[test]
    fn opus_quality_profiles_encode_sixty_ms_as_three_subframes() {
        for profile in [
            Profile::QualityMedium,
            Profile::QualityHigh,
            Profile::QualityMax,
        ] {
            let mut source = source_for(profile);
            let frame = source.next_raw_frame().unwrap();
            let mut encoder = OpusEncoderState::new(profile).unwrap();
            let mut decoder = OpusDecoderState::new(profile).unwrap();

            assert_eq!(encoder.subframe_count(), 3);
            assert_eq!(decoder.subframe_count(), 3);
            assert_eq!(
                encoder.subframe_sample_frames() * 3,
                encoder.sample_frames()
            );

            let encoded = encoder.encode_frame(&frame).unwrap();
            assert_eq!(encoded.codec, CodecKind::Opus);
            assert_eq!(encoded.payload[0] & 0x03, 0x03);
            assert_eq!(encoded.payload[1] & 0x3F, 3);
            assert_ne!(encoded.payload[1] & 0x80, 0);
            assert!(encoded.payload.len() <= profile.opus_payload_ceiling_bytes().unwrap());

            let decoded = decoder.decode_frame(&encoded).unwrap();
            assert_eq!(decoded.channels, profile.channels());
            assert_eq!(decoded.sample_frames(), profile.sample_frames_per_packet());
        }
    }

    #[test]
    fn opus_medium_uses_wideband_silk_at_low_bitrate() {
        let profile = Profile::QualityMedium;
        let mut source = source_for(profile);
        let frame = source.next_raw_frame().unwrap();
        let mut encoder = OpusEncoderState::new(profile).unwrap();

        let encoded = encoder.encode_frame(&frame).unwrap();

        assert_eq!(encoded.payload[0] & !0x03, 0x48);
        assert!(encoded.payload.len() <= profile.opus_payload_ceiling_bytes().unwrap());
    }
}
