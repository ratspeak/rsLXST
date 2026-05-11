use std::collections::VecDeque;

use thiserror::Error;

use crate::{CodecKind, Frame, LxstPacket};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StreamError {
    #[error("frame packetizer batch size must be greater than zero")]
    InvalidBatchSize,
    #[error("jitter buffer capacity must be greater than zero")]
    InvalidJitterCapacity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FramePacketizer {
    frames_per_packet: usize,
}

impl FramePacketizer {
    pub const PYTHON_COMPATIBLE: Self = Self {
        frames_per_packet: 1,
    };

    pub fn new(frames_per_packet: usize) -> Result<Self, StreamError> {
        if frames_per_packet == 0 {
            Err(StreamError::InvalidBatchSize)
        } else {
            Ok(Self { frames_per_packet })
        }
    }

    pub const fn frames_per_packet(self) -> usize {
        self.frames_per_packet
    }

    pub fn packetize_one(self, frame: Frame) -> LxstPacket {
        LxstPacket::frame(frame)
    }

    pub fn packetize(self, frames: impl IntoIterator<Item = Frame>) -> Vec<LxstPacket> {
        let mut packets = Vec::new();
        let mut batch = Vec::with_capacity(self.frames_per_packet);

        for frame in frames {
            batch.push(frame);
            if batch.len() == self.frames_per_packet {
                packets.push(packet_from_batch(&mut batch));
            }
        }

        if !batch.is_empty() {
            packets.push(packet_from_batch(&mut batch));
        }

        packets
    }
}

fn packet_from_batch(batch: &mut Vec<Frame>) -> LxstPacket {
    if batch.len() == 1 {
        LxstPacket::frame(batch.pop().expect("batch has one frame"))
    } else {
        LxstPacket {
            signals: Vec::new(),
            frames: std::mem::take(batch),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameStreamEvent {
    CodecChanged {
        from: Option<CodecKind>,
        to: CodecKind,
    },
    Frame(Frame),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameStreamState {
    current_codec: Option<CodecKind>,
}

impl FrameStreamState {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn current_codec(&self) -> Option<CodecKind> {
        self.current_codec
    }

    pub fn accept_packet(&mut self, packet: LxstPacket) -> Vec<FrameStreamEvent> {
        let mut events = Vec::new();

        for frame in packet.frames {
            if self.current_codec != Some(frame.codec) {
                let from = self.current_codec;
                self.current_codec = Some(frame.codec);
                events.push(FrameStreamEvent::CodecChanged {
                    from,
                    to: frame.codec,
                });
            }
            events.push(FrameStreamEvent::Frame(frame));
        }

        events
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropPolicy {
    DropNewest,
    DropOldest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct JitterStats {
    pub pushed: u64,
    pub popped: u64,
    pub dropped_oldest: u64,
    pub dropped_newest: u64,
    pub underruns: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JitterPush<T> {
    Accepted,
    DroppedIncoming(T),
    DroppedOldest(T),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitterBuffer<T> {
    capacity: usize,
    drop_policy: DropPolicy,
    queue: VecDeque<T>,
    stats: JitterStats,
}

impl<T> JitterBuffer<T> {
    pub fn new(capacity: usize, drop_policy: DropPolicy) -> Result<Self, StreamError> {
        if capacity == 0 {
            Err(StreamError::InvalidJitterCapacity)
        } else {
            Ok(Self {
                capacity,
                drop_policy,
                queue: VecDeque::with_capacity(capacity),
                stats: JitterStats::default(),
            })
        }
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub const fn drop_policy(&self) -> DropPolicy {
        self.drop_policy
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub const fn stats(&self) -> JitterStats {
        self.stats
    }

    pub fn push(&mut self, item: T) -> JitterPush<T> {
        self.stats.pushed += 1;
        if self.queue.len() < self.capacity {
            self.queue.push_back(item);
            return JitterPush::Accepted;
        }

        match self.drop_policy {
            DropPolicy::DropNewest => {
                self.stats.dropped_newest += 1;
                JitterPush::DroppedIncoming(item)
            }
            DropPolicy::DropOldest => {
                self.stats.dropped_oldest += 1;
                let dropped = self
                    .queue
                    .pop_front()
                    .expect("full jitter buffer has oldest item");
                self.queue.push_back(item);
                JitterPush::DroppedOldest(dropped)
            }
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        let item = self.queue.pop_front();
        if item.is_some() {
            self.stats.popped += 1;
        } else {
            self.stats.underruns += 1;
        }
        item
    }

    pub fn clear(&mut self) {
        self.queue.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_frame(byte: u8) -> Frame {
        Frame::new(CodecKind::Raw, [0x00, byte])
    }

    #[test]
    fn python_compatible_packetizer_sends_one_frame_per_packet() {
        let frames = vec![raw_frame(1), raw_frame(2)];
        let packets = FramePacketizer::PYTHON_COMPATIBLE.packetize(frames.clone());

        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].frames, vec![frames[0].clone()]);
        assert_eq!(packets[1].frames, vec![frames[1].clone()]);
    }

    #[test]
    fn batched_packetizer_groups_frames_without_reordering() {
        let frames = vec![raw_frame(1), raw_frame(2), raw_frame(3)];
        let packetizer = FramePacketizer::new(2).unwrap();
        let packets = packetizer.packetize(frames.clone());

        assert_eq!(packetizer.frames_per_packet(), 2);
        assert_eq!(packets.len(), 2);
        assert_eq!(
            packets[0].frames,
            vec![frames[0].clone(), frames[1].clone()]
        );
        assert_eq!(packets[1].frames, vec![frames[2].clone()]);
    }

    #[test]
    fn frame_stream_state_emits_codec_changes_before_frames() {
        let mut state = FrameStreamState::new();
        let packet = LxstPacket {
            signals: Vec::new(),
            frames: vec![
                Frame::new(CodecKind::Raw, [0x00]),
                Frame::new(CodecKind::Raw, [0x01]),
                Frame::new(CodecKind::Opus, [0xF8]),
            ],
        };

        let events = state.accept_packet(packet);
        assert_eq!(
            events,
            vec![
                FrameStreamEvent::CodecChanged {
                    from: None,
                    to: CodecKind::Raw,
                },
                FrameStreamEvent::Frame(Frame::new(CodecKind::Raw, [0x00])),
                FrameStreamEvent::Frame(Frame::new(CodecKind::Raw, [0x01])),
                FrameStreamEvent::CodecChanged {
                    from: Some(CodecKind::Raw),
                    to: CodecKind::Opus,
                },
                FrameStreamEvent::Frame(Frame::new(CodecKind::Opus, [0xF8])),
            ]
        );
        assert_eq!(state.current_codec(), Some(CodecKind::Opus));
    }

    #[test]
    fn jitter_buffer_drop_newest_keeps_existing_latency_window() {
        let mut buffer = JitterBuffer::new(2, DropPolicy::DropNewest).unwrap();
        assert_eq!(buffer.push(1), JitterPush::Accepted);
        assert_eq!(buffer.push(2), JitterPush::Accepted);
        assert_eq!(buffer.push(3), JitterPush::DroppedIncoming(3));
        assert_eq!(buffer.pop(), Some(1));
        assert_eq!(buffer.pop(), Some(2));
        assert_eq!(
            buffer.stats(),
            JitterStats {
                pushed: 3,
                popped: 2,
                dropped_oldest: 0,
                dropped_newest: 1,
                underruns: 0,
            }
        );
    }

    #[test]
    fn jitter_buffer_drop_oldest_preserves_latest_audio() {
        let mut buffer = JitterBuffer::new(2, DropPolicy::DropOldest).unwrap();
        assert_eq!(buffer.push(1), JitterPush::Accepted);
        assert_eq!(buffer.push(2), JitterPush::Accepted);
        assert_eq!(buffer.push(3), JitterPush::DroppedOldest(1));
        assert_eq!(buffer.pop(), Some(2));
        assert_eq!(buffer.pop(), Some(3));
        assert_eq!(
            buffer.stats(),
            JitterStats {
                pushed: 3,
                popped: 2,
                dropped_oldest: 1,
                dropped_newest: 0,
                underruns: 0,
            }
        );
    }

    #[test]
    fn jitter_buffer_tracks_playback_underruns() {
        let mut buffer = JitterBuffer::new(2, DropPolicy::DropOldest).unwrap();
        assert_eq!(buffer.pop(), None);
        assert_eq!(buffer.push(1), JitterPush::Accepted);
        assert_eq!(buffer.pop(), Some(1));
        assert_eq!(buffer.pop(), None);
        assert_eq!(
            buffer.stats(),
            JitterStats {
                pushed: 1,
                popped: 1,
                dropped_oldest: 0,
                dropped_newest: 0,
                underruns: 2,
            }
        );
    }
}
