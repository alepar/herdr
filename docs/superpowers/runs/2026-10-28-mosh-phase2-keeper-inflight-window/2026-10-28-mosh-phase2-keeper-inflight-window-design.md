# Mosh Phase 2: adaptive keeper in-flight window

## Goal

Under an output flood on a slow, high-latency link (300 ms RTT), pressing Ctrl-C in a remote pane
shows the interrupted command's result on the local screen within 1.5 s, because the keeper keeps
only about one link's worth of unacked output in flight instead of a fixed 2 MiB.

Builds on Phase 1 (`docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/`, code in
`src/mosh/`). Vision doc: `HERDR_MOSH_VISION.md` §4.2 and §5 (Phase 2 row), decision D3.

## Problem description

Phase 1 caps unacked bytes per direction at a fixed `WINDOW` of 2 MiB (`src/mosh/session.rs`,
used in `SessionEndpoint::may_read_source` and `poll_send`). The keeper stops reading the herdr
server's client socket only once 2 MiB sit unacked in its ring. On a link slower than the server's
output, those 2 MiB are a queue of stale frames: at 1 MiB/s they take 2 s to drain, and everything
the user does after Ctrl-C waits behind them. The server already coalesces frames when its client
socket stops draining (a single latest-wins render slot), so the fix is to stop draining sooner:
keep the window near the link's bandwidth-delay product, so the server, not the keeper, holds the
backlog and discards stale frames.

## Main challenges

The window has to track a link the endpoints cannot see, using only the existing session messages
(no protocol change, no new Cargo dependency, no upstream edit beyond the 20-line hook budget).
The receiver acks after delivery, every 64 KiB (`ACK_EVERY_BYTES`) or 200 ms (`ACK_EVERY`); a
window below two ack quanta would stall on the 200 ms timer and poison the RTT samples, and
changing the receiver's ack policy is a wire change older peers would not share. A second queue
the keeper cannot shrink sits upstream of it: the kernel buffer of the server's Unix socket
(about 200 KB by default). And the acceptance has to be measured without root or `tc`, inside the
fork's existing fake-ssh harness.

## Key decisions made

The window is a sender-side, delay-based controller (Vegas/LEDBAT family) in a new fork module,
fed only by DATA send times and ACK arrival. The receiver and the session protocol are unchanged:
`WINDOW` stays as the hard cap, so a new sender talking to a Phase 1 receiver still works, and
`SESSION_PROTOCOL_VERSION` does not change. The window floor is two ack quanta (128 KiB). Link
emulation is a delay-and-rate relay the fake `ssh` inserts when a harness knob is set, written in
Perl like the harness's existing throttle. D3 is applied as written: the server-side backlog is
measured and recorded; server pacing is not built in this run. If the measurement shows the
backlog is material, it is recorded as a parked follow-up with numbers, because pacing needs an
RTT signal inside the server and upstream edits beyond the hook budget.

## Decision points, by section

### 1. Window controller: delay-based, sender-side (chosen)

**Recommended:** a new `InflightWindow` type in `src/mosh/window.rs`, owned by each
`SessionEndpoint`. It records the first-transmission instant of each DATA chunk's end offset (a
bounded deque, at most `WINDOW / MAX_DATA_CHUNK` entries) and takes an RTT sample on each ACK that
advances the acked offset by at least `ACK_EVERY_BYTES`. Such an ACK was triggered by byte count,
not by the 200 ms timer, so the sample is not inflated by the receiver's ack delay. The sample is
`now - first_send(byte at ack-1)`. Replayed chunks are never sampled (Karn's rule): the deque is
cleared when a leg goes up or down. State: `base_rtt`, the minimum sample on the current leg;
`epoch_min`, the minimum sample in the current epoch; `window`. Once per epoch (an epoch ends
when an ACK covers the first byte sent after the epoch began, about one RTT):

- queueing delay `q = epoch_min - base_rtt`;
- if `q > TARGET_QUEUE_DELAY` (50 ms): `window = max(FLOOR, window * 3 / 4)`;
- otherwise, if the sender was window-limited during the epoch (it had bytes to send and no
  room): `window = min(CAP, window + MAX_DATA_CHUNK)`;
- otherwise unchanged. An idle link never grows the window, so a sudden flood after idle starts
  from a small window, not the cap.

Constants: `FLOOR = 2 * ACK_EVERY_BYTES` (128 KiB), `INITIAL = 256 KiB`, `CAP = WINDOW` (2 MiB),
`TARGET_QUEUE_DELAY = 50 ms`. A new leg resets the controller to `INITIAL` with no samples,
because a reconnect may be a different path.

**Considered:**
- A fixed smaller window (for example 256 KiB). Simplest, but too large for a 128 KiB/s link and
  too small for a fast long-haul one. Rejected: does not adapt.
- A BBR-style window, `gain * delivery_rate * min_rtt`. Needs a delivery-rate estimator and pacing
  to be stable. Rejected as more machinery than a TUI's traffic needs. The delay signal directly
  measures the staleness we want to bound.
- Heartbeat echo for RTT. Needs a new session message or field, which is a protocol change.
  Rejected.

### 2. Integration into `SessionEndpoint` (chosen: replace the two `WINDOW` uses)

**Recommended:** `may_read_source` and `poll_send` use `self.window.current()` in place of
`WINDOW` while a leg is up. `poll_send` reports each fresh DATA chunk to the controller, and
whether it stopped for lack of room (window-limited). The ACK path reports the new acked offset;
the leg-up and leg-down paths reset it. While no leg is up, the ring-cap rule is unchanged.
`push_source`'s debug assertion is restated against the cap. `WINDOW` keeps its value and becomes
the cap. The session unit tests that assert in-flight equals `WINDOW` are rewritten to assert the
controller's window. The same endpoint code runs on both sides, so the upstream direction (client
input) gets the controller too. Its traffic is small, so it stays at `INITIAL`, which is harmless.

**Considered:** a keeper-only window outside `SessionEndpoint`. Rejected: the endpoint owns all
flow control (its own documentation says so), and a second gate in the keeper would split that
ownership.

### 3. Keeper source reads sized to the window (chosen)

**Recommended:** the keeper's `server_reader` reads at most
`min(MAX_DATA_CHUNK, window - occupancy)` bytes, rounded up to 4 KiB, through a new
`SessionEndpoint::source_room()`. Otherwise one 64 KiB read can overshoot a 128 KiB window by
half. The local bridge's client reader does the same for symmetry.

**Considered:** let a read overshoot by up to one chunk. Rejected: at the floor that is 50 % more
queue than intended.

### 4. Link emulation harness (chosen: Perl delay line behind a knob)

**Recommended:** a `link` knob in `tests/mosh_harness.rs` holding
`<one-way delay ms> <rate bytes/s>`. When it is present, the fake `ssh` pipes both directions of
a session or plain leg through a delay line: a Perl script, written by the harness next to the
fake `ssh`. Each chunk read at time `t` is released at `max(t + delay, previous release + len /
rate)`, so the line models propagation delay plus a serializing bottleneck. It is single
threaded, uses `select`, and keeps an in-memory queue. It follows the existing `throttle` knob's
pattern, needs no root and no `tc`, and runs wherever the harness runs. Loss is not emulated. On a
TCP link, loss shows up as stalls, and the loss phases (3a and 3b) own that behaviour.
`Remote::set_link(delay, rate)` and `clear_link()` are the helpers.

**Considered:**
- `tc netem`: needs root and Linux only. Rejected for CI.
- A Rust relay thread inside the test process: the fake `ssh` is a separate process started by
  herdr, so it would need a socket rendezvous back into the test. Rejected as more moving parts
  than a 40-line script.

### 5. Acceptance test and profile (chosen)

**Recommended:** `tests/mosh_pacing.rs`:

- **Ctrl-C under flood:** link `150 ms` one way and rate `R`. A pane runs `yes; echo <marker>`,
  and the test waits until the client output has grown for 2 s of flood. It writes `\x03` into the
  client pty and measures the time until the client screen shows the marker. The assertion is
  `<= 1.5 s` (5 RTT).
- **No throughput regression:** with no link knob (LAN), the keeper trace shows the window
  growing to at least 1 MiB under the same flood.
- **Window converges:** under the slow profile, the keeper's traced window settles to at most
  `max(FLOOR, 2 * R * 0.3 s)` plus one chunk.

`R` is fixed by the measurement leaf (§6) as the largest of 256 KiB/s and 1 MiB/s that is at most
half the server's measured flood output rate at the test terminal size. The terminal size may be
raised from 80x24 to reach a usable flood rate. With the Phase 1 fixed window the same test must
fail the 1.5 s bound. That baseline is measured once and recorded as evidence, not kept as a CI
test.

**Considered:** a CI test that runs both the fixed and the adaptive window. It would need a
production knob to force the fixed window. Rejected: configurability nobody needs.

### 6. Measurement and D3 (chosen: measure, record, do not build pacing)

**Recommended:** a measurement leaf runs first on the link harness against the current code
(fixed window). It records in the run's `evidence/`:

- (a) the server's flood output rate at the test terminal size;
- (b) the Ctrl-C latency at the chosen profile (the baseline);
- (c) the server-socket backlog: the keeper traces `FIONREAD` on its server stream once per
  window epoch, as `server_backlog` bytes.

Once the window lands, the acceptance leaf re-records (b) and (c). Backlog staleness is
`backlog / R`. If its 95th percentile exceeds 250 ms, the upper clamp in D3, the evidence file
says so and server pacing is parked as a measured follow-up. It is not built here: it needs an RTT
signal delivered into the server and upstream edits outside the budget. If the acceptance bound
still holds, Phase 2 is done either way.

**Considered:** shrinking the server's per-client `SO_SNDBUF` through a one-line upstream hook.
Cheaper than pacing, but it changes behaviour for every local client too, and it is a server
change D3 reserves for evidence. Recorded as an option in the follow-up if (c) is material.

### 7. Observability (chosen: keeper trace events)

**Recommended:** under `HERDR_MOSH_TRACE`, the keeper emits `window` events (`window`, `base_rtt`,
`epoch_min`, `server_backlog`) once per epoch. Tests read these through the harness's existing
`keeper_events`. No new environment variables.

### 8. Constraints held

- All code is under `src/mosh/` (the new `window.rs` is mounted in `src/mosh/mod.rs`), plus
  `tests/mosh_*.rs`.
- No upstream file is edited: the diff budget stays at 12 lines.
- No change to the herdr wire, `PROTOCOL_VERSION` or hello fields. No change to the session
  protocol (`SESSION_PROTOCOL_VERSION` stays 1; the receiver is untouched).
- No new Cargo dependency (`libc` is already a dependency for `FIONREAD`).
- Conventional commits without AI co-author lines.

### 9. Out of scope

- Server-side pacing (D3 fallback; see §6).
- Frame-aware cancel-on-supersede (Phase 3b).
- Loss emulation, QUIC (Phase 3a).
- Prediction (Phase 4).

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
