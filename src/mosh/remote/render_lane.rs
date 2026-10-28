//! The keeper's render-lane queue handle.
//!
//! Owns: the bounded, clearable queue of render frames between the keeper's `server_reader`
//! thread (producer) and a leg's lane task on the runtime (consumer, mosh-19g.2.4), and the
//! offer semantics that implement supersede.
//! Hides: the lock and the wake-up of the waiting consumer.
//! Must not: classify frames (the caller passes the `FrameClass`), touch a stream, or know
//! about sessions or legs.
//! Lifecycle: created with a QUIC leg, shared by `KeeperLeg` and its `LegSlot`, closed when
//! the leg is dropped; the consumer drains what is queued and then sees `None`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::remote::attach::mosh::link_frames::FrameClass;

/// Frames a lane holds before chained frames are turned away.
pub(super) const LANE_CAPACITY: usize = 64;

/// What `offer` did with a frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Offer {
    Accepted,
    Rejected,
}

struct State {
    frames: VecDeque<(FrameClass, Vec<u8>)>,
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
}

/// Producer side: the keeper's `server_reader` thread. Consumer side: the lane's runtime task.
// The lane task (mosh-19g.2.4) is the first non-test user of everything but `close`.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone)]
pub(super) struct LaneQueue {
    inner: Arc<Inner>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl LaneQueue {
    pub(super) fn new() -> LaneQueue {
        LaneQueue {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    frames: VecDeque::new(),
                    closed: false,
                }),
                notify: Notify::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `SelfContained` clears the queue (everything queued is superseded), pushes and is
    /// accepted. `Chained` is rejected when `LANE_CAPACITY` frames are queued, else pushed.
    /// Either is rejected once the lane is closed. `Reliable` frames never travel on a lane.
    pub(super) fn offer(&self, class: FrameClass, bytes: Vec<u8>) -> Offer {
        debug_assert_ne!(class, FrameClass::Reliable, "reliable frames are not lane frames");
        {
            let mut state = self.lock();
            if state.closed {
                return Offer::Rejected;
            }
            match class {
                FrameClass::SelfContained => state.frames.clear(),
                FrameClass::Chained | FrameClass::Reliable => {
                    if state.frames.len() >= LANE_CAPACITY {
                        return Offer::Rejected;
                    }
                }
            }
            state.frames.push_back((class, bytes));
        }
        self.inner.notify.notify_one();
        Offer::Accepted
    }

    /// Waits for the next frame; `None` once the lane is closed and drained. One consumer.
    pub(super) async fn pop(&self) -> Option<(FrameClass, Vec<u8>)> {
        loop {
            {
                let mut state = self.lock();
                if let Some(frame) = state.frames.pop_front() {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            // `notify_one` stores a permit when nobody waits, so an offer between the unlock
            // above and this await still wakes it.
            self.inner.notify.notified().await;
        }
    }

    /// The leg ended: wake the consumer and end it once the queue drains.
    pub(super) fn close(&self) {
        self.lock().closed = true;
        self.inner.notify.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn fill_chained(queue: &LaneQueue) {
        for n in 0..LANE_CAPACITY {
            assert_eq!(queue.offer(FrameClass::Chained, vec![n as u8]), Offer::Accepted);
        }
    }

    #[test]
    fn capacity_is_64() {
        assert_eq!(LANE_CAPACITY, 64);
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.lock().frames.len(), 64);
    }

    #[test]
    fn chained_rejected_when_64_queued() {
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.offer(FrameClass::Chained, vec![0xff]), Offer::Rejected);
        assert_eq!(queue.lock().frames.len(), 64);
        // Rejected means not queued: the 64th frame is still the last one.
        assert_eq!(queue.lock().frames.back().unwrap().1, vec![63]);
    }

    #[test]
    fn self_contained_clears_and_is_accepted_when_full() {
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.offer(FrameClass::SelfContained, vec![0xaa]), Offer::Accepted);
        let state = queue.lock();
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0], (FrameClass::SelfContained, vec![0xaa]));
        drop(state);
        // Room again for chained frames.
        assert_eq!(queue.offer(FrameClass::Chained, vec![1]), Offer::Accepted);
    }

    #[test]
    fn pop_returns_frames_in_order() {
        let queue = LaneQueue::new();
        queue.offer(FrameClass::SelfContained, vec![1]);
        queue.offer(FrameClass::Chained, vec![2]);
        let rt = runtime();
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::SelfContained, vec![1])));
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::Chained, vec![2])));
    }

    #[test]
    fn pop_wakes_on_offer() {
        let queue = LaneQueue::new();
        let producer = queue.clone();
        let start = Instant::now();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            producer.offer(FrameClass::Chained, vec![7]);
        });
        let frame = runtime().block_on(queue.pop());
        assert_eq!(frame, Some((FrameClass::Chained, vec![7])));
        assert!(start.elapsed() >= Duration::from_millis(50), "pop returned before the offer");
        handle.join().unwrap();
    }

    #[test]
    fn close_ends_pop() {
        let queue = LaneQueue::new();
        let closer = queue.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            closer.close();
        });
        assert_eq!(runtime().block_on(queue.pop()), None);
        handle.join().unwrap();
    }

    #[test]
    fn close_drains_queued_frames_then_ends() {
        let queue = LaneQueue::new();
        queue.offer(FrameClass::Chained, vec![1]);
        queue.close();
        let rt = runtime();
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::Chained, vec![1])));
        assert_eq!(rt.block_on(queue.pop()), None);
    }

    #[test]
    fn closed_lane_rejects_offers() {
        let queue = LaneQueue::new();
        queue.close();
        assert_eq!(queue.offer(FrameClass::SelfContained, vec![1]), Offer::Rejected);
        assert_eq!(queue.offer(FrameClass::Chained, vec![1]), Offer::Rejected);
    }
}
