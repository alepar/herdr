//! Bridge-side ordering of render-lane frames: in-order delivery, gap detection (reset or
//! overdue), chain dropping and the once-per-gap resync request.
//! Sans-IO: time and SRTT are inputs, the caller performs every delivery and request.
//! Owns: `RenderReorder`, `RenderArrival`. Does not own: lane reading or resync encoding.

use super::link_frames::FrameClass;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// SRTT assumed while the process-local read is unknown.
pub(super) const UNKNOWN_SRTT: Duration = Duration::from_millis(250);
/// Floor of the overdue threshold.
pub(super) const MIN_OVERDUE: Duration = Duration::from_millis(50);

/// What the bridge's lane reader saw for one render stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RenderArrival {
    Header {
        seq: u64,
        class: FrameClass,
    },
    Complete {
        seq: u64,
        class: FrameClass,
        bytes: Vec<u8>,
    },
    Reset {
        seq: u64,
    },
}

/// Why a gap was declared (reported on the call that declared it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GapCause {
    Reset,
    Overdue,
    /// A reset of a seq that a known later self-contained frame already supersedes.
    Supersede,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ReorderOutput {
    /// Whole frames to hand on, in order.
    pub deliver: Vec<Vec<u8>>,
    /// The caller should inject one resync request.
    pub request_resync: bool,
    /// Set on the call that declared a gap.
    pub gap: Option<GapCause>,
}

#[derive(Debug)]
struct Held {
    class: FrameClass,
    bytes: Vec<u8>,
    at: Instant,
}

#[derive(Debug)]
pub(super) struct RenderReorder {
    /// Lowest seq neither delivered nor dropped.
    next: u64,
    held: BTreeMap<u64, Held>,
    /// Seqs whose self-contained header was seen and which are not yet delivered.
    sc_headers: BTreeSet<u64>,
    in_gap: bool,
    /// The current gap's resync is sent, or deliberately skipped.
    requested: bool,
}

fn threshold(srtt: Option<Duration>) -> Duration {
    srtt.unwrap_or(UNKNOWN_SRTT).max(MIN_OVERDUE)
}

impl RenderReorder {
    /// A new leg: gap state, seq 0 expected, request counted as sent (leg 0's first frame
    /// is a full surface; later legs get the bridge's resume resync).
    pub(super) fn new() -> RenderReorder {
        RenderReorder {
            next: 0,
            held: BTreeMap::new(),
            sc_headers: BTreeSet::new(),
            in_gap: true,
            requested: true,
        }
    }

    pub(super) fn on_arrival(
        &mut self,
        arrival: RenderArrival,
        now: Instant,
        srtt: Option<Duration>,
    ) -> ReorderOutput {
        let mut out = ReorderOutput::default();
        match arrival {
            RenderArrival::Header { seq, class } => {
                if seq >= self.next && class == FrameClass::SelfContained {
                    self.sc_headers.insert(seq);
                }
            }
            RenderArrival::Complete { seq, class, bytes } => {
                if seq >= self.next {
                    if class == FrameClass::SelfContained {
                        self.sc_headers.insert(seq);
                    }
                    if !(self.in_gap && class == FrameClass::Chained) {
                        self.held.entry(seq).or_insert(Held {
                            class,
                            bytes,
                            at: now,
                        });
                    }
                }
            }
            RenderArrival::Reset { seq } => {
                if seq >= self.next && !self.in_gap && !self.held.contains_key(&seq) {
                    let superseded = self.sc_headers.range(seq + 1..).next().is_some();
                    if superseded {
                        // The request is skipped on purpose: the SC frame is the resync.
                        self.requested = true;
                        self.declare_gap(GapCause::Supersede, &mut out);
                    } else {
                        self.declare_gap(GapCause::Reset, &mut out);
                    }
                }
            }
        }
        self.settle(now, srtt, &mut out);
        out
    }

    /// Overdue check; call on every tick.
    pub(super) fn poll(&mut self, now: Instant, srtt: Option<Duration>) -> ReorderOutput {
        let mut out = ReorderOutput::default();
        self.settle(now, srtt, &mut out);
        out
    }

    /// The instant at which the missing `next` becomes overdue, if a later completion is held.
    pub(super) fn next_deadline(&self, srtt: Option<Duration>) -> Option<Instant> {
        if self.in_gap {
            return None;
        }
        self.held
            .values()
            .map(|h| h.at)
            .min()
            .map(|at| at + threshold(srtt))
    }

    fn declare_gap(&mut self, cause: GapCause, out: &mut ReorderOutput) {
        self.in_gap = true;
        out.gap = Some(cause);
        self.held.retain(|_, h| h.class != FrameClass::Chained);
    }

    fn deliver(&mut self, seq: u64, out: &mut ReorderOutput) {
        if let Some(h) = self.held.remove(&seq) {
            out.deliver.push(h.bytes);
        }
        self.next = seq + 1;
        self.held = self.held.split_off(&self.next);
        self.sc_headers = self.sc_headers.split_off(&self.next);
    }

    fn settle(&mut self, now: Instant, srtt: Option<Duration>, out: &mut ReorderOutput) {
        loop {
            if self.in_gap {
                let sc = self
                    .held
                    .iter()
                    .find(|(_, h)| h.class == FrameClass::SelfContained)
                    .map(|(seq, _)| *seq);
                match sc {
                    Some(seq) => {
                        self.deliver(seq, out);
                        self.in_gap = false;
                        self.requested = false;
                    }
                    None => break,
                }
            } else if self.held.contains_key(&self.next) {
                self.deliver(self.next, out);
            } else if self
                .next_deadline(srtt)
                .is_some_and(|deadline| now >= deadline)
            {
                self.declare_gap(GapCause::Overdue, out);
            } else {
                break;
            }
        }
        // A gap closed within this call by an already-held frame needs no resync.
        if self.in_gap && !self.requested {
            self.requested = true;
            out.request_resync = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FrameClass::{Chained, SelfContained};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn complete(seq: u64, class: FrameClass) -> RenderArrival {
        RenderArrival::Complete {
            seq,
            class,
            bytes: vec![seq as u8],
        }
    }
    fn header(seq: u64, class: FrameClass) -> RenderArrival {
        RenderArrival::Header { seq, class }
    }
    /// A reorder that has left its initial gap by delivering SC frame 0.
    fn live(t0: Instant) -> RenderReorder {
        let mut r = RenderReorder::new();
        let out = r.on_arrival(complete(0, SelfContained), t0, None);
        assert_eq!(out.deliver, vec![vec![0]]);
        assert!(!out.request_resync);
        r
    }

    #[test]
    fn in_order_delivery() {
        let t0 = Instant::now();
        let mut r = live(t0);
        let o1 = r.on_arrival(complete(1, Chained), t0, None);
        let o2 = r.on_arrival(complete(2, SelfContained), t0, None);
        assert_eq!(o1.deliver, vec![vec![1]]);
        assert_eq!(o2.deliver, vec![vec![2]]);
        assert_eq!(o2.gap, None);
        assert_eq!(r.next_deadline(None), None);
    }

    #[test]
    fn out_of_order_waits_then_delivers() {
        let t0 = Instant::now();
        let mut r = live(t0);
        let o = r.on_arrival(complete(2, Chained), t0, None);
        assert!(o.deliver.is_empty());
        let o = r.on_arrival(complete(1, Chained), t0 + ms(10), None);
        assert_eq!(o.deliver, vec![vec![1], vec![2]]);
        assert_eq!(o.gap, None);
        assert!(!o.request_resync);
    }

    #[test]
    fn reset_drops_chained_until_self_contained_and_requests_once() {
        let t0 = Instant::now();
        let mut r = live(t0);
        let o = r.on_arrival(RenderArrival::Reset { seq: 1 }, t0, None);
        assert_eq!(o.gap, Some(GapCause::Reset));
        assert!(o.request_resync);
        let o = r.on_arrival(complete(2, Chained), t0, None);
        assert!(o.deliver.is_empty() && !o.request_resync);
        let o = r.on_arrival(RenderArrival::Reset { seq: 3 }, t0, None);
        assert!(o.deliver.is_empty() && !o.request_resync && o.gap.is_none());
        let o = r.on_arrival(complete(4, SelfContained), t0, None);
        assert_eq!(o.deliver, vec![vec![4]]);
        // The dropped chained frame 2 never comes back.
        let o = r.on_arrival(complete(5, Chained), t0, None);
        assert_eq!(o.deliver, vec![vec![5]]);
    }

    fn overdue_gap_at(srtt: Option<Duration>, threshold: Duration) {
        let t0 = Instant::now();
        let mut r = live(t0);
        let o = r.on_arrival(complete(2, Chained), t0, srtt);
        assert!(o.deliver.is_empty());
        assert_eq!(r.next_deadline(srtt), Some(t0 + threshold));
        let o = r.poll(t0 + threshold - ms(1), srtt);
        assert_eq!(o, ReorderOutput::default());
        let o = r.poll(t0 + threshold, srtt);
        assert_eq!(o.gap, Some(GapCause::Overdue));
        assert!(o.request_resync);
        assert!(o.deliver.is_empty(), "held chained frame is dropped");
        assert_eq!(r.next_deadline(srtt), None);
    }

    #[test]
    fn overdue_gap_uses_unknown_srtt_250ms() {
        overdue_gap_at(None, ms(250));
    }

    #[test]
    fn overdue_gap_respects_50ms_floor() {
        overdue_gap_at(Some(ms(20)), ms(50));
    }

    #[test]
    fn overdue_gap_uses_srtt() {
        overdue_gap_at(Some(ms(400)), ms(400));
    }

    #[test]
    fn self_contained_after_gap_supersedes_missing() {
        let t0 = Instant::now();
        let mut r = live(t0);
        r.on_arrival(RenderArrival::Reset { seq: 1 }, t0, None);
        // seq 2 is missing too; SC 3 releases at once and supersedes both.
        let o = r.on_arrival(complete(3, SelfContained), t0, None);
        assert_eq!(o.deliver, vec![vec![3]]);
        let o = r.on_arrival(complete(4, Chained), t0, None);
        assert_eq!(o.deliver, vec![vec![4]]);
        // Late arrivals of superseded seqs are stale.
        let o = r.on_arrival(complete(2, SelfContained), t0, None);
        assert!(o.deliver.is_empty());
    }

    #[test]
    fn self_contained_ahead_of_missing_waits_without_gap() {
        let t0 = Instant::now();
        let mut r = live(t0);
        let o = r.on_arrival(complete(2, SelfContained), t0, None);
        assert!(o.deliver.is_empty() && o.gap.is_none() && !o.request_resync);
        let o = r.poll(t0 + ms(249), None);
        assert_eq!(o, ReorderOutput::default());
        // Overdue: the held SC releases at once, and since it closes the gap in the same
        // call there is nothing left to resync.
        let o = r.poll(t0 + ms(250), None);
        assert_eq!(o.deliver, vec![vec![2]]);
        assert_eq!(o.gap, Some(GapCause::Overdue));
        assert!(!o.request_resync);
        let o = r.on_arrival(complete(3, Chained), t0 + ms(251), None);
        assert_eq!(o.deliver, vec![vec![3]]);
    }

    #[test]
    fn reset_with_known_later_self_contained_header_is_supersede_without_request() {
        let t0 = Instant::now();
        let mut r = live(t0);
        r.on_arrival(header(3, SelfContained), t0, None);
        let o = r.on_arrival(RenderArrival::Reset { seq: 1 }, t0, None);
        assert_eq!(o.gap, Some(GapCause::Supersede));
        assert!(!o.request_resync);
        // Chained frames are dropped while waiting for the SC body.
        let o = r.on_arrival(complete(2, Chained), t0, None);
        assert!(o.deliver.is_empty() && !o.request_resync);
        let o = r.on_arrival(complete(3, SelfContained), t0, None);
        assert_eq!(o.deliver, vec![vec![3]]);
        assert!(!o.request_resync);
    }

    #[test]
    fn reset_with_only_chained_headers_above_still_requests() {
        let t0 = Instant::now();
        let mut r = live(t0);
        r.on_arrival(header(3, Chained), t0, None);
        let o = r.on_arrival(RenderArrival::Reset { seq: 1 }, t0, None);
        assert_eq!(o.gap, Some(GapCause::Reset));
        assert!(o.request_resync);
    }

    #[test]
    fn once_per_gap_then_new_gap_requests_again() {
        let t0 = Instant::now();
        let mut r = live(t0);
        assert!(
            r.on_arrival(RenderArrival::Reset { seq: 1 }, t0, None)
                .request_resync
        );
        assert!(
            !r.poll(t0 + ms(1000), None).request_resync,
            "no repeat while the gap stays open"
        );
        r.on_arrival(complete(2, SelfContained), t0, None);
        let o = r.on_arrival(RenderArrival::Reset { seq: 3 }, t0, None);
        assert_eq!(o.gap, Some(GapCause::Reset));
        assert!(o.request_resync);
    }

    #[test]
    fn new_leg_starts_in_gap_without_request() {
        let t0 = Instant::now();
        let mut r = RenderReorder::new();
        let o = r.on_arrival(complete(0, Chained), t0, None);
        assert_eq!(o, ReorderOutput::default());
        let o = r.poll(t0 + ms(5000), None);
        assert_eq!(o, ReorderOutput::default());
        let o = r.on_arrival(complete(1, SelfContained), t0, None);
        assert_eq!(o.deliver, vec![vec![1]]);
        assert!(!o.request_resync);
    }

    #[test]
    fn stale_frames_dropped() {
        let t0 = Instant::now();
        let mut r = live(t0);
        r.on_arrival(complete(1, Chained), t0, None);
        let o = r.on_arrival(complete(1, Chained), t0, None);
        assert!(o.deliver.is_empty());
        let o = r.on_arrival(complete(0, SelfContained), t0, None);
        assert!(o.deliver.is_empty());
        let o = r.on_arrival(RenderArrival::Reset { seq: 0 }, t0, None);
        assert_eq!(o, ReorderOutput::default());
    }

    #[test]
    fn next_deadline_tracks_overdue() {
        let t0 = Instant::now();
        let mut r = live(t0);
        assert_eq!(r.next_deadline(None), None);
        r.on_arrival(complete(3, Chained), t0 + ms(10), None);
        r.on_arrival(complete(2, Chained), t0 + ms(20), None);
        r.on_arrival(complete(5, Chained), t0 + ms(30), None);
        // Measured from the earliest held completion.
        assert_eq!(r.next_deadline(None), Some(t0 + ms(10) + ms(250)));
        assert_eq!(r.next_deadline(Some(ms(400))), Some(t0 + ms(10) + ms(400)));
        // Delivering seq 1 releases 2 and 3; seq 4 is now missing behind 5 (from t0+30).
        r.on_arrival(complete(1, Chained), t0 + ms(40), None);
        assert_eq!(r.next_deadline(None), Some(t0 + ms(30) + ms(250)));
        r.on_arrival(complete(4, Chained), t0 + ms(50), None);
        assert_eq!(r.next_deadline(None), None);
    }
}
