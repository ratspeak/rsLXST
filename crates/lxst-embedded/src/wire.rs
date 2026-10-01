//! Borrowed, bounded MessagePack adaptation of lxst-core::wire.
include!("../../lxst-core/src/shared/wire_types.rs");

pub const MAX_PACKET_BYTES: usize = 512;
pub const MAX_FRAME_BYTES: usize = 256;
pub const MAX_SIGNALS: usize = 8;
pub const MAX_FRAMES: usize = 4;
const MAX_FIELDS: usize = 16;
const MAX_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Shape,
    Limit,
    DuplicateField,
    TrailingData,
    OutputCapacity,
    UnknownCodec(u8),
    UnknownCodec2Mode(u8),
    EmptyFrame,
    NonTransmittableCodec(CodecKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub codec: CodecKind,
    pub payload: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
pub struct Packet<'a> {
    signals: [Signal; MAX_SIGNALS],
    signal_count: usize,
    frames: [Frame<'a>; MAX_FRAMES],
    frame_count: usize,
}
impl<'a> Packet<'a> {
    pub fn signals(&self) -> &[Signal] {
        &self.signals[..self.signal_count]
    }
    pub fn frames(&self) -> &[Frame<'a>] {
        &self.frames[..self.frame_count]
    }

    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(Error::Limit);
        }
        let mut reader = Reader { bytes, offset: 0 };
        let fields = reader.map_len()?;
        if fields > MAX_FIELDS {
            return Err(Error::Limit);
        }
        let mut packet = Self {
            signals: [Signal::Raw(0); MAX_SIGNALS],
            signal_count: 0,
            frames: [Frame {
                codec: CodecKind::Null,
                payload: &[],
            }; MAX_FRAMES],
            frame_count: 0,
        };
        let mut seen = 0;
        for _ in 0..fields {
            let key = reader.uint()?;
            if key > u8::MAX as u64 {
                return Err(Error::Shape);
            }
            match key {
                0 => {
                    if seen & 1 != 0 {
                        return Err(Error::DuplicateField);
                    }
                    seen |= 1;
                    let count = reader.array_len()?.unwrap_or(1);
                    if count > MAX_SIGNALS {
                        return Err(Error::Limit);
                    }
                    for signal in &mut packet.signals[..count] {
                        let raw = u32::try_from(reader.uint()?).map_err(|_| Error::Shape)?;
                        *signal = Signal::from_wire(raw);
                    }
                    packet.signal_count = count;
                }
                1 => {
                    if seen & 2 != 0 {
                        return Err(Error::DuplicateField);
                    }
                    seen |= 2;
                    let count = reader.array_len()?.unwrap_or(1);
                    if count > MAX_FRAMES {
                        return Err(Error::Limit);
                    }
                    for frame in &mut packet.frames[..count] {
                        let bytes = reader.binary()?;
                        let (&codec, payload) = bytes.split_first().ok_or(Error::EmptyFrame)?;
                        if bytes.len() > MAX_FRAME_BYTES {
                            return Err(Error::Limit);
                        }
                        let codec = CodecKind::from_wire(codec)?;
                        if !codec.is_transmittable() {
                            return Err(Error::NonTransmittableCodec(codec));
                        }
                        *frame = Frame { codec, payload };
                    }
                    packet.frame_count = count;
                }
                _ => reader.skip(0)?,
            }
        }
        if reader.offset != bytes.len() {
            return Err(Error::TrailingData);
        }
        Ok(packet)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        let end = self.offset.checked_add(count).ok_or(Error::Limit)?;
        let value = self.bytes.get(self.offset..end).ok_or(Error::Truncated)?;
        self.offset = end;
        Ok(value)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn number(&mut self, size: usize) -> Result<u64, Error> {
        Ok(self
            .take(size)?
            .iter()
            .fold(0, |value, byte| value << 8 | u64::from(*byte)))
    }
    fn uint(&mut self) -> Result<u64, Error> {
        match self.byte()? {
            value @ 0..=127 => Ok(u64::from(value)),
            0xcc => self.number(1),
            0xcd => self.number(2),
            0xce => self.number(4),
            0xcf => self.number(8),
            tag @ 0xd0..=0xd3 => {
                let size = 1 << (tag - 0xd0);
                let value = self.number(size)?;
                if value >> (size * 8 - 1) != 0 {
                    Err(Error::Shape)
                } else {
                    Ok(value)
                }
            }
            _ => Err(Error::Shape),
        }
    }
    fn map_len(&mut self) -> Result<usize, Error> {
        match self.byte()? {
            tag @ 0x80..=0x8f => Ok(usize::from(tag & 15)),
            0xde => Ok(self.number(2)? as usize),
            0xdf => usize::try_from(self.number(4)?).map_err(|_| Error::Limit),
            _ => Err(Error::Shape),
        }
    }
    fn array_len(&mut self) -> Result<Option<usize>, Error> {
        let tag = *self.bytes.get(self.offset).ok_or(Error::Truncated)?;
        let count = match tag {
            0x90..=0x9f => {
                self.offset += 1;
                usize::from(tag & 15)
            }
            0xdc => {
                self.offset += 1;
                self.number(2)? as usize
            }
            0xdd => {
                self.offset += 1;
                usize::try_from(self.number(4)?).map_err(|_| Error::Limit)?
            }
            _ => return Ok(None),
        };
        Ok(Some(count))
    }
    fn binary(&mut self) -> Result<&'a [u8], Error> {
        let count = match self.byte()? {
            0xc4 => self.number(1)?,
            0xc5 => self.number(2)?,
            0xc6 => self.number(4)?,
            _ => return Err(Error::Shape),
        };
        self.take(usize::try_from(count).map_err(|_| Error::Limit)?)
    }
    fn skip(&mut self, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Limit);
        }
        let tag = self.byte()?;
        let (bytes, children) = match tag {
            0x00..=0x7f | 0xe0..=0xff | 0xc0 | 0xc2 | 0xc3 => (0, 0),
            0x80..=0x8f => (0, usize::from(tag & 15) * 2),
            0x90..=0x9f => (0, usize::from(tag & 15)),
            0xa0..=0xbf => (usize::from(tag & 31), 0),
            0xcc | 0xd0 => (1, 0),
            0xcd | 0xd1 => (2, 0),
            0xce | 0xd2 | 0xca => (4, 0),
            0xcf | 0xd3 | 0xcb => (8, 0),
            0xc4 | 0xd9 => (self.number(1)? as usize, 0),
            0xc5 | 0xda => (self.number(2)? as usize, 0),
            0xc6 | 0xdb => (
                usize::try_from(self.number(4)?).map_err(|_| Error::Limit)?,
                0,
            ),
            0xdc..=0xdf => {
                let size = if tag & 1 == 0 { 2 } else { 4 };
                let count = usize::try_from(self.number(size)?).map_err(|_| Error::Limit)?;
                let children = count
                    .checked_mul(if tag >= 0xde { 2 } else { 1 })
                    .ok_or(Error::Limit)?;
                (0, children)
            }
            0xd4..=0xd8 => (1 + (1 << (tag - 0xd4)), 0),
            0xc7..=0xc9 => {
                let count =
                    usize::try_from(self.number(1 << (tag - 0xc7))?).map_err(|_| Error::Limit)?;
                (count.checked_add(1).ok_or(Error::Limit)?, 0)
            }
            _ => return Err(Error::Shape),
        };
        if children > MAX_PACKET_BYTES {
            return Err(Error::Limit);
        }
        self.take(bytes)?;
        for _ in 0..children {
            self.skip(depth + 1)?;
        }
        Ok(())
    }
}

fn uint_bytes(value: u32) -> usize {
    match value {
        0..=127 => 1,
        128..=255 => 2,
        256..=65535 => 3,
        _ => 5,
    }
}

/// Encode the same canonical map/array/binary forms as the full Rust codec.
/// Validation precedes all writes; malformed shape/capacity leaves output intact.
pub fn encode(signals: &[Signal], frames: &[Frame<'_>], output: &mut [u8]) -> Result<usize, Error> {
    if signals.len() > MAX_SIGNALS || frames.len() > MAX_FRAMES {
        return Err(Error::Limit);
    }
    let mut size = 1;
    if !signals.is_empty() {
        size += 2 + signals
            .iter()
            .map(|v| uint_bytes(v.wire_value()))
            .sum::<usize>();
    }
    if !frames.is_empty() {
        size += 1 + usize::from(frames.len() > 1);
        for frame in frames {
            if !frame.codec.is_transmittable() {
                return Err(Error::NonTransmittableCodec(frame.codec));
            }
            if frame.payload.len() >= MAX_FRAME_BYTES {
                return Err(Error::Limit);
            }
            let length = frame.payload.len() + 1;
            size += length + if length <= 255 { 2 } else { 3 };
        }
    }
    if size > MAX_PACKET_BYTES {
        return Err(Error::Limit);
    }
    if output.len() < size {
        return Err(Error::OutputCapacity);
    }
    let mut writer = Writer { output, offset: 0 };
    writer.byte(0x80 | (u8::from(!signals.is_empty()) + u8::from(!frames.is_empty())));
    if !signals.is_empty() {
        writer.byte(0);
        writer.byte(0x90 | signals.len() as u8);
        for signal in signals {
            writer.uint(signal.wire_value());
        }
    }
    if !frames.is_empty() {
        writer.byte(1);
        if frames.len() > 1 {
            writer.byte(0x90 | frames.len() as u8);
        }
        for frame in frames {
            let length = frame.payload.len() + 1;
            if length <= 255 {
                writer.byte(0xc4);
                writer.byte(length as u8);
            } else {
                writer.byte(0xc5);
                writer.bytes(&(length as u16).to_be_bytes());
            }
            writer.byte(frame.codec.wire_id());
            writer.bytes(frame.payload);
        }
    }
    debug_assert_eq!(writer.offset, size);
    Ok(size)
}
struct Writer<'a> {
    output: &'a mut [u8],
    offset: usize,
}
impl Writer<'_> {
    fn byte(&mut self, value: u8) {
        self.output[self.offset] = value;
        self.offset += 1;
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.output[self.offset..self.offset + bytes.len()].copy_from_slice(bytes);
        self.offset += bytes.len();
    }
    fn uint(&mut self, value: u32) {
        match value {
            0..=127 => self.byte(value as u8),
            128..=255 => {
                self.byte(0xcc);
                self.byte(value as u8);
            }
            256..=65535 => {
                self.byte(0xcd);
                self.bytes(&(value as u16).to_be_bytes());
            }
            _ => {
                self.byte(0xce);
                self.bytes(&value.to_be_bytes());
            }
        }
    }
}
