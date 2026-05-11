use rmpv::Value;
use rmpv::decode::read_value;
use rmpv::encode::write_value;
use thiserror::Error;

use crate::profile::{Profile, SignallingStatus};

pub const FIELD_SIGNALLING: u8 = 0x00;
pub const FIELD_FRAMES: u8 = 0x01;

const PREFERRED_PROFILE_BASE: u32 = 0xFF;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("msgpack decode error: {0}")]
    Decode(String),
    #[error("msgpack encode error: {0}")]
    Encode(String),
    #[error("LXST packet root must be a msgpack map")]
    RootNotMap,
    #[error("LXST field key must be a non-negative integer")]
    InvalidFieldKey,
    #[error("LXST field {field:#04x} has invalid value type")]
    InvalidFieldType { field: u8 },
    #[error("LXST signal value must be a non-negative integer")]
    InvalidSignal,
    #[error("LXST frame must contain a codec header byte")]
    EmptyFrame,
    #[error("unknown LXST codec id {0:#04x}")]
    UnknownCodec(u8),
    #[error("LXST codec {0:?} is not transmittable as a media frame")]
    NonTransmittableCodec(CodecKind),
    #[error("expected raw codec frame, got {0:?}")]
    InvalidRawFrameCodec(CodecKind),
    #[error("invalid raw channel count {0}; expected 1..=64")]
    InvalidRawChannels(u8),
    #[error("unknown raw bit-depth header {0}")]
    UnknownRawBitDepth(u8),
    #[error("raw payload is empty; expected one header byte plus sample data")]
    EmptyRawPayload,
    #[error("raw payload sample bytes are not aligned to {bytes_per_sample}-byte samples")]
    InvalidRawSampleBytes { bytes_per_sample: usize },
    #[error("raw sample count {samples} is not divisible by channel count {channels}")]
    InvalidRawSampleCount { samples: usize, channels: u8 },
    #[error("unknown Codec2 mode header {0:#04x}")]
    UnknownCodec2Mode(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CodecKind {
    Raw = 0x00,
    Opus = 0x01,
    Codec2 = 0x02,
    Null = 0xFF,
}

impl CodecKind {
    pub const fn wire_id(self) -> u8 {
        self as u8
    }

    pub const fn from_wire(id: u8) -> Result<Self, Error> {
        match id {
            0x00 => Ok(Self::Raw),
            0x01 => Ok(Self::Opus),
            0x02 => Ok(Self::Codec2),
            0xFF => Ok(Self::Null),
            other => Err(Error::UnknownCodec(other)),
        }
    }

    pub const fn is_transmittable(self) -> bool {
        !matches!(self, Self::Null)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    Status(SignallingStatus),
    PreferredProfile(Profile),
    Raw(u32),
}

impl Signal {
    pub const fn wire_value(self) -> u32 {
        match self {
            Self::Status(status) => status.wire_value(),
            Self::PreferredProfile(profile) => PREFERRED_PROFILE_BASE + profile.wire_value(),
            Self::Raw(value) => value,
        }
    }

    pub const fn from_wire(value: u32) -> Self {
        if let Some(status) = SignallingStatus::from_wire(value) {
            Self::Status(status)
        } else if value >= PREFERRED_PROFILE_BASE {
            let profile_value = value - PREFERRED_PROFILE_BASE;
            if let Some(profile) = Profile::from_wire(profile_value) {
                Self::PreferredProfile(profile)
            } else {
                Self::Raw(value)
            }
        } else {
            Self::Raw(value)
        }
    }
}

impl From<SignallingStatus> for Signal {
    fn from(value: SignallingStatus) -> Self {
        Self::Status(value)
    }
}

impl From<Profile> for Signal {
    fn from(value: Profile) -> Self {
        Self::PreferredProfile(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub codec: CodecKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(codec: CodecKind, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            codec,
            payload: payload.into(),
        }
    }

    pub fn from_wire_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let Some((&codec_id, payload)) = bytes.split_first() else {
            return Err(Error::EmptyFrame);
        };

        let codec = CodecKind::from_wire(codec_id)?;
        if !codec.is_transmittable() {
            return Err(Error::NonTransmittableCodec(codec));
        }

        Ok(Self {
            codec,
            payload: payload.to_vec(),
        })
    }

    pub fn to_wire_bytes(&self) -> Result<Vec<u8>, Error> {
        if !self.codec.is_transmittable() {
            return Err(Error::NonTransmittableCodec(self.codec));
        }

        let mut bytes = Vec::with_capacity(1 + self.payload.len());
        bytes.push(self.codec.wire_id());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LxstPacket {
    pub signals: Vec<Signal>,
    pub frames: Vec<Frame>,
}

impl LxstPacket {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn signalling(signals: impl IntoIterator<Item = Signal>) -> Self {
        Self {
            signals: signals.into_iter().collect(),
            frames: Vec::new(),
        }
    }

    pub fn frame(frame: Frame) -> Self {
        Self {
            signals: Vec::new(),
            frames: vec![frame],
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut map = Vec::with_capacity(2);

        if !self.signals.is_empty() {
            let signals = self
                .signals
                .iter()
                .map(|signal| Value::from(signal.wire_value() as u64))
                .collect();
            map.push((Value::from(FIELD_SIGNALLING as u64), Value::Array(signals)));
        }

        if !self.frames.is_empty() {
            let value = if self.frames.len() == 1 {
                Value::Binary(self.frames[0].to_wire_bytes()?)
            } else {
                Value::Array(
                    self.frames
                        .iter()
                        .map(|frame| frame.to_wire_bytes().map(Value::Binary))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            };
            map.push((Value::from(FIELD_FRAMES as u64), value));
        }

        let mut out = Vec::new();
        write_value(&mut out, &Value::Map(map)).map_err(|e| Error::Encode(e.to_string()))?;
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let value = read_value(&mut &bytes[..]).map_err(|e| Error::Decode(e.to_string()))?;
        Self::from_value(value)
    }

    fn from_value(value: Value) -> Result<Self, Error> {
        let Value::Map(entries) = value else {
            return Err(Error::RootNotMap);
        };

        let mut packet = Self::new();
        for (key, value) in entries {
            let Some(field) = integer_value(&key).and_then(|v| u8::try_from(v).ok()) else {
                return Err(Error::InvalidFieldKey);
            };

            match field {
                FIELD_SIGNALLING => {
                    packet.signals.extend(parse_signals(value)?);
                }
                FIELD_FRAMES => {
                    packet.frames.extend(parse_frames(value)?);
                }
                _ => {}
            }
        }

        Ok(packet)
    }
}

fn parse_signals(value: Value) -> Result<Vec<Signal>, Error> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(parse_signal)
            .collect::<Result<Vec<_>, _>>(),
        other => Ok(vec![parse_signal(&other)?]),
    }
}

fn parse_signal(value: &Value) -> Result<Signal, Error> {
    let Some(raw) = integer_value(value).and_then(|v| u32::try_from(v).ok()) else {
        return Err(Error::InvalidSignal);
    };
    Ok(Signal::from_wire(raw))
}

fn parse_frames(value: Value) -> Result<Vec<Frame>, Error> {
    match value {
        Value::Binary(bytes) => Ok(vec![Frame::from_wire_bytes(&bytes)?]),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::Binary(bytes) => Frame::from_wire_bytes(&bytes),
                _ => Err(Error::InvalidFieldType {
                    field: FIELD_FRAMES,
                }),
            })
            .collect(),
        _ => Err(Error::InvalidFieldType {
            field: FIELD_FRAMES,
        }),
    }
}

fn integer_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RawBitDepth {
    Float16 = 0x00,
    Float32 = 0x01,
    Float64 = 0x02,
    Float128 = 0x03,
}

impl RawBitDepth {
    pub const fn from_header_bits(bits: u8) -> Result<Self, Error> {
        match bits {
            0x00 => Ok(Self::Float16),
            0x01 => Ok(Self::Float32),
            0x02 => Ok(Self::Float64),
            0x03 => Ok(Self::Float128),
            other => Err(Error::UnknownRawBitDepth(other)),
        }
    }

    pub const fn bits(self) -> u16 {
        match self {
            Self::Float16 => 16,
            Self::Float32 => 32,
            Self::Float64 => 64,
            Self::Float128 => 128,
        }
    }

    pub const fn bytes_per_sample(self) -> usize {
        (self.bits() as usize) / 8
    }

    pub const fn numpy_dtype(self) -> &'static str {
        match self {
            Self::Float16 => "float16",
            Self::Float32 => "float32",
            Self::Float64 => "float64",
            Self::Float128 => "float128",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawFrameHeader {
    pub channels: u8,
    pub bit_depth: RawBitDepth,
}

impl RawFrameHeader {
    pub const fn new(channels: u8, bit_depth: RawBitDepth) -> Result<Self, Error> {
        if channels == 0 || channels > 64 {
            Err(Error::InvalidRawChannels(channels))
        } else {
            Ok(Self {
                channels,
                bit_depth,
            })
        }
    }

    pub fn parse(byte: u8) -> Result<Self, Error> {
        let channels = (byte & 0b0011_1111) + 1;
        let bit_depth = RawBitDepth::from_header_bits(byte >> 6)?;
        Self::new(channels, bit_depth)
    }

    pub const fn encode(self) -> u8 {
        ((self.bit_depth as u8) << 6) | (self.channels - 1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec2Mode {
    Mode700C = 0x00,
    Mode1200 = 0x01,
    Mode1300 = 0x02,
    Mode1400 = 0x03,
    Mode1600 = 0x04,
    Mode2400 = 0x05,
    Mode3200 = 0x06,
}

impl Codec2Mode {
    pub const fn from_header(byte: u8) -> Result<Self, Error> {
        match byte {
            0x00 => Ok(Self::Mode700C),
            0x01 => Ok(Self::Mode1200),
            0x02 => Ok(Self::Mode1300),
            0x03 => Ok(Self::Mode1400),
            0x04 => Ok(Self::Mode1600),
            0x05 => Ok(Self::Mode2400),
            0x06 => Ok(Self::Mode3200),
            other => Err(Error::UnknownCodec2Mode(other)),
        }
    }

    pub const fn header(self) -> u8 {
        self as u8
    }

    pub const fn bitrate(self) -> u16 {
        match self {
            Self::Mode700C => 700,
            Self::Mode1200 => 1200,
            Self::Mode1300 => 1300,
            Self::Mode1400 => 1400,
            Self::Mode1600 => 1600,
            Self::Mode2400 => 2400,
            Self::Mode3200 => 3200,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_header_mapping_matches_python() {
        assert_eq!(CodecKind::Raw.wire_id(), 0x00);
        assert_eq!(CodecKind::Opus.wire_id(), 0x01);
        assert_eq!(CodecKind::Codec2.wire_id(), 0x02);
        assert_eq!(CodecKind::Null.wire_id(), 0xFF);
        assert_eq!(CodecKind::from_wire(0x02), Ok(CodecKind::Codec2));
        assert_eq!(CodecKind::from_wire(0x03), Err(Error::UnknownCodec(0x03)));
    }

    #[test]
    fn raw_header_roundtrip() {
        let header = RawFrameHeader::new(2, RawBitDepth::Float32).unwrap();
        assert_eq!(header.encode(), 0x41);
        assert_eq!(RawFrameHeader::parse(0x41), Ok(header));

        let max = RawFrameHeader::new(64, RawBitDepth::Float128).unwrap();
        assert_eq!(RawFrameHeader::parse(max.encode()), Ok(max));
        assert_eq!(
            RawFrameHeader::new(0, RawBitDepth::Float16),
            Err(Error::InvalidRawChannels(0))
        );
    }

    #[test]
    fn codec2_mode_headers_match_python() {
        assert_eq!(Codec2Mode::Mode700C.header(), 0x00);
        assert_eq!(Codec2Mode::Mode1600.header(), 0x04);
        assert_eq!(Codec2Mode::Mode3200.header(), 0x06);
        assert_eq!(Codec2Mode::Mode700C.bitrate(), 700);
        assert_eq!(
            Codec2Mode::from_header(0x07),
            Err(Error::UnknownCodec2Mode(0x07))
        );
    }

    #[test]
    fn frame_roundtrip() {
        let frame = Frame::new(CodecKind::Raw, [0x41, 0xAA, 0xBB]);
        let wire = frame.to_wire_bytes().unwrap();
        assert_eq!(wire, vec![0x00, 0x41, 0xAA, 0xBB]);
        assert_eq!(Frame::from_wire_bytes(&wire), Ok(frame));
    }

    #[test]
    fn null_codec_is_known_but_not_transmittable() {
        assert_eq!(CodecKind::from_wire(0xFF), Ok(CodecKind::Null));
        assert_eq!(
            Frame::from_wire_bytes(&[0xFF]),
            Err(Error::NonTransmittableCodec(CodecKind::Null))
        );
        assert_eq!(
            LxstPacket::frame(Frame::new(CodecKind::Null, [])).encode(),
            Err(Error::NonTransmittableCodec(CodecKind::Null))
        );
    }

    #[test]
    fn signal_profile_base_matches_python() {
        let signal = Signal::from(Profile::QualityMedium);
        assert_eq!(signal.wire_value(), 0xFF + 0x40);
        assert_eq!(
            Signal::from_wire(0xFF + 0x70),
            Signal::PreferredProfile(Profile::LatencyUltraLow)
        );
    }

    #[test]
    fn packet_encodes_single_frame_as_binary_field() {
        let packet = LxstPacket::frame(Frame::new(CodecKind::Raw, [0x00]));
        let encoded = packet.encode().unwrap();
        assert_eq!(encoded, vec![0x81, 0x01, 0xC4, 0x02, 0x00, 0x00]);
        assert_eq!(LxstPacket::decode(&encoded), Ok(packet));
    }
}
