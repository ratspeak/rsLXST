//! Local arrival/capture deadlines, not a claim about end-to-end packet age.
//! LXST has no sequence/timestamp in its baseline wire contract.
use crate::wire::MAX_FRAME_BYTES;
const CAPACITY: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    EmptyPayload,
    PayloadTooLarge,
    Expired,
    OutputCapacity,
}
#[derive(Clone, Copy)]
struct Slot {
    bytes: [u8; MAX_FRAME_BYTES],
    length: usize,
    expires: u64,
    generation: u32,
}
impl Slot {
    const EMPTY: Self = Self {
        bytes: [0; MAX_FRAME_BYTES],
        length: 0,
        expires: 0,
        generation: 0,
    };
}
/// Three arrival-ordered encoded frames. Pressure drops the oldest frame;
/// draining discards expired and stale-generation frames before returning data.
pub struct MediaQueue {
    slots: [Slot; CAPACITY],
    head: usize,
    length: usize,
    dropped: u64,
}
impl Default for MediaQueue {
    fn default() -> Self {
        Self::new()
    }
}
impl MediaQueue {
    pub const fn new() -> Self {
        Self {
            slots: [Slot::EMPTY; CAPACITY],
            head: 0,
            length: 0,
            dropped: 0,
        }
    }
    pub const fn len(&self) -> usize {
        self.length
    }
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
    pub fn clear(&mut self) {
        self.slots.fill(Slot::EMPTY);
        self.head = 0;
        self.length = 0;
    }
    fn retire(&mut self, dropped: bool) {
        self.slots[self.head] = Slot::EMPTY;
        self.head = (self.head + 1) % CAPACITY;
        self.length -= 1;
        if dropped {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
    pub fn push(
        &mut self,
        bytes: &[u8],
        generation: u32,
        now_ms: u64,
        expires_ms: u64,
    ) -> Result<(), QueueError> {
        if bytes.is_empty() {
            return Err(QueueError::EmptyPayload);
        }
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(QueueError::PayloadTooLarge);
        }
        if expires_ms <= now_ms {
            return Err(QueueError::Expired);
        }
        if self.length == CAPACITY {
            self.retire(true);
        }
        let slot = &mut self.slots[(self.head + self.length) % CAPACITY];
        slot.bytes[..bytes.len()].copy_from_slice(bytes);
        slot.length = bytes.len();
        slot.expires = expires_ms;
        slot.generation = generation;
        self.length += 1;
        Ok(())
    }
    pub fn pop(
        &mut self,
        now_ms: u64,
        generation: u32,
        output: &mut [u8],
    ) -> Result<Option<usize>, QueueError> {
        while self.length != 0 {
            let slot = &self.slots[self.head];
            if slot.expires <= now_ms || slot.generation != generation {
                self.retire(true);
                continue;
            }
            if output.len() < slot.length {
                return Err(QueueError::OutputCapacity);
            }
            let length = slot.length;
            output[..length].copy_from_slice(&slot.bytes[..length]);
            self.retire(false);
            return Ok(Some(length));
        }
        Ok(None)
    }
}
