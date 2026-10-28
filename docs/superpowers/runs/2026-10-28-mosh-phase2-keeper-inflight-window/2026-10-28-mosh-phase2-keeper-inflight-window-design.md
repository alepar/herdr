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
`SESSION_PROTOCOL_VERSION` does not change. The window floor is two ack quanta (128 KiB). The
1.5 s goal is claimed for link rates `R >= 256 KiB/s`; below that the floor alone is about 0.5 s
or more of queue and the goal is not claimed (§5). Link
emulation is a delay-and-rate relay the fake `ssh` inserts when a harness knob is set, written in
Perl like the harness's existing throttle. D3 is applied as written: the server-side backlog is
measured and recorded; server pacing is not built in this run. If the measurement shows the
backlog is material, it is recorded as a parked follow-up with numbers, because pacing needs an
RTT signal inside the server and upstream edits beyond the hook budget.

## Decision points, by section

### 1. Window controller: delay-based, sender-side (chosen)

**Recommended:** a new `InflightWindow` type in `src/mosh/window.rs`, owned by each
`SessionEndpoint`. Its bookkeeping is in bytes and offsets, not per chunk. The send record maps
offsets to first-transmission instants, coalesced to at most one record per `ACK_EVERY_BYTES`
boundary of offset: a fresh send whose end offset falls in the same 64 KiB quantum as the newest
record updates nothing (the record keeps the earlier instant, so a sample never under-reads), and
one that crosses a boundary adds a record. The record is therefore bounded by bytes in flight
(at most `CAP / ACK_EVERY_BYTES` + 1 entries) whatever the chunk sizes; at the bound records are
coalesced, never dropped. It takes an RTT sample on each ACK that advances the acked offset by at
least `ACK_EVERY_BYTES`. Such an ACK was triggered by byte count, not by the 200 ms timer, so the
sample is not inflated by the receiver's ack delay. The sample is `now - first_send(byte at
ack-1)`, read from the record covering that byte. Timer-only ACKs (advance below
`ACK_EVERY_BYTES`) are not sampled. Replayed chunks are never sampled (Karn's rule): the record
is cleared when a leg goes up or down. State: `base_rtt`, the minimum sample on the current leg;
`epoch_min`, the minimum sample in the current epoch (none until one arrives); `window`; and the
epoch boundary, a byte offset (the send offset when the epoch began), not a record entry. Once
per epoch (an epoch ends when an ACK covers the boundary offset, about one RTT):

- if the epoch has no qualifying sample: hold the window (no grow, no shrink);
- queueing delay `q = epoch_min - base_rtt`;
- if `q > TARGET_QUEUE_DELAY` (50 ms): `window = max(FLOOR, window * 3 / 4)`;
- otherwise, if the sender was window-limited during the epoch (it had bytes to send and no
  room): `window = min(CAP, window + MAX_DATA_CHUNK)`;
- otherwise unchanged. An idle link never grows the window, so a sudden flood after idle starts
  from a small window, not the cap.

After any window change, the next epoch is a hold epoch: its samples were sent under the old
window, so it updates the epoch bookkeeping and `base_rtt` but makes no grow or shrink decision.
Every decision therefore judges bytes sent under the current window.

Constants: `FLOOR = 2 * ACK_EVERY_BYTES` (128 KiB), `INITIAL = 256 KiB`, `CAP = WINDOW` (2 MiB),
`TARGET_QUEUE_DELAY = 50 ms`. A new leg resets the controller to `INITIAL` with no samples,
because a reconnect may be a different path.

Sampling rate: a sample needs 64 KiB acked within one 200 ms ack timer, so below about
320 KiB/s, or with traffic in sub-64 KiB bursts, the controller may sample rarely and then
holds. Under the acceptance profile's sustained flood the keeper reads full 64 KiB chunks
(`source_room()` is at least one chunk whenever the window has a chunk of room), so each
delivered chunk triggers a byte-count ACK and epochs sample.

`base_rtt` is per leg and does not age: a same-leg path RTT increase above 50 ms reads as
queueing and pins the window at `FLOOR` until the next leg (a reconnect re-learns it). This
tradeoff is accepted; rolling minima (LEDBAT's base-delay history) are deferred.

**Considered:**
- A fixed smaller window (for example 256 KiB). Simplest, but it cannot track both ends of the
  range: 256 KiB is about 1 s of queue at 256 KiB/s and 0.25 s at 1 MiB/s, and too small for a
  fast long-haul link. The adaptive window's own minimum is bounded by `FLOOR` (about 0.5 s of
  queue at 256 KiB/s, the bottom of the supported range, §5). Rejected: does not adapt.
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
fake `ssh`. Each chunk read at time `t_arrival` departs at
`depart = max(t_arrival, prev_depart) + len / rate` and is delivered at `depart + delay`, so every
chunk pays serialization, idle line or not, and the line models a serializing bottleneck plus
propagation delay. It is single
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

- **Ctrl-C under flood:** link `150 ms` one way and rate `R`. The pane command must print the
  marker after SIGINT and must not contain the marker literally: an interactive `sh` aborts the
  rest of a command list when its foreground child dies of SIGINT, so `yes; echo <marker>` never
  prints it, and the echoed command line would match a literal marker. A job-control shell also
  delivers SIGINT only to the foreground job, so a shell-level `INT` trap never fires. The pane
  runs `(trap 'printf "%s%s\n" MA RKER; exit' INT; yes)`: the trap lives inside the foreground
  job, `yes` writes to the terminal so the flood is on screen, and the marker is built by
  expansion, so the typed line never contains it. The test waits until the
  client output has grown for 2 s of flood. It writes `\x03` into the client pty and measures the
  time until the client screen shows the marker. The assertion is `<= 1.5 s` (5 RTT).
- **No throughput regression:** with no link knob (LAN), the same test runs the same flood twice:
  once over the plain bridge (`HERDR_MOSH_RESUME=0`, the existing Phase 1 fallback, no window at
  all) and once with the resumable session. The assertion is one-sided: adaptive delivered
  bytes/s `>= 0.9 *` plain-bridge delivered bytes/s, both measured client-side in the same run.
  (Window size is not asserted: on a sub-ms path with a source-bound flood the sender is rarely
  window-limited, so the window need not grow.)
- **Window converges:** under the slow profile with a 20 s flood, the median of the keeper's
  traced window over the last 5 s is at most `max(FLOOR, 2 * R * 0.3 s)` plus one chunk. This is
  an upper bound only; a window pinned at `FLOOR` also satisfies it. Lower-bound sanity: the
  traced window is never below `FLOOR`, and the measured goodput under the flood is at least
  `0.8 * R`.

`R` is fixed by the measurement leaf (§6) as the largest of 256 KiB/s and 1 MiB/s that is at most
half the server's measured flood output rate at the test terminal size. The terminal size may be
raised from 80x24 to reach a usable flood rate. With the Phase 1 fixed window the same test must
fail the 1.5 s bound. That baseline is measured once and recorded as evidence, not kept as a CI
test.

Supported range: the 1.5 s goal is claimed for `R >= 256 KiB/s`. Below that, `FLOOR` (128 KiB)
alone is about 0.5 s or more of queue, before the server-socket backlog, and the goal is not
claimed; no acceptance test runs there.

Latency budget for the marker: `0.15 s` upstream (the `\x03`) + in-flight window drain
(`window / R`) + server-socket backlog `/ R` + `0.15 s` downstream. The in-flight window already
occupies the pipe, so downstream propagation is not counted a second time on top of the drain.
The 1.5 s claim is verified only at the `R` the acceptance test runs (chosen by the measurement
leaf, §6). The measurement leaf's predicted budget decides whether the 256 KiB/s bottom closes:
if its prediction for 256 KiB/s exceeds 1.5 s, the evidence file says so and the supported
bottom moves up to the lowest `R` where the budget closes.

**Considered:** a CI test that runs both the fixed and the adaptive window. It would need a
production knob to force the fixed window. Rejected: configurability nobody needs. The plain
bridge (`HERDR_MOSH_RESUME=0`, no window) is the same-run throughput reference instead, and needs
no new knob.

### 6. Measurement and D3 (chosen: measure, record, do not build pacing)

**Recommended:** a measurement leaf runs first on the link harness against the current code
(fixed window). It records in the run's `evidence/`:

- (a) the server's flood output rate at the test terminal size (the LAN flood throughput with the
  fixed window; evidence only, the §5 parity test uses the same-run plain-bridge reference), and
  the LAN max ring occupancy with the fixed window;
- (b) the Ctrl-C latency at the chosen profile (the baseline), using the §5 probe line, which is
  first verified under `sh -i` in a pty to print the marker after `\x03`;
- (c) the server-socket backlog. In the baseline the keeper trace does not exist yet, so it is
  sampled externally (the `Recv-Q` of the keeper's server-side Unix socket, via `ss -xp`). Once
  the window lands, the keeper traces `FIONREAD` on its server stream once per window epoch, as
  `server_backlog` bytes.

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
- `base_rtt` aging (rolling minima); a same-leg RTT increase pins the window at `FLOOR` until the
  next leg (§1).

## Key abstractions

**InflightWindow** — new struct (`src/mosh/window.rs`), the sender-side delay-based (Vegas/LEDBAT-style) in-flight window controller; a sans-IO state machine.
- Owns: the current window value and the constants FLOOR, INITIAL, CAP and TARGET_QUEUE_DELAY; the send-time record of first transmissions of fresh DATA, kept in bytes/offsets and coalesced to at most one record per `ACK_EVERY_BYTES` boundary of offset (bounded by bytes in flight; coalesced, never dropped, at the bound); RTT sampling, which only accepts ACKs that advance the acked offset by at least `ACK_EVERY_BYTES` and never samples replayed bytes (Karn's rule); `base_rtt` and `epoch_min`; epoch boundaries, tracked as a byte offset (an epoch ends at the first ACK that covers the first byte sent after the epoch began); an epoch with no qualifying sample holds the window; the epoch after any window change is a hold epoch (updates bookkeeping and `base_rtt`, no grow/shrink decision), so every decision judges bytes sent under the current window; the window-limited flag for the epoch; the decrease/grow/hold rule; reset to INITIAL with no samples. When an epoch closes it produces a snapshot (window, base_rtt, epoch_min) that its owner can take.
- Hides: the control law and its tuning. No other code compares RTTs, knows the 50 ms target or knows the 3/4 and +1-chunk steps.
- Must not: touch the ring, offsets beyond those it is told about, sockets, the clock (time is an input) or trace output. It must not know which side it runs on. The keeper does the tracing and the FIONREAD read. `SessionEndpoint` decides when a leg is up.
- Relationships: composed by `SessionEndpoint`, one per endpoint, so the keeper and the local end each have one. It depends on nothing fork-side except the `session.rs` constants it derives from (`WINDOW`, `ACK_EVERY_BYTES`, `MAX_DATA_CHUNK`). No trait: only one controller exists, and the spec names no second one (§1 rejects BBR and fixed windows instead of keeping them as alternatives).
- Lifecycle: built with the endpoint and lives for the whole session. `SessionEndpoint` resets it on leg up and leg down. It is single-owner and moves with the endpoint under the keeper's table lock. Nothing needs releasing.
- Errors: none. Implausible inputs are ignored, never reported: an ACK below the oldest record, an empty send record, a sample before any send. An ACK beyond the sent bytes is already a leg protocol error raised by `SessionEndpoint` before the controller sees it.

**SessionEndpoint** — extended: it now owns an `InflightWindow` and is the only place that feeds it. `poll_send` reports fresh (non-replay) DATA sends. The ACK path reports the new acked offset. `set_up` and `leg_down` reset the controller. `may_read_source`, `poll_send`'s room calculation and the new `source_room()` use the controller's current window while a leg is up, and the ring cap while down. `WINDOW` is now only the hard cap, and the `push_source` assertion is restated against it. It passes the epoch snapshot on to its owner unchanged.

**Keeper** — extended: `server_reader` sizes each read with `source_room()`, rounded up to 4 KiB. When an epoch snapshot is available, the keeper reads `FIONREAD` on that session's server stream and emits one `window` trace event (`window`, `base_rtt`, `epoch_min`, `server_backlog`). Both the backlog measurement and the trace stay in the keeper.

**LocalBridge** — extended: the client reader sizes reads with `source_room()`, the same as the keeper (§3, for symmetry). It does not emit trace events.

**Fake-ssh harness (`tests/mosh_harness.rs`)** — extended: a `link` knob next to `throttle`, the Perl delay-line script the harness writes beside the fake `ssh`, and `Remote::set_link` / `clear_link`. These are test-only knob helpers, not a new abstraction.

Variation: the control law and its constants are the only thing likely to vary (retuning, or the Phase 3a QUIC leg), and they all live in `InflightWindow`. The kind of leg (ssh now, QUIC later) does not reach the controller, because it sees only send and ACK events from `SessionEndpoint`. There is no registry: one controller, constructed in one place, the endpoint constructor.
Walk-through: the keeper reads up to `source_room()` from the server and calls `push_source`. `poll_send` emits a DATA chunk and records its send time in `InflightWindow`. The local side delivers and ACKs every 64 KiB. `on_leg_bytes` passes the ACK to the controller, which samples the RTT. When the epoch closes, the controller shrinks the window (q > 50 ms) or grows it (window-limited). The keeper takes the snapshot, adds FIONREAD and traces it. A smaller `source_room()` then stops the keeper draining the server socket, so the server's latest-wins slot holds the backlog. A future change to a different law, or to tuning after Phase 3a QUIC, touches only `window.rs`. No caller changes, because callers see only "current window", "room" and "epoch snapshot".
Gap settled here (fills §2, does not override it): with §3's reads sized to the window, ring occupancy never exceeds the window, so `poll_send` never stops for lack of room on its own and the window-limited signal would never fire. `SessionEndpoint` also marks the epoch window-limited when the source was refused room (occupancy reached the window); that decision lives in the endpoint and is passed to the controller, never inferred in the keeper or the bridge.
Per spec §2: the upstream direction (client input) also runs the controller, at INITIAL. This is deliberate symmetry, not a missing per-side branch. Do not add a role check to turn it off.
Per spec §5: there is deliberately no production switch between the fixed and the adaptive window. The fixed-window baseline is measured once on the pre-change code.

Digest:
- InflightWindow: owns the in-flight window value, RTT sampling (Karn, ack-quantum gating), epochs and the delay-based grow/shrink law;
  new control behaviour and constants are added in `src/mosh/window.rs` only;
  callers never compare RTTs, apply window arithmetic or emit traces from it; they feed send/ACK/reset events through `SessionEndpoint` and read the current window or epoch snapshot.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*

- 2026-10-28 (execution, mosh-hex.3/.5): herdr sends screen diffs, so the `yes` probe produces about 3 KB/s regardless of terminal size and cannot load the link; the fixed window answers Ctrl-C in 0.31 s with it, the same as the adaptive one. With a dense flood (`base64 /dev/urandom` at 400x100, about 508 KiB/s) on 150 ms one way / 256 KiB/s, Ctrl-C took 8.7-9.2 s with the fixed 2 MiB window and 1.54-1.86 s with the adaptive window converged at FLOOR. The residual is server-socket backlog at the FIONREAD cap (p95 about 214.5 KB, at least 0.82 s of staleness), so D3's evidence bar is met. Orchestrator decision (autonomous run): the acceptance uses the dense flood, asserts <= 2.5 s and <= 0.35 x the fixed-window latency, and goodput >= 0.6 R; the 1.5 s goal and the 0.8 R goodput floor are recorded as unmet agent checks; server pacing / per-client SO_SNDBUF and goodput at FLOOR are parked follow-ups for the human, not built in Phase 2.
