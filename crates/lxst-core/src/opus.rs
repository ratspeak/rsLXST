use opus_rs::{Application, OpusDecoder, OpusEncoder};
use thiserror::Error;

use crate::{AudioCodec, CodecKind, Frame, OpusApplication, OpusProfile, Profile, RawAudioFrame};

/// The fixed output clock used by the interoperable Opus packet decoder.
pub const OPUS_MONO_DECODE_SAMPLE_RATE_HZ: u32 = 48_000;

/// The maximum encoded size of one frame contained in an Opus packet.
pub const OPUS_ENCODED_FRAME_MAX_BYTES: usize = 1_275;

/// The maximum encoded Opus packet accepted by the interoperable decoder.
///
/// RFC 7845 bounds one Opus packet at 61,440 encoded bytes. This is distinct from the 1,275-byte
/// limit for each frame inside the packet and from the bounded decoded PCM allocation.
pub const OPUS_ENCODED_PACKET_MAX_BYTES: usize = 61_440;

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
    #[error("encoded Opus packet length {actual} exceeds the {maximum}-byte ceiling")]
    EncodedPacketTooLarge { actual: usize, maximum: usize },
    #[error("encoded Opus frame {index} length {actual} exceeds the {maximum}-byte ceiling")]
    EncodedFrameTooLarge {
        index: usize,
        actual: usize,
        maximum: usize,
    },
    #[error("unsupported Opus packet duration: {samples_48k} samples at 48 kHz")]
    UnsupportedPacketDuration { samples_48k: usize },
    #[error(
        "Opus decoder returned {actual} sample frames for a packet frame containing {expected}"
    )]
    DecodedPacketDurationMismatch { expected: usize, actual: usize },
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

/// A stateful decoder for interoperable mono Opus packets at the Opus 48 kHz clock.
///
/// Unlike [`OpusDecoderState`], this decoder is not tied to an LXST call profile. It accepts
/// every structurally legal packet duration from 2.5 through 120 milliseconds and preserves
/// decoder state across calls. The returned [`RawAudioFrame`] is always mono at 48 kHz.
pub struct OpusMonoDecoder {
    decoder: OpusDecoder,
}

impl OpusMonoDecoder {
    pub fn new() -> Result<Self, OpusCodecError> {
        let decoder = OpusDecoder::new(OPUS_MONO_DECODE_SAMPLE_RATE_HZ as i32, 1)
            .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
        Ok(Self { decoder })
    }

    pub const fn sample_rate_hz(&self) -> u32 {
        OPUS_MONO_DECODE_SAMPLE_RATE_HZ
    }

    pub const fn channels(&self) -> u8 {
        1
    }

    /// Decodes one complete Opus packet to mono PCM at 48 kHz.
    pub fn decode_packet(&mut self, packet: &[u8]) -> Result<RawAudioFrame, OpusCodecError> {
        let inspected = inspect_opus_packet(packet)?;
        if inspected.channels != 1 {
            return Err(OpusCodecError::ChannelMismatch {
                expected: 1,
                actual: inspected.channels,
            });
        }

        let mut samples = Vec::with_capacity(inspected.duration_samples_48k);
        let direct_toc = inspected.toc & !0x03;
        for payload in inspected.frames {
            let mut direct_packet = Vec::with_capacity(payload.len() + 1);
            direct_packet.push(direct_toc);
            direct_packet.extend_from_slice(payload);

            let output_start = samples.len();
            samples.resize(output_start + inspected.samples_per_frame_48k, 0.0);
            let decoded = self
                .decoder
                .decode(
                    &direct_packet,
                    inspected.samples_per_frame_48k,
                    &mut samples[output_start..],
                )
                .map_err(|err| OpusCodecError::Codec(err.to_string()))?;
            if decoded != inspected.samples_per_frame_48k {
                return Err(OpusCodecError::DecodedPacketDurationMismatch {
                    expected: inspected.samples_per_frame_48k,
                    actual: decoded,
                });
            }
        }

        debug_assert_eq!(samples.len(), inspected.duration_samples_48k);
        Ok(RawAudioFrame::new(1, samples)?)
    }
}

/// Returns a supported packet's total duration in samples at the Opus 48 kHz clock.
///
/// The packet is structurally inspected using the framing rules from RFC 6716. Every legal
/// duration from 2.5 through 120 milliseconds is accepted in 2.5 millisecond increments.
pub fn opus_packet_duration_samples_48k(packet: &[u8]) -> Result<usize, OpusCodecError> {
    Ok(inspect_opus_packet(packet)?.duration_samples_48k)
}

#[derive(Debug)]
struct InspectedOpusPacket<'a> {
    toc: u8,
    channels: u8,
    samples_per_frame_48k: usize,
    duration_samples_48k: usize,
    frames: Vec<&'a [u8]>,
}

fn inspect_opus_packet(packet: &[u8]) -> Result<InspectedOpusPacket<'_>, OpusCodecError> {
    if packet.is_empty() {
        return Err(OpusCodecError::MalformedPacket("packet is empty"));
    }
    if packet.len() > OPUS_ENCODED_PACKET_MAX_BYTES {
        return Err(OpusCodecError::EncodedPacketTooLarge {
            actual: packet.len(),
            maximum: OPUS_ENCODED_PACKET_MAX_BYTES,
        });
    }

    let toc = packet[0];
    let samples_per_frame_48k = opus_samples_per_frame_48k(toc);
    let frames = parse_opus_packet_frames(packet)?;
    for (index, frame) in frames.iter().enumerate() {
        if frame.len() > OPUS_ENCODED_FRAME_MAX_BYTES {
            return Err(OpusCodecError::EncodedFrameTooLarge {
                index,
                actual: frame.len(),
                maximum: OPUS_ENCODED_FRAME_MAX_BYTES,
            });
        }
    }
    let duration_samples_48k = samples_per_frame_48k
        .checked_mul(frames.len())
        .ok_or(OpusCodecError::MalformedPacket("packet duration overflows"))?;
    if duration_samples_48k > 5_760 {
        return Err(OpusCodecError::MalformedPacket(
            "packet duration exceeds 120 milliseconds",
        ));
    }
    if duration_samples_48k == 0 || !duration_samples_48k.is_multiple_of(120) {
        return Err(OpusCodecError::UnsupportedPacketDuration {
            samples_48k: duration_samples_48k,
        });
    }

    Ok(InspectedOpusPacket {
        toc,
        channels: if toc & 0x04 == 0 { 1 } else { 2 },
        samples_per_frame_48k,
        duration_samples_48k,
        frames,
    })
}

// Straight from libopus opus_packet_get_samples_per_frame() for the fixed 48 kHz clock.
fn opus_samples_per_frame_48k(toc: u8) -> usize {
    if toc & 0x80 != 0 {
        let config = usize::from((toc >> 3) & 0x03);
        ((OPUS_MONO_DECODE_SAMPLE_RATE_HZ as usize) << config) / 400
    } else if toc & 0x60 == 0x60 {
        if toc & 0x08 != 0 { 960 } else { 480 }
    } else {
        let config = usize::from((toc >> 3) & 0x03);
        if config == 3 {
            2_880
        } else {
            ((OPUS_MONO_DECODE_SAMPLE_RATE_HZ as usize) << config) / 100
        }
    }
}

fn parse_opus_packet_frames(packet: &[u8]) -> Result<Vec<&[u8]>, OpusCodecError> {
    let toc = packet[0];
    let body = &packet[1..];
    match toc & 0x03 {
        0 => Ok(vec![body]),
        1 => {
            if !body.len().is_multiple_of(2) {
                return Err(OpusCodecError::MalformedPacket(
                    "code 1 CBR payload has odd length",
                ));
            }
            let frame_len = body.len() / 2;
            Ok(vec![&body[..frame_len], &body[frame_len..]])
        }
        2 => {
            let (first_len, length_bytes) = read_subframe_payload_len(body)?;
            let payload = &body[length_bytes..];
            if first_len > payload.len() {
                return Err(OpusCodecError::MalformedPacket(
                    "code 2 first frame exceeds packet payload",
                ));
            }
            Ok(vec![&payload[..first_len], &payload[first_len..]])
        }
        3 => parse_code3_packet_frames(packet),
        _ => unreachable!(),
    }
}

fn parse_code3_packet_frames(packet: &[u8]) -> Result<Vec<&[u8]>, OpusCodecError> {
    let count_byte = *packet.get(1).ok_or(OpusCodecError::MalformedPacket(
        "code 3 packet is too short",
    ))?;
    let frame_count = usize::from(count_byte & 0x3F);
    if frame_count == 0 {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 frame count is zero",
        ));
    }
    if frame_count > 48 {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 frame count exceeds 48",
        ));
    }

    let mut cursor = 2usize;
    let mut payload_end = packet.len();
    if count_byte & 0x40 != 0 {
        let mut padding = 0usize;
        loop {
            let byte = usize::from(*packet.get(cursor).ok_or(OpusCodecError::MalformedPacket(
                "code 3 padding exceeds packet",
            ))?);
            cursor += 1;
            padding = padding
                .checked_add(if byte == 255 { 254 } else { byte })
                .ok_or(OpusCodecError::MalformedPacket(
                    "code 3 padding length overflows",
                ))?;
            if byte != 255 {
                break;
            }
        }
        payload_end = packet
            .len()
            .checked_sub(padding)
            .ok_or(OpusCodecError::MalformedPacket(
                "code 3 padding exceeds packet",
            ))?;
        if cursor > payload_end {
            return Err(OpusCodecError::MalformedPacket(
                "code 3 padding exceeds payload",
            ));
        }
    }

    if count_byte & 0x80 == 0 {
        let payload = &packet[cursor..payload_end];
        if !payload.len().is_multiple_of(frame_count) {
            return Err(OpusCodecError::MalformedPacket(
                "code 3 CBR payload is not evenly divisible",
            ));
        }
        let frame_len = payload.len() / frame_count;
        return Ok((0..frame_count)
            .map(|index| {
                let start = index * frame_len;
                &payload[start..start + frame_len]
            })
            .collect());
    }

    let mut lengths = Vec::with_capacity(frame_count);
    for _ in 0..frame_count - 1 {
        let (frame_len, length_bytes) = read_subframe_payload_len(&packet[cursor..payload_end])?;
        cursor += length_bytes;
        lengths.push(frame_len);
    }
    let declared = lengths.iter().sum::<usize>();
    let remaining = payload_end
        .checked_sub(cursor)
        .ok_or(OpusCodecError::MalformedPacket(
            "code 3 payload cursor exceeds packet",
        ))?;
    if declared > remaining {
        return Err(OpusCodecError::MalformedPacket(
            "code 3 declared frame lengths exceed packet payload",
        ));
    }
    lengths.push(remaining - declared);

    let mut frames = Vec::with_capacity(frame_count);
    let mut payload_cursor = cursor;
    for frame_len in lengths {
        let next = payload_cursor + frame_len;
        frames.push(&packet[payload_cursor..next]);
        payload_cursor = next;
    }
    Ok(frames)
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
            && sample_frames.is_multiple_of(subframe_sample_frames)
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
    if !numerator.is_multiple_of(denominator) {
        return Err(OpusCodecError::UnsupportedFrameDuration {
            sample_rate_hz: encode_sample_rate,
            sample_frames,
        });
    }
    Ok(numerator / denominator)
}

fn supports_direct_frame(sample_rate_hz: u32, sample_frames: usize) -> bool {
    sample_frames != 0 && (sample_rate_hz as usize).is_multiple_of(sample_frames)
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
        if !compressed.len().is_multiple_of(frame_count) {
            return Err(OpusCodecError::MalformedPacket(
                "CBR code 3 payload is not evenly divisible",
            ));
        }
        let frame_len = compressed.len() / frame_count;
        // An empty payload passes the divisibility check with frame_len 0,
        // and chunks(0) panics — reject before reaching it.
        if frame_len == 0 {
            return Err(OpusCodecError::MalformedPacket(
                "CBR code 3 payload is empty",
            ));
        }
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

    #[test]
    fn upstream_opus_state_remains_heap_backed() {
        assert!(std::mem::size_of::<OpusEncoder>() < 4_096);
        assert!(std::mem::size_of::<OpusDecoder>() < 4_096);
    }

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

    fn encoded_mono_packet(sample_frames: usize, bitrate_bps: i32) -> Vec<u8> {
        let samples = (0..sample_frames)
            .map(|index| {
                let phase = std::f32::consts::TAU * 440.0 * index as f32
                    / OPUS_MONO_DECODE_SAMPLE_RATE_HZ as f32;
                phase.sin() * 0.2
            })
            .collect::<Vec<_>>();
        let mut encoder = OpusEncoder::new(
            OPUS_MONO_DECODE_SAMPLE_RATE_HZ as i32,
            1,
            Application::RestrictedLowDelay,
        )
        .unwrap();
        encoder.bitrate_bps = bitrate_bps;
        encoder.use_cbr = true;
        let mut packet = vec![0u8; OPUS_ENCODED_FRAME_MAX_BYTES + 1];
        let written = encoder
            .encode(&samples, sample_frames, &mut packet)
            .unwrap();
        packet.truncate(written);
        assert_eq!(packet[0] & 0x03, 0);
        assert_eq!(opus_samples_per_frame_48k(packet[0]), sample_frames);
        packet
    }

    fn encoded_twenty_ms_mono_packet() -> Vec<u8> {
        encoded_mono_packet(960, 16_000)
    }

    fn repacketize_cbr(packet: &[u8], frame_count: usize) -> Vec<u8> {
        assert!((1..=48).contains(&frame_count));
        let toc = packet[0] & !0x03;
        let payload = &packet[1..];
        let mut combined = Vec::new();
        match frame_count {
            1 => combined.push(toc),
            2 => combined.push(toc | 0x01),
            _ => {
                combined.push(toc | 0x03);
                combined.push(frame_count as u8);
            }
        }
        for _ in 0..frame_count {
            combined.extend_from_slice(payload);
        }
        combined
    }

    fn repacketize_vbr(packet: &[u8], frame_count: usize, padding: usize) -> Vec<u8> {
        assert!((2..=48).contains(&frame_count));
        assert!(padding < 255);
        let toc = packet[0] & !0x03;
        let payload = &packet[1..];
        let mut combined = Vec::new();
        if frame_count == 2 && padding == 0 {
            combined.push(toc | 0x02);
            push_subframe_payload_len(&mut combined, payload.len()).unwrap();
        } else {
            combined.push(toc | 0x03);
            combined.push(0x80 | (frame_count as u8) | if padding == 0 { 0 } else { 0x40 });
            if padding != 0 {
                combined.push(padding as u8);
            }
            for _ in 0..frame_count - 1 {
                push_subframe_payload_len(&mut combined, payload.len()).unwrap();
            }
        }
        for _ in 0..frame_count {
            combined.extend_from_slice(payload);
        }
        combined.resize(combined.len() + padding, 0);
        combined
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

    /// T0-3: a two-byte CBR code-3 frame (empty payload) passed the
    /// divisibility check with frame_len 0 and panicked in chunks(0) —
    /// a remote crash vector for any call peer.
    #[test]
    fn opus_decoder_rejects_empty_cbr_code3_payload() {
        let mut decoder = OpusDecoderState::new(Profile::QualityMedium).unwrap();
        let hostile = Frame::new(CodecKind::Opus, [0x03, 0x03]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decoder.decode_frame(&hostile)
        }));

        let decode_result = result.expect("decode must not panic");
        assert!(matches!(
            decode_result,
            Err(OpusCodecError::MalformedPacket(
                "CBR code 3 payload is empty"
            ))
        ));
    }

    #[test]
    fn opus_decoder_accepts_direct_sixty_ms_mediumband_without_panicking() {
        let mut decoder = OpusDecoderState::new(Profile::QualityMedium).unwrap();
        let mediumband_silk = Frame::new(CodecKind::Opus, [0x38, 0x00, 0x00]);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decoder.decode_frame(&mediumband_silk)
        }));

        assert!(result.is_ok());
        let decoded = result.unwrap().unwrap();
        assert_eq!(decoded.channels, Profile::QualityMedium.channels());
        assert_eq!(
            decoded.sample_frames(),
            Profile::QualityMedium.sample_frames_per_packet()
        );
    }

    #[test]
    fn interoperable_packet_inspection_follows_rfc_vbr_lengths_and_padding() {
        let toc = 0xF8;

        let mut code_two = vec![toc | 0x02, 252, 0];
        code_two.extend(std::iter::repeat_n(0, 253));
        assert_eq!(opus_packet_duration_samples_48k(&code_two).unwrap(), 1_920);

        let padded_code_three = [toc | 0x03, 0x43, 1, 0, 0, 0, 0];
        assert_eq!(
            opus_packet_duration_samples_48k(&padded_code_three).unwrap(),
            2_880
        );
    }

    #[test]
    fn interoperable_packet_inspection_accepts_every_two_and_half_ms_quantum() {
        let toc = 0x80;
        for frame_count in 1..=48 {
            let packet = match frame_count {
                1 => vec![toc],
                2 => vec![toc | 0x01],
                _ => vec![toc | 0x03, frame_count as u8],
            };
            assert_eq!(
                opus_packet_duration_samples_48k(&packet),
                Ok(frame_count * 120)
            );
        }
    }

    #[test]
    fn interoperable_decoder_decodes_rfc_duration_range_at_the_48khz_clock() {
        let two_and_half = encoded_mono_packet(120, 64_000);
        let five = encoded_mono_packet(240, 64_000);
        let ten = encoded_mono_packet(480, 64_000);
        let twenty = encoded_twenty_ms_mono_packet();
        let packets = [
            (two_and_half, 120),
            (five, 240),
            (ten.clone(), 480),
            (twenty.clone(), 960),
            (repacketize_cbr(&ten, 3), 1_440),
            (repacketize_cbr(&twenty, 2), 1_920),
            (repacketize_cbr(&twenty, 3), 2_880),
            (repacketize_cbr(&twenty, 4), 3_840),
            (repacketize_cbr(&twenty, 5), 4_800),
            (repacketize_cbr(&twenty, 6), 5_760),
        ];
        let mut decoder = OpusMonoDecoder::new().unwrap();
        assert_eq!(decoder.sample_rate_hz(), 48_000);
        assert_eq!(decoder.channels(), 1);

        for (packet, expected_sample_frames) in packets {
            assert!(packet.len() <= OPUS_ENCODED_PACKET_MAX_BYTES);
            assert_eq!(
                opus_packet_duration_samples_48k(&packet),
                Ok(expected_sample_frames)
            );
            let decoded = decoder.decode_packet(&packet).unwrap();
            assert_eq!(decoded.channels, 1);
            assert_eq!(decoded.sample_frames(), expected_sample_frames);
        }
    }

    #[test]
    fn interoperable_decoder_accepts_mixed_rfc_packet_framing() {
        let direct = encoded_twenty_ms_mono_packet();
        let packets = [
            (direct.clone(), 960),
            (repacketize_cbr(&direct, 2), 1_920),
            (repacketize_vbr(&direct, 2, 0), 1_920),
            (repacketize_cbr(&direct, 3), 2_880),
            (repacketize_vbr(&direct, 3, 7), 2_880),
        ];
        let mut decoder = OpusMonoDecoder::new().unwrap();

        for (packet, expected_sample_frames) in packets {
            assert_eq!(
                opus_packet_duration_samples_48k(&packet),
                Ok(expected_sample_frames)
            );
            assert_eq!(
                decoder.decode_packet(&packet).unwrap().sample_frames(),
                expected_sample_frames
            );
        }
    }

    #[test]
    fn interoperable_decoder_accepts_code3_zero_byte_plc_frames() {
        let toc = encoded_twenty_ms_mono_packet()[0] & !0x03;
        let packets = [vec![toc | 0x03, 0x03], vec![toc | 0x03, 0x83, 0, 0]];
        let mut decoder = OpusMonoDecoder::new().unwrap();

        for packet in packets {
            assert_eq!(opus_packet_duration_samples_48k(&packet), Ok(2_880));
            assert_eq!(
                decoder.decode_packet(&packet).unwrap().sample_frames(),
                2_880
            );
        }
    }

    #[test]
    fn interoperable_decoder_accepts_compound_packets_above_one_frame_ceiling() {
        let direct = encoded_mono_packet(960, 510_000);
        assert!(direct.len() - 1 <= OPUS_ENCODED_FRAME_MAX_BYTES);

        let compound = repacketize_cbr(&direct, 2);
        assert!(compound.len() > OPUS_ENCODED_FRAME_MAX_BYTES);
        assert!(compound.len() <= OPUS_ENCODED_PACKET_MAX_BYTES);
        assert_eq!(opus_packet_duration_samples_48k(&compound), Ok(1_920));

        let decoded = OpusMonoDecoder::new()
            .unwrap()
            .decode_packet(&compound)
            .unwrap();
        assert_eq!(decoded.sample_frames(), 1_920);
    }

    #[test]
    fn interoperable_decoder_accepts_current_quality_medium_packets() {
        let profile = Profile::QualityMedium;
        let frame = source_for(profile).next_raw_frame().unwrap();
        let packet = OpusEncoderState::new(profile)
            .unwrap()
            .encode_frame(&frame)
            .unwrap();

        let decoded = OpusMonoDecoder::new()
            .unwrap()
            .decode_packet(&packet.payload)
            .unwrap();

        assert_eq!(opus_packet_duration_samples_48k(&packet.payload), Ok(2_880));
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.sample_frames(), 2_880);
    }

    #[test]
    fn interoperable_decoder_rejects_stereo_packets_before_decoding() {
        let stereo_twenty_ms = [0xFC, 0];
        assert_eq!(opus_packet_duration_samples_48k(&stereo_twenty_ms), Ok(960));
        assert!(matches!(
            OpusMonoDecoder::new()
                .unwrap()
                .decode_packet(&stereo_twenty_ms),
            Err(OpusCodecError::ChannelMismatch {
                expected: 1,
                actual: 2
            })
        ));
    }

    #[test]
    fn interoperable_packet_policy_enforces_frame_packet_and_duration_bounds() {
        let oversize = vec![0xF8; OPUS_ENCODED_PACKET_MAX_BYTES + 1];
        assert_eq!(
            opus_packet_duration_samples_48k(&oversize),
            Err(OpusCodecError::EncodedPacketTooLarge {
                actual: OPUS_ENCODED_PACKET_MAX_BYTES + 1,
                maximum: OPUS_ENCODED_PACKET_MAX_BYTES,
            })
        );

        let oversized_direct_frame = vec![0xF8; OPUS_ENCODED_FRAME_MAX_BYTES + 2];
        assert_eq!(
            opus_packet_duration_samples_48k(&oversized_direct_frame),
            Err(OpusCodecError::EncodedFrameTooLarge {
                index: 0,
                actual: OPUS_ENCODED_FRAME_MAX_BYTES + 1,
                maximum: OPUS_ENCODED_FRAME_MAX_BYTES,
            })
        );

        let overlong_twenty_ms = [0xFB, 0x07, 0, 0, 0, 0, 0, 0, 0];
        assert!(matches!(
            opus_packet_duration_samples_48k(&overlong_twenty_ms),
            Err(OpusCodecError::MalformedPacket(
                "packet duration exceeds 120 milliseconds"
            ))
        ));

        let too_many_two_and_half_ms = [0x83, 49];
        assert!(matches!(
            opus_packet_duration_samples_48k(&too_many_two_and_half_ms),
            Err(OpusCodecError::MalformedPacket(
                "code 3 frame count exceeds 48"
            ))
        ));
    }

    #[test]
    fn interoperable_packet_inspection_rejects_malformed_framing() {
        let malformed = [
            Vec::new(),
            vec![0xFB],
            vec![0xF9, 0],
            vec![0xFA, 252],
            vec![0xFB, 0],
            vec![0xFB, 0x03, 0, 0],
            vec![0xFB, 0x43, 2, 0],
        ];

        for packet in malformed {
            assert!(matches!(
                opus_packet_duration_samples_48k(&packet),
                Err(OpusCodecError::MalformedPacket(_))
            ));
        }
    }
}
