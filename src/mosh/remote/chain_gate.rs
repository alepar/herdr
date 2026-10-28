//! The keeper's render-frame routing table.
//!
//! Owns: the open/closed chain state and the routing of each classified server frame to the
//! ring, a leg's render lane, or the floor; the inject-resync verdict when a lane rejects
//! while its leg is up.
//! Hides: nothing beyond the one bit of chain state; it is sans-IO and sees only classes.
//! Must not: read frames, touch a stream or a lane, or know about sessions.
//! Lifecycle: one per render-split session, created open; closed by every leg end.

use crate::remote::attach::mosh::link_frames::FrameClass;

/// What the current leg offers a render frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LegState {
    Down,
    UpNoLane,
    UpLane,
}

/// Where a frame goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Ring,
    Lane,
    Drop,
}

/// The caller must queue one resync request for the server.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct InjectResync;

pub(super) struct ChainGate {
    open: bool,
}

impl ChainGate {
    pub(super) fn new() -> ChainGate {
        ChainGate { open: true }
    }

    /// `Reliable` always goes to the ring. A render frame with no leg is dropped and closes
    /// the gate; while closed, `Chained` is dropped and `SelfContained` opens the gate and is
    /// routed; while open it goes to the lane, or to the ring on a lane-less leg.
    pub(super) fn route(&mut self, class: FrameClass, leg: LegState) -> Route {
        if class == FrameClass::Reliable {
            return Route::Ring;
        }
        if leg == LegState::Down {
            self.open = false;
            return Route::Drop;
        }
        if !self.open {
            if class == FrameClass::Chained {
                return Route::Drop;
            }
            self.open = true;
        }
        match leg {
            LegState::UpLane => Route::Lane,
            _ => Route::Ring,
        }
    }

    /// The lane rejected an offer while the leg is up: close; the caller injects a resync.
    #[must_use]
    pub(super) fn lane_rejected(&mut self) -> InjectResync {
        self.open = false;
        InjectResync
    }

    /// Any leg end (drop, steal, verdict): close, no inject.
    pub(super) fn leg_ended(&mut self) {
        self.open = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FrameClass::{Chained, Reliable, SelfContained};

    fn closed_gate() -> ChainGate {
        let mut gate = ChainGate::new();
        gate.leg_ended();
        gate
    }

    #[test]
    fn reliable_always_ring() {
        for leg in [LegState::Down, LegState::UpNoLane, LegState::UpLane] {
            let mut open = ChainGate::new();
            assert_eq!(open.route(Reliable, leg), Route::Ring);
            assert!(open.open, "reliable must not close an open gate");
            let mut closed = closed_gate();
            assert_eq!(closed.route(Reliable, leg), Route::Ring);
            assert!(!closed.open, "reliable must not open a closed gate");
        }
    }

    #[test]
    fn down_drops_and_closes() {
        for class in [SelfContained, Chained] {
            let mut gate = ChainGate::new();
            assert_eq!(gate.route(class, LegState::Down), Route::Drop);
            assert!(!gate.open);
            // Still closed afterwards: a chained frame on a live leg is dropped.
            assert_eq!(gate.route(Chained, LegState::UpLane), Route::Drop);
        }
    }

    #[test]
    fn closed_drops_chained() {
        for leg in [LegState::UpNoLane, LegState::UpLane] {
            let mut gate = closed_gate();
            assert_eq!(gate.route(Chained, leg), Route::Drop);
            assert_eq!(gate.route(Chained, leg), Route::Drop);
            assert!(!gate.open);
        }
    }

    #[test]
    fn closed_self_contained_opens_and_routes() {
        let mut gate = closed_gate();
        assert_eq!(gate.route(SelfContained, LegState::UpLane), Route::Lane);
        assert!(gate.open);
        assert_eq!(gate.route(Chained, LegState::UpLane), Route::Lane);
        let mut gate = closed_gate();
        assert_eq!(gate.route(SelfContained, LegState::UpNoLane), Route::Ring);
        assert!(gate.open);
    }

    #[test]
    fn open_with_lane_goes_to_lane() {
        let mut gate = ChainGate::new();
        assert_eq!(gate.route(Chained, LegState::UpLane), Route::Lane);
        assert_eq!(gate.route(SelfContained, LegState::UpLane), Route::Lane);
    }

    #[test]
    fn open_without_lane_goes_to_ring() {
        let mut gate = ChainGate::new();
        assert_eq!(gate.route(Chained, LegState::UpNoLane), Route::Ring);
        assert_eq!(gate.route(SelfContained, LegState::UpNoLane), Route::Ring);
    }

    #[test]
    fn leg_end_closes() {
        let mut gate = ChainGate::new();
        gate.leg_ended();
        assert_eq!(gate.route(Chained, LegState::UpNoLane), Route::Drop);
    }

    #[test]
    fn lane_reject_closes_and_injects() {
        let mut gate = ChainGate::new();
        assert_eq!(gate.lane_rejected(), InjectResync);
        assert_eq!(gate.route(Chained, LegState::UpLane), Route::Drop);
        assert_eq!(gate.route(SelfContained, LegState::UpLane), Route::Lane);
    }
}
