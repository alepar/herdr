//! Sender-side in-flight window: a sans-IO, delay-based controller (see `InflightWindow`).
//! Time is always an `Instant` parameter; nothing here reads a clock, does I/O or traces.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::session::{ACK_EVERY_BYTES, MAX_DATA_CHUNK, WINDOW};

/// Smallest window: two ack quanta, so a full window in flight always triggers the
/// receiver's byte-count ACK before its timer.
pub(super) const FLOOR: usize = 2 * ACK_EVERY_BYTES as usize;
/// Window on a fresh leg.
pub(super) const INITIAL: usize = 256 * 1024;
/// Hard cap: the Phase 1 fixed window.
pub(super) const CAP: usize = WINDOW;
/// Queueing delay above which an epoch shrinks the window.
pub(super) const TARGET_QUEUE_DELAY: Duration = Duration::from_millis(50);

/// Send records kept at most: one per ack quantum of a full `CAP` in flight, plus one partial.
const MAX_RECORDS: usize = CAP / ACK_EVERY_BYTES as usize + 1;

const _: () = assert!(FLOOR <= INITIAL && INITIAL <= CAP && MAX_DATA_CHUNK <= FLOOR);

/// The state an epoch closed with, for the owner to trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WindowSnapshot {
    pub window: usize,
    pub base_rtt: Option<Duration>,
    pub epoch_min: Option<Duration>,
}

/// First transmission of the fresh bytes below `end` that no earlier record covers.
#[derive(Debug, Clone, Copy)]
struct SendRecord {
    end: u64,
    sent_at: Instant,
}

/// Sender-side in-flight window: a sans-IO, delay-based (Vegas/LEDBAT-style) controller that
/// keeps about one link's worth of unacked DATA in flight.
///
/// Owns: the window value and its constants (`FLOOR`, `INITIAL`, `CAP`,
/// `TARGET_QUEUE_DELAY`); the record of first transmissions of fresh DATA, kept by offset and
/// coalesced to one record per `ACK_EVERY_BYTES` quantum (at most `MAX_RECORDS`; coalesced,
/// never dropped); RTT sampling (only ACKs advancing at least `ACK_EVERY_BYTES`, and never
/// replayed bytes: Karn's rule); `base_rtt` (minimum on the current leg, not aged) and the
/// per-epoch minimum; epochs (a byte-offset boundary closed by the first ACK covering it);
/// the hold epoch after every change; the shrink/grow/hold law; reset to `INITIAL`.
///
/// Does not own: the ring, sockets, the clock (every instant is an input), tracing, which
/// side it runs on, or when a leg is up. `SessionEndpoint` feeds it fresh sends, ACKs, the
/// window-limited signal and resets, and reads `current()` and the epoch snapshot; the
/// keeper traces snapshots.
///
/// New control behaviour and tuning go here only: no other code compares RTTs or applies
/// window arithmetic. Implausible inputs (an ACK below the oldest record or with no record,
/// an instant before the send, a replayed or empty send) are ignored, never reported.
#[derive(Debug)]
pub(super) struct InflightWindow {
    window: usize,
    /// First-send instants of fresh bytes in `[record_start, newest end)`, oldest first.
    records: VecDeque<SendRecord>,
    /// Bytes below this have no record: acked, or sent before the last reset.
    record_start: u64,
    /// End of the fresh data reported since the last reset; `None` before the first send.
    sent_end: Option<u64>,
    /// The peer's cumulative ACK as last reported.
    acked: u64,
    base_rtt: Option<Duration>,
    epoch_min: Option<Duration>,
    /// The epoch closes at the first ACK above this offset; `None` before the first send.
    epoch_boundary: Option<u64>,
    window_limited: bool,
    /// The current epoch's samples were sent under the previous window: no decision.
    hold: bool,
    snapshot: Option<WindowSnapshot>,
    #[cfg(test)]
    samples: u64,
}

impl InflightWindow {
    /// A controller at `INITIAL` with no samples; `acked` is the peer's ACK offset now.
    pub fn new(acked: u64) -> Self {
        InflightWindow {
            window: INITIAL,
            records: VecDeque::new(),
            record_start: acked,
            sent_end: None,
            acked,
            base_rtt: None,
            epoch_min: None,
            epoch_boundary: None,
            window_limited: false,
            hold: false,
            snapshot: None,
            #[cfg(test)]
            samples: 0,
        }
    }

    /// Bytes the owner may keep unacked in flight.
    pub fn current(&self) -> usize {
        self.window
    }

    /// Qualifying RTT samples taken since the last reset.
    #[cfg(test)]
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// A leg came up or went down: the path may differ, so start over.
    pub fn reset(&mut self, acked: u64) {
        *self = InflightWindow::new(acked);
    }

    /// Fresh DATA `[start, end)` went on the wire at `now`. Replays are not fresh data.
    pub fn on_send(&mut self, now: Instant, start: u64, end: u64) {
        if end <= start || self.sent_end.is_some_and(|sent| start < sent) {
            return;
        }
        if self.sent_end.is_none() {
            self.record_start = start;
        }
        if self.epoch_boundary.is_none() {
            self.epoch_boundary = Some(start);
        }
        self.sent_end = Some(end);
        let full = self.records.len() >= MAX_RECORDS;
        if let Some(newest) = self.records.back_mut() {
            if full || (newest.end - 1) / ACK_EVERY_BYTES == (end - 1) / ACK_EVERY_BYTES {
                newest.end = end;
                return;
            }
        }
        self.records.push_back(SendRecord { end, sent_at: now });
    }

    /// The owner had data and no room during this epoch.
    pub fn on_window_limited(&mut self) {
        self.window_limited = true;
    }

    /// The peer acknowledged everything below `acked` (cumulative) at `now`.
    pub fn on_ack(&mut self, now: Instant, acked: u64) {
        if acked <= self.acked {
            return;
        }
        let advance = acked - self.acked;
        self.acked = acked;
        // Only a byte-count ACK is free of the receiver's ack-timer delay.
        if advance >= ACK_EVERY_BYTES {
            if let Some(rtt) = self
                .first_send_of(acked - 1)
                .and_then(|sent_at| now.checked_duration_since(sent_at))
            {
                self.sample(rtt);
            }
        }
        while let Some(oldest) = self.records.front().copied() {
            if oldest.end > acked {
                break;
            }
            self.records.pop_front();
            self.record_start = oldest.end;
        }
        if self.epoch_boundary.is_some_and(|boundary| acked > boundary) {
            self.close_epoch();
        }
    }

    /// The last closed epoch's state, once.
    pub fn take_snapshot(&mut self) -> Option<WindowSnapshot> {
        self.snapshot.take()
    }

    fn first_send_of(&self, byte: u64) -> Option<Instant> {
        if byte < self.record_start {
            return None;
        }
        self.records
            .iter()
            .find(|record| record.end > byte)
            .map(|record| record.sent_at)
    }

    fn sample(&mut self, rtt: Duration) {
        #[cfg(test)]
        {
            self.samples += 1;
        }
        self.base_rtt = Some(self.base_rtt.map_or(rtt, |base| base.min(rtt)));
        self.epoch_min = Some(self.epoch_min.map_or(rtt, |min| min.min(rtt)));
    }

    fn close_epoch(&mut self) {
        let decide = !std::mem::take(&mut self.hold);
        if let (true, Some(epoch_min), Some(base_rtt)) = (decide, self.epoch_min, self.base_rtt) {
            let queueing = epoch_min.saturating_sub(base_rtt);
            let next = if queueing > TARGET_QUEUE_DELAY {
                (self.window * 3 / 4).max(FLOOR)
            } else if self.window_limited {
                (self.window + MAX_DATA_CHUNK).min(CAP)
            } else {
                self.window
            };
            if next != self.window {
                self.window = next;
                self.hold = true;
            }
        }
        self.snapshot = Some(WindowSnapshot {
            window: self.window,
            base_rtt: self.base_rtt,
            epoch_min: self.epoch_min,
        });
        self.epoch_min = None;
        self.window_limited = false;
        // The next epoch is judged by the bytes sent from here on.
        self.epoch_boundary = self.sent_end;
    }
}

#[cfg(test)]
mod tests {
    use super::super::session::ACK_EVERY;
    use super::*;

    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    const QUANTUM: u64 = ACK_EVERY_BYTES;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Synthetic clock: every instant is `t0 + offset`; `t0` is the only clock read.
    struct Clock(Instant);

    impl Clock {
        fn new() -> Self {
            Clock(Instant::now())
        }

        fn at(&self, offset: Duration) -> Instant {
            self.0 + offset
        }
    }

    /// Drives one epoch per round: one fresh send, acked `rtt` later.
    struct Driver {
        clock: Clock,
        w: InflightWindow,
        t: Duration,
        sent: u64,
    }

    impl Driver {
        fn new() -> Self {
            Driver {
                clock: Clock::new(),
                w: InflightWindow::new(0),
                t: Duration::ZERO,
                sent: 0,
            }
        }

        fn round_of(&mut self, len: u64, rtt: Duration, limited: bool) -> Option<WindowSnapshot> {
            let start = self.sent;
            self.sent += len;
            self.w.on_send(self.clock.at(self.t), start, self.sent);
            if limited {
                self.w.on_window_limited();
            }
            self.t += rtt;
            self.w.on_ack(self.clock.at(self.t), self.sent);
            self.t += ms(1);
            self.w.take_snapshot()
        }

        fn round(&mut self, rtt: Duration, limited: bool) -> WindowSnapshot {
            self.round_of(QUANTUM, rtt, limited)
                .expect("every round closes an epoch")
        }
    }

    #[test]
    fn queueing_above_target_shrinks_by_a_quarter_per_epoch_down_to_floor() {
        let mut d = Driver::new();
        assert_eq!(d.round(ms(100), false).window, INITIAL);
        let windows: Vec<usize> = (0..10).map(|_| d.round(ms(200), false).window).collect();
        // Every change is followed by a hold epoch.
        assert_eq!(
            windows,
            vec![192 * KIB, 192 * KIB, 144 * KIB, 144 * KIB, FLOOR, FLOOR, FLOOR, FLOOR, FLOOR, FLOOR]
        );
    }

    #[test]
    fn window_limited_epochs_without_queueing_grow_one_chunk_up_to_cap() {
        let mut d = Driver::new();
        let mut windows = vec![INITIAL];
        for _ in 0..80 {
            windows.push(d.round(ms(100), true).window);
        }
        assert_eq!(windows.last().copied(), Some(CAP));
        for pair in windows.windows(2) {
            assert!(
                pair[1] == pair[0] || pair[1] == pair[0] + MAX_DATA_CHUNK,
                "step {pair:?}"
            );
        }
        for triple in windows.windows(3) {
            assert!(
                !(triple[1] > triple[0] && triple[2] > triple[1]),
                "grew in consecutive epochs: {triple:?}"
            );
        }
    }

    #[test]
    fn an_idle_link_never_grows() {
        let mut d = Driver::new();
        for _ in 0..20 {
            assert_eq!(d.round(ms(100), false).window, INITIAL);
        }
    }

    #[test]
    fn reset_returns_to_initial_with_no_samples() {
        let mut d = Driver::new();
        d.round(ms(100), false);
        for _ in 0..4 {
            d.round(ms(300), false);
        }
        assert!(d.w.current() < INITIAL);
        d.w.reset(d.sent);
        assert_eq!(d.w.current(), INITIAL);
        assert_eq!((d.w.base_rtt, d.w.epoch_min), (None, None));
        assert!(d.w.records.is_empty());
        assert_eq!(d.w.take_snapshot(), None);
        // The new leg learns its own base: 300 ms is not queueing against the old 100 ms.
        let s = d.round(ms(300), false);
        assert_eq!((s.window, s.base_rtt), (INITIAL, Some(ms(300))));
        assert_eq!(d.round(ms(300), false).window, INITIAL);
    }

    #[test]
    fn acks_advancing_less_than_a_quantum_are_not_sampled() {
        let clock = Clock::new();
        let mut w = InflightWindow::new(0);
        w.on_send(clock.at(ms(0)), 0, QUANTUM);
        w.on_ack(clock.at(ms(100)), QUANTUM / 2);
        w.on_ack(clock.at(ms(150)), QUANTUM);
        assert_eq!((w.samples, w.base_rtt), (0, None));
        w.on_send(clock.at(ms(200)), QUANTUM, 2 * QUANTUM);
        w.on_ack(clock.at(ms(300)), 2 * QUANTUM);
        assert_eq!((w.samples, w.base_rtt), (1, Some(ms(100))));
    }

    #[test]
    fn replayed_bytes_after_a_reset_are_never_sampled() {
        let clock = Clock::new();
        let mut w = InflightWindow::new(0);
        for i in 0..4 {
            w.on_send(clock.at(ms(0)), i * QUANTUM, (i + 1) * QUANTUM);
        }
        // Leg down with nothing acked: the next leg replays [0, 4 quanta) unreported and
        // sends fresh bytes from 4 quanta on.
        w.reset(0);
        w.on_send(clock.at(ms(1000)), 4 * QUANTUM, 5 * QUANTUM);
        w.on_ack(clock.at(ms(1100)), 2 * QUANTUM);
        w.on_ack(clock.at(ms(1150)), 4 * QUANTUM);
        assert_eq!(w.samples, 0, "replayed bytes were sampled");
        w.on_ack(clock.at(ms(1200)), 5 * QUANTUM);
        assert_eq!((w.samples, w.base_rtt), (1, Some(ms(200))));
    }

    #[test]
    fn implausible_inputs_are_ignored_without_panic() {
        let clock = Clock::new();
        let mut w = InflightWindow::new(0);
        // An ACK with an empty send record.
        w.on_ack(clock.at(ms(10)), QUANTUM);
        // Empty and inverted sends.
        w.on_send(clock.at(ms(20)), 3 * QUANTUM, 3 * QUANTUM);
        w.on_send(clock.at(ms(20)), 4 * QUANTUM, 3 * QUANTUM);
        assert!(w.records.is_empty());
        // Records start at 4 quanta: an ACK below the oldest record has no send time.
        w.on_send(clock.at(ms(30)), 4 * QUANTUM, 5 * QUANTUM);
        w.on_ack(clock.at(ms(40)), 3 * QUANTUM);
        // An ACK going backwards.
        w.on_ack(clock.at(ms(50)), 2 * QUANTUM);
        // An ACK instant before the send instant.
        w.on_send(clock.at(ms(500)), 5 * QUANTUM, 6 * QUANTUM);
        w.on_ack(clock.at(ms(100)), 6 * QUANTUM);
        assert_eq!((w.samples, w.current()), (0, INITIAL));
        // A send overlapping reported fresh data is a replay: not recorded.
        w.on_send(clock.at(ms(600)), 5 * QUANTUM, 7 * QUANTUM);
        w.on_send(clock.at(ms(700)), 6 * QUANTUM, 7 * QUANTUM);
        w.on_ack(clock.at(ms(800)), 7 * QUANTUM);
        assert_eq!(w.base_rtt, Some(ms(100)));
    }

    #[test]
    fn small_sends_coalesce_per_ack_quantum_and_still_sample() {
        let clock = Clock::new();
        let mut w = InflightWindow::new(0);
        let piece = 4 * KIB as u64;
        let mut sent = 0;
        for i in 0..CAP as u64 / piece {
            w.on_send(clock.at(ms(i)), sent, sent + piece);
            sent += piece;
            assert!(w.records.len() <= CAP / QUANTUM as usize + 1);
        }
        assert_eq!(w.records.len(), CAP / QUANTUM as usize);
        // Byte 64 KiB - 1 went out at 15 ms; its quantum's record keeps the first instant (0).
        w.on_ack(clock.at(ms(1000)), QUANTUM);
        assert_eq!(w.base_rtt, Some(ms(1000)));
        // The second quantum's record started with the send at 16 ms.
        w.on_ack(clock.at(ms(1100)), 2 * QUANTUM);
        assert_eq!((w.samples, w.epoch_min), (2, Some(ms(1084))));
        // Past the bound records are coalesced into the newest, never dropped.
        for i in 0..CAP as u64 / piece {
            w.on_send(clock.at(ms(2000 + i)), sent, sent + piece);
            sent += piece;
        }
        assert_eq!(w.records.len(), MAX_RECORDS);
        assert_eq!(w.records.back().map(|r| r.end), Some(sent));
        assert_eq!(w.first_send_of(2 * QUANTUM), Some(clock.at(ms(32))));
    }

    #[test]
    fn an_epoch_without_a_qualifying_sample_holds() {
        let mut d = Driver::new();
        d.round(ms(100), false);
        for _ in 0..6 {
            // Half-quantum rounds: timer-style ACKs, no sample, whatever the delay or limit.
            let s = d.round_of(QUANTUM / 2, ms(400), true).expect("epoch closes");
            assert_eq!((s.window, s.epoch_min), (INITIAL, None));
        }
    }

    #[test]
    fn the_epoch_after_a_change_holds_but_updates_base_rtt() {
        let mut d = Driver::new();
        assert_eq!(d.round(ms(100), true).window, INITIAL + MAX_DATA_CHUNK);
        let s = d.round(ms(80), true);
        assert_eq!(
            s,
            WindowSnapshot {
                window: INITIAL + MAX_DATA_CHUNK,
                base_rtt: Some(ms(80)),
                epoch_min: Some(ms(80)),
            }
        );
        assert_eq!(d.round(ms(80), true).window, INITIAL + 2 * MAX_DATA_CHUNK);
        // A hold epoch with queueing does not shrink either; the next decision does.
        assert_eq!(d.round(ms(300), false).window, INITIAL + 2 * MAX_DATA_CHUNK);
        assert_eq!(
            d.round(ms(300), false).window,
            (INITIAL + 2 * MAX_DATA_CHUNK) * 3 / 4
        );
    }

    /// The spec §4 line (depart = max(t, prev_depart) + len/rate, arrive = depart + delay)
    /// with the receiver's ack policy (ACK at ACK_EVERY_BYTES delivered or ACK_EVERY after the
    /// first unacked delivery) around one controller, stepped in 1 ms ticks of a synthetic
    /// clock. The source is a flood whose sends are at most `chunk` bytes. ACKs are tiny:
    /// propagation only on the way back.
    struct LineSim {
        clock: Clock,
        w: InflightWindow,
        rate: u64,
        one_way_us: u64,
        chunk: usize,
        sent: u64,
        acked: u64,
        prev_depart_us: u64,
        /// (arrival µs, end offset) of chunks on the forward line.
        in_line: VecDeque<(u64, u64)>,
        delivered: u64,
        last_ack_sent: u64,
        ack_pending_since_us: Option<u64>,
        /// (arrival µs, acked offset) of ACKs on the return line.
        acks: VecDeque<(u64, u64)>,
        qualifying_acks: u64,
        /// (µs, window) every 10 ms.
        windows: Vec<(u64, usize)>,
        /// (µs, snapshot) per closed epoch.
        epochs: Vec<(u64, WindowSnapshot)>,
    }

    impl LineSim {
        fn new(rate: u64, one_way: Duration, chunk: usize) -> Self {
            LineSim {
                clock: Clock::new(),
                w: InflightWindow::new(0),
                rate,
                one_way_us: one_way.as_micros() as u64,
                chunk,
                sent: 0,
                acked: 0,
                prev_depart_us: 0,
                in_line: VecDeque::new(),
                delivered: 0,
                last_ack_sent: 0,
                ack_pending_since_us: None,
                acks: VecDeque::new(),
                qualifying_acks: 0,
                windows: Vec::new(),
                epochs: Vec::new(),
            }
        }

        fn run(&mut self, total: Duration) {
            let end_us = total.as_micros() as u64;
            let mut t = 0;
            while t <= end_us {
                self.tick(t);
                t += 1_000;
            }
        }

        fn send_ack(&mut self, at_us: u64) {
            self.last_ack_sent = self.delivered;
            self.ack_pending_since_us = None;
            self.acks.push_back((at_us + self.one_way_us, self.delivered));
        }

        fn tick(&mut self, t: u64) {
            let now = self.clock.at(Duration::from_micros(t));
            // Receiver: deliveries, then the byte-count or timer ACK.
            while let Some(&(at, end)) = self.in_line.front() {
                if at > t {
                    break;
                }
                self.in_line.pop_front();
                self.delivered = end;
                if self.ack_pending_since_us.is_none() {
                    self.ack_pending_since_us = Some(at);
                }
                if self.delivered - self.last_ack_sent >= ACK_EVERY_BYTES {
                    self.send_ack(at);
                }
            }
            if let Some(since) = self.ack_pending_since_us {
                if t >= since + ACK_EVERY.as_micros() as u64 {
                    self.send_ack(t);
                }
            }
            // Sender: ACK arrivals.
            while let Some(&(at, offset)) = self.acks.front() {
                if at > t {
                    break;
                }
                self.acks.pop_front();
                if offset - self.acked >= ACK_EVERY_BYTES {
                    self.qualifying_acks += 1;
                }
                self.acked = offset;
                self.w.on_ack(now, offset);
            }
            if let Some(snapshot) = self.w.take_snapshot() {
                self.epochs.push((t, snapshot));
            }
            // Sender: the flood fills the window.
            loop {
                let in_flight = (self.sent - self.acked) as usize;
                let room = self.w.current().saturating_sub(in_flight);
                if room == 0 {
                    self.w.on_window_limited();
                    break;
                }
                let len = room.min(self.chunk) as u64;
                let depart = t.max(self.prev_depart_us) + len * 1_000_000 / self.rate;
                self.prev_depart_us = depart;
                self.in_line
                    .push_back((depart + self.one_way_us, self.sent + len));
                self.w.on_send(now, self.sent, self.sent + len);
                self.sent += len;
            }
            if t.is_multiple_of(10_000) {
                self.windows.push((t, self.w.current()));
            }
        }
    }

    fn median(mut values: Vec<usize>) -> usize {
        values.sort_unstable();
        values[values.len() / 2]
    }

    #[test]
    fn line_model_keeps_the_window_near_the_bandwidth_delay_product() {
        for rate in [256 * KIB as u64, MIB as u64] {
            let mut sim = LineSim::new(rate, ms(150), MAX_DATA_CHUNK);
            sim.run(Duration::from_secs(60));
            let bound = FLOOR.max((rate * 6 / 10) as usize) + MAX_DATA_CHUNK;
            let last_5s: Vec<usize> = sim
                .windows
                .iter()
                .filter(|(t, _)| *t >= 55_000_000)
                .map(|&(_, w)| w)
                .collect();
            let med = median(last_5s);
            assert!(med <= bound, "rate {rate}: median {med} > bound {bound}");
            let late: Vec<WindowSnapshot> = sim
                .epochs
                .iter()
                .filter(|(t, _)| *t >= 10_000_000)
                .map(|&(_, s)| s)
                .collect();
            assert!(late.len() >= 10, "rate {rate}: only {} epochs", late.len());
            let over = late.iter().filter(|s| s.window > bound).count();
            assert!(
                over * 3 <= late.len(),
                "rate {rate}: {over} of {} epochs above {bound}",
                late.len()
            );
            assert!(sim.windows.iter().all(|&(_, w)| (FLOOR..=CAP).contains(&w)));
        }
    }

    #[test]
    fn slow_link_with_small_chunks_counts_samples_and_holds_without_them() {
        // 8 KiB sends at 256 KiB/s: the 200 ms timer always fires before 64 KiB is
        // delivered, so no ACK qualifies. 32 KiB sends: every second delivery does.
        for chunk in [8 * KIB, 32 * KIB] {
            let mut sim = LineSim::new(256 * KIB as u64, ms(150), chunk);
            sim.run(Duration::from_secs(30));
            assert_eq!(sim.w.samples, sim.qualifying_acks, "chunk {chunk}");
            let mut previous = INITIAL;
            let mut sampleless = 0;
            for (_, s) in &sim.epochs {
                if s.epoch_min.is_none() {
                    assert_eq!(s.window, previous, "chunk {chunk}: changed without a sample");
                    sampleless += 1;
                }
                previous = s.window;
            }
            if chunk == 8 * KIB {
                assert_eq!(sim.qualifying_acks, 0);
                assert!(sampleless >= 10, "only {sampleless} sample-less epochs");
            } else {
                assert!(sim.qualifying_acks > 0);
            }
        }
    }
}
