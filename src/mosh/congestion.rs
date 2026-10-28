//! The QUIC leg's congestion controller: NewReno that does not read random loss as congestion.
//!
//! Owns: `LossTolerantReno`, its factory, and the constants of its two reactions.
//! Hides: quinn-proto's `Controller` trait; the rest of the crate only names the factory.
//! Must not: know about streams, frames or the render lane.
//! Lifecycle: one controller per connection, built by quinn from the factory in
//! `quic::transport_config`; its state is the window and two byte counters.
//!
//! Why not quinn's own: Cubic and NewReno cut the window by a half (Cubic by 30 %) on every
//! loss event, and at 10 % random loss and 300 ms RTT a loss event occurs every round trip, so
//! the window sat at a few packets and the leg carried 10 to 50 KB/s (mosh-19g.2.19, measured:
//! 18 of 20 seconds without a screen change). Here a loss event removes 1 % of the window
//! unless a large share of what was sent since the last event was lost: then it is the classic
//! half. Slow start goes on through light loss, so the window finds the path's capacity
//! instead of the loss rate's. The render lane bounds the data in flight, and the window is
//! capped, so the leg cannot queue more than `MAX_WINDOW` on a slow link.

use std::any::Any;
use std::sync::Arc;
use std::time::Instant;

use quinn::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;

/// A loss event removes this fraction of the window ...
const LIGHT_LOSS_KEEP: f32 = 0.99;
/// ... unless more than `HEAVY_LOSS_PERCENT` of the bytes resolved since the last event were
/// lost; then it keeps this fraction (the classic halving).
const HEAVY_LOSS_KEEP: f32 = 0.5;
const HEAVY_LOSS_PERCENT: u64 = 30;
/// Packets of window a connection starts with.
const INITIAL_PACKETS: u64 = 10;
/// Packets of window below which it never goes.
const MIN_PACKETS: u64 = 2;
/// The window never grows past this.
const MAX_WINDOW: u64 = 1024 * 1024;

/// The controller's state and rules, free of quinn's types so tests drive it directly.
#[derive(Clone, Debug)]
struct Window {
    mtu: u64,
    window: u64,
    ssthresh: u64,
    recovery_start: Instant,
    bytes_acked: u64,
    /// Bytes acknowledged / declared lost since the last reduction.
    acked_since: u64,
    lost_since: u64,
}

impl Window {
    fn new(now: Instant, mtu: u16) -> Window {
        let mtu = u64::from(mtu);
        Window {
            mtu,
            window: INITIAL_PACKETS * mtu,
            ssthresh: u64::MAX,
            recovery_start: now,
            bytes_acked: 0,
            acked_since: 0,
            lost_since: 0,
        }
    }

    fn minimum(&self) -> u64 {
        MIN_PACKETS * self.mtu
    }

    fn on_ack(&mut self, sent: Instant, bytes: u64, app_limited: bool) {
        self.acked_since = self.acked_since.saturating_add(bytes);
        if app_limited || sent <= self.recovery_start {
            return;
        }
        if self.window < self.ssthresh {
            self.window += bytes;
        } else {
            self.bytes_acked += bytes;
            if self.bytes_acked >= self.window {
                self.bytes_acked -= self.window;
                self.window += self.mtu;
            }
        }
        self.window = self.window.min(MAX_WINDOW);
    }

    fn on_loss(&mut self, now: Instant, sent: Instant, lost_bytes: u64, persistent: bool) {
        self.lost_since = self.lost_since.saturating_add(lost_bytes);
        if sent <= self.recovery_start {
            return;
        }
        self.recovery_start = now;
        let resolved = self.acked_since.saturating_add(self.lost_since).max(1);
        let heavy = self.lost_since.saturating_mul(100) > resolved * HEAVY_LOSS_PERCENT;
        self.acked_since = 0;
        self.lost_since = 0;
        if heavy {
            self.window = ((self.window as f32 * HEAVY_LOSS_KEEP) as u64).max(self.minimum());
            self.ssthresh = self.window;
        } else {
            self.window = ((self.window as f32 * LIGHT_LOSS_KEEP) as u64).max(self.minimum());
        }
        if persistent {
            self.window = self.minimum();
            self.ssthresh = self.window;
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct LossTolerantReno {
    state: Window,
}

impl Controller for LossTolerantReno {
    fn on_ack(
        &mut self,
        _now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        _rtt: &RttEstimator,
    ) {
        self.state.on_ack(sent, bytes, app_limited);
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        self.state.on_loss(now, sent, lost_bytes, is_persistent_congestion);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.state.mtu = u64::from(new_mtu);
        self.state.window = self.state.window.max(self.state.minimum());
    }

    fn window(&self) -> u64 {
        self.state.window
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        INITIAL_PACKETS * self.state.mtu
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Builds a `LossTolerantReno` for each connection.
#[derive(Debug, Default)]
pub(super) struct LossTolerantRenoConfig;

impl ControllerFactory for LossTolerantRenoConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(LossTolerantReno {
            state: Window::new(now, current_mtu),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MTU: u16 = 1200;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// A window of 200 kB in slow start with `acked` bytes resolved since the last event.
    fn grown(t0: Instant, acked: u64) -> Window {
        let mut w = Window::new(t0, MTU);
        w.window = 200_000;
        w.acked_since = acked;
        w
    }

    #[test]
    fn starts_at_ten_packets() {
        assert_eq!(Window::new(Instant::now(), MTU).window, 12_000);
    }

    #[test]
    fn light_loss_takes_one_percent_and_keeps_slow_start() {
        let t0 = Instant::now();
        // 10 % of the resolved bytes lost: the random loss this exists for.
        let mut w = grown(t0, 108_000);
        w.on_loss(at(t0, 500), at(t0, 100), 12_000, false);
        assert_eq!(w.window, 198_000);
        assert_eq!(w.ssthresh, u64::MAX, "slow start goes on");
        // And it keeps growing by what is acknowledged.
        w.on_ack(at(t0, 600), 1200, false);
        assert_eq!(w.window, 199_200);
    }

    #[test]
    fn heavy_loss_halves_and_leaves_slow_start() {
        let t0 = Instant::now();
        // 50 % of the resolved bytes lost.
        let mut w = grown(t0, 60_000);
        w.on_loss(at(t0, 500), at(t0, 100), 60_000, false);
        assert_eq!(w.window, 100_000);
        assert_eq!(w.ssthresh, 100_000);
    }

    #[test]
    fn the_heavy_threshold_is_thirty_percent() {
        let t0 = Instant::now();
        let mut at_threshold = grown(t0, 70_000);
        at_threshold.on_loss(at(t0, 500), at(t0, 100), 30_000, false);
        assert_eq!(at_threshold.window, 198_000, "exactly 30 % is still light");
        let mut over = grown(t0, 69_000);
        over.on_loss(at(t0, 500), at(t0, 100), 31_000, false);
        assert_eq!(over.window, 100_000);
    }

    #[test]
    fn counters_restart_after_a_reduction() {
        let t0 = Instant::now();
        let mut w = grown(t0, 100_000);
        w.on_loss(at(t0, 500), at(t0, 100), 1200, false);
        assert_eq!((w.acked_since, w.lost_since), (0, 0));
    }

    #[test]
    fn one_reduction_per_round_of_losses() {
        let t0 = Instant::now();
        let mut w = grown(t0, 100_000);
        w.on_loss(at(t0, 500), at(t0, 100), 1200, false);
        let once = w.window;
        // A packet sent before the reduction is lost too: the same loss event.
        w.on_loss(at(t0, 510), at(t0, 400), 1200, false);
        assert_eq!(w.window, once);
        assert_eq!(w.lost_since, 1200, "but its bytes count toward the next ratio");
    }

    #[test]
    fn persistent_congestion_goes_to_the_minimum() {
        let t0 = Instant::now();
        let mut w = grown(t0, 100_000);
        w.on_loss(at(t0, 500), at(t0, 100), 1200, true);
        assert_eq!(w.window, 2 * 1200);
        assert_eq!(w.ssthresh, w.window);
    }

    #[test]
    fn repeated_light_loss_never_drops_below_the_minimum() {
        let t0 = Instant::now();
        let mut w = Window::new(t0, MTU);
        for round in 1..2000u64 {
            w.acked_since = 100_000;
            w.on_loss(at(t0, round * 10), at(t0, round * 10 - 5), 1200, false);
        }
        assert!(w.window >= 2 * 1200);
    }

    #[test]
    fn congestion_avoidance_adds_one_packet_per_window() {
        let t0 = Instant::now();
        let mut w = Window::new(t0, MTU);
        w.window = 12_000;
        w.ssthresh = 12_000;
        for _ in 0..10 {
            w.on_ack(at(t0, 10), 1200, false);
        }
        assert_eq!(w.window, 13_200);
    }

    #[test]
    fn app_limited_acks_do_not_grow_the_window() {
        let t0 = Instant::now();
        let mut w = Window::new(t0, MTU);
        w.on_ack(at(t0, 10), 1200, true);
        assert_eq!(w.window, 12_000);
    }

    #[test]
    fn the_window_is_capped() {
        let t0 = Instant::now();
        let mut w = Window::new(t0, MTU);
        w.window = MAX_WINDOW;
        w.on_ack(at(t0, 10), 1_000_000, false);
        assert_eq!(w.window, MAX_WINDOW);
    }
}
