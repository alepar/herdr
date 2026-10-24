//! A bounded replay ring keyed by absolute byte offset: `push`, `ack(offset)`,
//! `read_at(offset, max)`. Overflow is reported, never silently dropped. Does not decide
//! what to do on overflow (the caller does).

use std::collections::VecDeque;

/// A `push` that would exceed the ring capacity (the ring is left unchanged).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RingOverflow {
    pub capacity: usize,
    pub attempted: usize,
}

/// Bounded byte ring keyed by absolute offset. Owner supplies the capacity; it reports overflow
/// and never decides what overflow means.
pub(super) struct ReplayRing {
    bytes: VecDeque<u8>,
    start: u64,
    capacity: usize,
}

impl ReplayRing {
    pub fn new(capacity: usize) -> Self {
        ReplayRing {
            bytes: VecDeque::new(),
            start: 0,
            capacity,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Lowest unacked offset.
    pub fn start_offset(&self) -> u64 {
        self.start
    }

    /// Next offset to be pushed (`start + occupancy`).
    pub fn end_offset(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    pub fn occupancy(&self) -> usize {
        self.bytes.len()
    }

    /// All-or-nothing: on overflow the ring is unchanged and the error says so.
    pub fn push(&mut self, data: &[u8]) -> Result<(), RingOverflow> {
        let attempted = self.bytes.len().saturating_add(data.len());
        if attempted > self.capacity {
            return Err(RingOverflow {
                capacity: self.capacity,
                attempted,
            });
        }
        self.bytes.extend(data.iter().copied());
        Ok(())
    }

    /// Drop bytes below `offset`. Offsets <= start are a no-op. Offsets > end are rejected,
    /// because a peer acked bytes it was never sent.
    pub fn ack(&mut self, offset: u64) -> Result<(), ()> {
        if offset > self.end_offset() {
            return Err(());
        }
        if offset > self.start {
            let drop = (offset - self.start) as usize;
            self.bytes.drain(..drop);
            self.start = offset;
        }
        Ok(())
    }

    /// Up to `max` bytes starting at `offset`; `None` if `offset < start` (trimmed) or `> end`.
    pub fn read_at(&self, offset: u64, max: usize) -> Option<Vec<u8>> {
        if offset < self.start || offset > self.end_offset() {
            return None;
        }
        let from = (offset - self.start) as usize;
        let to = from + max.min(self.bytes.len() - from);
        Some(self.bytes.range(from..to).copied().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_is_reported_and_leaves_ring_unchanged() {
        let mut ring = ReplayRing::new(8);
        ring.push(b"abcdefgh").unwrap();
        assert_eq!(ring.occupancy(), 8);
        let err = ring.push(b"i").unwrap_err();
        assert_eq!(
            err,
            RingOverflow {
                capacity: 8,
                attempted: 9
            }
        );
        assert_eq!(ring.occupancy(), 8);
        assert_eq!(ring.end_offset(), 8);
        assert_eq!(ring.read_at(0, 100).unwrap(), b"abcdefgh");
    }

    #[test]
    fn wraparound_keeps_absolute_offsets() {
        let mut ring = ReplayRing::new(8);
        let mut stream = Vec::new();
        for i in 0..100u32 {
            let chunk: Vec<u8> = (0..5).map(|j| (i * 5 + j) as u8).collect();
            ring.push(&chunk).unwrap();
            stream.extend_from_slice(&chunk);
            let start = ring.start_offset();
            assert_eq!(
                ring.read_at(start, 100).unwrap(),
                stream[start as usize..],
                "round {i}"
            );
            ring.ack(start + 5).unwrap();
        }
        assert_eq!(ring.end_offset(), 500);
        assert_eq!(ring.start_offset(), 500);
        assert_eq!(ring.occupancy(), 0);
    }

    #[test]
    fn ack_trims_and_rejects_beyond_end() {
        let mut ring = ReplayRing::new(16);
        ring.push(b"0123456789").unwrap();
        ring.ack(4).unwrap();
        assert_eq!((ring.start_offset(), ring.occupancy()), (4, 6));
        ring.ack(2).unwrap();
        assert_eq!((ring.start_offset(), ring.occupancy()), (4, 6));
        assert_eq!(ring.ack(11), Err(()));
        assert_eq!((ring.start_offset(), ring.occupancy()), (4, 6));
        ring.ack(10).unwrap();
        assert_eq!((ring.start_offset(), ring.occupancy()), (10, 0));
    }

    #[test]
    fn read_at_boundaries() {
        let mut ring = ReplayRing::new(16);
        ring.push(b"0123456789").unwrap();
        ring.ack(3).unwrap();
        assert_eq!(ring.read_at(3, 100).unwrap(), b"3456789");
        assert_eq!(ring.read_at(10, 5).unwrap(), b"");
        assert_eq!(ring.read_at(2, 5), None);
        assert_eq!(ring.read_at(11, 5), None);
        assert_eq!(ring.read_at(4, 3).unwrap(), b"456");
        assert_eq!(ring.read_at(4, 0).unwrap(), b"");
    }

    #[test]
    fn push_after_ack_reuses_space() {
        let mut ring = ReplayRing::new(4);
        ring.push(b"abcd").unwrap();
        assert!(ring.push(b"e").is_err());
        ring.ack(2).unwrap();
        ring.push(b"ef").unwrap();
        assert_eq!(ring.read_at(2, 10).unwrap(), b"cdef");
    }
}
