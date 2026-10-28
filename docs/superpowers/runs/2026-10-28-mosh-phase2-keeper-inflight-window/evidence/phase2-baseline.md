# Phase 2 baseline: fixed 2 MiB window (mosh-hex.3)

Headline: with the spec's probe (`yes` in a pane) the server's flood output is about 3 KB/s at every
terminal size, because `yes` redraws an almost uniform screen and herdr sends only the diff. No
size reaches the 512 KiB/s needed for R = 256 KiB/s, so the spec's R rule has no solution, and
the baseline Ctrl-C latency at 150 ms one way is about 0.31 s, not above 1.5 s. The probe cannot
discriminate the fixed window from the adaptive one. The R1-miss path in mosh-hex.5 applies and a
D3 entry is pre-filled below. A non-uniform flood (`base64 /dev/urandom`) was also measured for
reference and still tops out at 508 KiB/s at 400x100.

## Header

- Measured commit: `b9ea542dcee97413d0dfa460da4c3c150e954720` (integration tip before mosh-hex.4;
  `src/mosh/session.rs:703` still uses `room = WINDOW - in_flight`, `WINDOW = 2 MiB`, line 32;
  the `InflightWindow` controller from mosh-hex.2 exists but is not wired in).
- Date: 2026-10-28. Host: `Linux blacky 7.0.12-201.fc44.x86_64 #1 SMP PREEMPT_DYNAMIC Thu Jun 11 01:30:16 UTC 2026 x86_64 GNU/Linux`
- `/proc/sys/net/core/wmem_default` = 212992, `wmem_max` = 4194304.
- All tests run with `TMPDIR=/tmp/sp-fd54fa48/mosh-hex.3 PATH=$HOME/.local/bin:$PATH`; helpers and
  the measurement tests live in `tests/mosh_pacing.rs`.

## Probe check (before any timing)

Command: `cargo nextest run --locked --test mosh_pacing flood_probe`

```
PASS [   0.105s] herdr::mosh_pacing flood_probe_prints_the_marker_after_ctrl_c_under_interactive_sh
Summary [   0.105s] 1 test run: 1 passed, 6 skipped
```

The exact line `(trap 'printf "%s%s\n" MA RKER; exit' INT; yes)` runs under `/bin/sh -i` in a pty,
floods more than 64 KiB, and prints `MARKER` after `\x03`; the echoed typed line does not contain
`MARKER`. The test stays in CI (not ignored).

## (a) and (a2) LAN flood rate and ring occupancy, fixed window

Command: `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture baseline_lan`
(resumable session, no link knob, 10 s window after 2 s settle; client-side output bytes/s)

```
size (80, 24):   flood 3028 B/s; keeper leg Send-Q max 0   p95 0
size (120, 40):  flood 3158 B/s; keeper leg Send-Q max 768 p95 0
size (200, 60):  flood 3101 B/s; keeper leg Send-Q max 768 p95 0
```

The flood is about 3 KB/s regardless of size (this is also the same-run LAN flood throughput with
the fixed window; mosh-hex.5 test (2) still uses a same-run plain-bridge reference). Cause: the
client output tail is a stream of cursor moves and single `y` cells at row 24; scrolling `yes`
output leaves the visible screen unchanged, so there is almost nothing to diff. Plain bridge
(`HERDR_MOSH_RESUME=0`) measured the same, 2885 B/s at 80x24, so it is not a window effect.

(a2) LAN max ring occupancy, fixed window: measured lower bound = keeper leg Send-Q (`ss -xpn`,
100 ms samples) max 768 bytes, p95 0. Method limit: Send-Q sees only bytes the attacher has not
yet read from the leg socket, not bytes the keeper holds unacked in its ring. Analytic upper bound
from `may_read_source` (`session.rs:634-648`): `WINDOW + MAX_DATA_CHUNK` = 2 MiB + 64 KiB =
2,162,688 bytes. The actual occupancy under a 3 KB/s source is bounded by the source rate times
the ack delay, i.e. a few KB.

Reference flood, not the spec probe (`base64 /dev/urandom` in the pane, 5 s window, LAN):

```
80x24:    76875 B/s
200x60:  318304 B/s
300x80:  422475 B/s
400x100: 520351 B/s   (508 KiB/s, just under 2 * 256 KiB/s = 512 KiB/s)
```

## (b) Chosen profile

Rule (spec section 5): R = the largest of 256 KiB/s and 1 MiB/s that is at most half the measured
flood rate, at the smallest size from 80x24 up that admits 256 KiB/s.

Result: no size admits 256 KiB/s with the spec probe (max measured 3158 B/s, needs >= 524288 B/s)
and even the best non-uniform flood at 400x100 falls just short. The rule has no solution. The
constants in `tests/mosh_pacing.rs` are therefore provisional and mosh-hex.5 must not treat them
as rule-derived:

- `ONE_WAY_DELAY_MS = 150`
- `PROFILE_RATE = 262_144` (256 KiB/s, the lowest supported rate; provisional)
- `PROFILE_SIZE = (80, 24)` (provisional; larger sizes do not help with `yes`)

## (c) Ctrl-C latency at the profile (fixed window)

Command: `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture baseline_ctrl_c`
(link 150 ms one way, 256 KiB/s, 80x24, 2 s settled flood, `\x03` into the client pty, stopwatch to
`MARKER` on the client screen)

```
run 1: ctrl-c 325 ms
run 2: ctrl-c 311 ms
run 3: ctrl-c 309 ms
```

Baseline is about 0.31 s, which is at most 1.5 s, so the acceptance test cannot discriminate: the
latency is pure RTT (300 ms) because there is no queued output to drain (flood about 3 KB/s,
far below R). The R rule's lower alternative is already 256 KiB/s (the lowest supported rate), so
no lower R exists within the rule. The only way to make the baseline exceed 1.5 s is a flood that
actually saturates the link, which the spec probe does not produce.

## (d) Server-socket backlog during the flood

Same runs as (c), sampled every 100 ms with `ss -xpn`:

```
run 1: server backlog p50 0 p95 0 max 0   bytes (218 samples)
run 2: server backlog p50 0 p95 0 max 0   bytes (219 samples)
run 3: server backlog p50 0 p95 0 max 104 bytes (223 samples)
```

Backlog metric definition (mosh-hex.4's `server_backlog` trace must measure exactly this): bytes
queued in the receive queue of the keeper's server-side Unix stream socket, that is `ss` Recv-Q
of that socket, equal to FIONREAD on that stream. Helper: `keeper_server_backlog` in
`tests/mosh_pacing.rs` (the `ESTAB` row of a keeper pid whose local address is `*`).

Sample rows (`ss -xpn`, columns Netid State Recv-Q Send-Q Local Inode Peer Inode Process), keeper
pid 3714205, server pid 3714108:

```
u_str ESTAB 0 0 /tmp/hm3714106-0/remote/tmp/herdr-mosh-1001/k-85e3....sock 1774544974 * 1774545948 users:(("herdr",pid=3714205,...))   # leg socket (named: rendezvous path)
u_str ESTAB 0 0 *                                                          1774545949 * 1774535048 users:(("herdr",pid=3714205,...))   # keeper server stream (read by the metric)
u_str ESTAB 0 0 /tmp/hm3714106-0/remote/runtime/herdr-client.sock          1774535048 * 1774545949 users:(("herdr",pid=3714108,...))   # server side of the same stream
```

The keeper's `*` row has peer inode 1774535048, which is the local inode of the server's
`herdr-client.sock` accepted socket (pid 3714108): it is the server stream.

Userland queue beyond the socket: yes, see the next section. The Recv-Q metric does not see it.

## FIONREAD / Recv-Q cap

On Linux a Unix stream socket's queued bytes are charged to the sender's send buffer, so Recv-Q
(FIONREAD) on the keeper side saturates near the server socket's `SO_SNDBUF` (`wmem_default` =
212992 here). A reading at or near 212992 means "at least this much": the true backlog may be
larger, held in the server's userland frame or in its blocked write. A D3 staleness computed from
a saturated reading is only a lower bound, so a saturated trace cannot confirm a small backlog and
cannot size a pacing fix; it can only show that a backlog of at least the cap exists.

## Server slow-client behaviour (client stops reading)

Source: `src/server/client_transport.rs` unless noted.

- Render lane capacity is one frame: `render: Option<Vec<u8>>` (line 277), documented "Capacity is
  one so slow clients cannot build lag" (line 125). `try_send_render` returns `Full` while a frame
  is pending (lines 320-327).
- On `Full` the server coalesces: the caller keeps the client dirty with `defer_full_render()`
  (`src/server/headless/render.rs:964-985`, setter at `src/server/clients.rs:457`), and re-renders a
  fresh frame when the writer claims the pending one (`ClientWriterDrained`, sent at
  `client_transport.rs:958-960`, handled at `src/server/headless.rs:2651-2656`). No frame history
  accumulates.
- `ordered` lane: at most one older render plus one direct frame (`send_ordered`, lines 340-351);
  a second `send_ordered` while non-empty is refused `Full`.
- `control` lane is an unbounded `VecDeque` (line 275, `send_control` lines 310-318): reliable
  control messages (shutdown, notifications, clipboard) queue without a cap. Output frames do
  not use it.
- The writer thread (`client_writer_loop`, lines 951-973) blocks in `write_all` on the stream; for
  an ordinary client there is no write timeout (`src/platform/unix_common.rs:140-141`,
  `socket.write_timeout()` is `None`, so plain `write_all`). Only terminal observers get
  `OBSERVER_WRITE_TIMEOUT` = 30 s (lines 44-45, 1078), after which the stream is shut down. A
  stalled ordinary client is never disconnected; it stays blocked with one frame in flight.
- Consequence: the server holds at most one rendered frame (plus the in-progress write) in
  userland beyond the socket. The FIONREAD metric does not count that frame (nor the blocked
  `write_all` remainder), so a saturated Recv-Q understates the true backlog by up to one full
  frame (a full-screen redraw at a large terminal, tens of KB at most here). The latency
  attribution must add this frame to the socket backlog.

## Latency budget prediction

Budget = 0.15 s up + window/R + backlog/R + 0.15 s down (the window occupies the pipe, so
propagation is not counted twice). Adaptive window = `max(FLOOR, 2*R*0.3 s)`, FLOOR = 128 KiB.

| R | window | window/R | backlog (measured p95) | total, backlog 0 | total, backlog at the 212992 cap |
|---|---|---|---|---|---|
| 256 KiB/s | 157,286 (2*R*0.3; FLOOR alone 131,072 = 0.50 s) | 0.60 s | 0 | 0.90 s | 1.71 s |
| 1 MiB/s | 629,146 | 0.60 s | 0 | 0.90 s | 1.10 s |

- At 256 KiB/s the prediction with the measured backlog (0) is 0.90 s, which fits 1.5 s. With a
  saturated backlog (212992 bytes = 0.81 s at 256 KiB/s) it is 1.71 s, which does not fit. The
  budget closes whenever backlog <= 0.6 s * R = 157 KB at 256 KiB/s. Lowest R where the cap
  case closes: 0.9 + 212992/R <= 1.5 gives R >= 354,987 B/s (347 KiB/s), i.e. 1 MiB/s from the
  rule's set.
- The measured backlog (0) is from a probe that does not flood, so it says nothing about the
  backlog under a saturating flood. The supported bottom is therefore claimed at 256 KiB/s only if
  the server backlog under a saturating flood stays below about 157 KB; otherwise it moves up to
  1 MiB/s.
- Fixed window for contrast: 2 MiB / 256 KiB/s = 8 s drain, 2 MiB / 1 MiB/s = 2 s, both well above
  1.5 s if the link were saturated. The baseline did not show this only because the spec probe
  never fills the window.

Does the chosen profile fit 1.5 s with the adaptive window? On paper yes (0.90 s), but no profile
within the spec's R rule exists and the baseline did not exceed 1.5 s, so the spec's acceptance
test as written (fail with the fixed window, pass with the adaptive one) cannot be run with this
probe at this profile.

## D3 decision (pre-filled; R1-miss path in mosh-hex.5 applies)

- Finding: spec probe flood = about 3 KB/s at 80x24 to 200x60 (a uniform scrolling screen produces
  tiny diffs); with `base64 /dev/urandom` it is 77 KB/s at 80x24 and 508 KiB/s at 400x100.
  Required for R = 256 KiB/s: >= 512 KiB/s.
- Baseline Ctrl-C at 150 ms one way, 256 KiB/s: 309-325 ms (3 runs), server backlog p95 0, max 104
  bytes: no queue forms, so there is nothing for the window to fix under this probe.
- Decision input for the plan owner: either (1) change the flood to a content-dense stream (the
  probe line is spec-fixed here, so that is a spec amendment) and a terminal size near 400x100,
  accepting that it just misses 2x256 KiB/s, or R lower than the supported bottom; or (2) record
  that on herdr's own render path the flood never exceeds the link, so the Phase 1 fixed window
  is not the bottleneck in this harness and D3 (no pacing built) stands with this evidence.
  This leaf does not choose.

## Findings carried to the plan

1. `FLOOD_PROBE` cannot produce a saturating flood in herdr; mosh-hex.5 as specified cannot fail
   at baseline.
2. `PROFILE_RATE` and `PROFILE_SIZE` are provisional.
3. Server userland holds at most one frame beyond the socket; Recv-Q caps at 212992.
