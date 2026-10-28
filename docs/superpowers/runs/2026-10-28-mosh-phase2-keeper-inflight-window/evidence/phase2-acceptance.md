# Phase 2 acceptance: adaptive in-flight window (mosh-hex.5, amended)

Headline: with the amended dense-flood probe, tests (1), (2) and (3) pass 5 of 5 consecutive runs;
the resume-under-flood test fails deterministically (15.8 s and 16.7 s against the 2.5 s bound).
The spec's 1.5 s Ctrl-C goal is an unmet agent check (median 1.6-2.1 s). Server pacing is the
measured follow-up (D3), parked, not built.

## Header

- Measured source: branch `task-mosh-hex.5` = integration tip `eb8583cf` + mosh-hex.4 (merge `e8955401`) + this task's tests.
- Date 2026-10-28. Host `Linux blacky 7.0.12-201.fc44.x86_64`, quiet host. All runs under `TMPDIR=/tmp/sp-fd54fa48/mosh-hex.5`.
- Profile: `ONE_WAY_DELAY_MS = 150`, `PROFILE_RATE = 262144` (256 KiB/s), dense flood size 400x100.
- Flood: `(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)`, about 508 KiB/s on LAN (about 1.98 x R), so the link saturates.
  The `yes` probe (about 3 KB/s) is kept only for `flood_probe_prints_the_marker_after_ctrl_c_under_interactive_sh`.
- Window constants mirrored in the test file: `FLOOR` 128 KiB, `INITIAL` 256 KiB, `CHUNK` 64 KiB; test (3) bound = `max(FLOOR, 2*R*0.3 s) + CHUNK` = 222,822 B.
- Command: `cargo nextest run --locked --test mosh_pacing --no-capture --no-fail-fast` (run 1, all tests), then `-E 'not test(a_leg_killed)'` for runs 2-5.

## Fixed-window figure (measured once)

`saturating_flood_evidence` (ignored) at the same profile against source `b9ea542` (src identical to `eb8583cf`; verified with `git diff --stat b9ea542 eb8583cf -- src` empty), test files copied in:

| | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| fixed 2 MiB window, Ctrl-C | 9400 ms | 9182 ms | 8437 ms | **9182 ms** |
| fixed goodput | 262,419 B/s | 260,163 B/s | 262,373 B/s | |
| fixed server backlog (ss) p95 | 214,515 B | 231,049 B | 231,321 B | |

`FIXED_WINDOW_CTRL_C_MS = 9182` in the test; 0.35 x 9182 = 3213 ms, so the 2.5 s bound is the binding one. (The first attempt measured 8733-9195 ms at `eb8583cf`.)

## Test (1): Ctrl-C, adaptive window (3 runs per test run, median asserted <= 2500 ms and <= 0.35 x fixed)

| test run | latencies (ms) | median | server_backlog p95 (trace) | result |
|---|---|---|---|---|
| 1 | 1813, 1894, 1620 | 1813 | 215,913 B (0.82 s) | pass |
| 2 | 1865, 1468, 1610 | 1609 | 215,037-231,286 B (0.82-0.88 s) | pass |
| 3 | 2083, 2578, 2069 | 2082 | 160,445-215,857 B | pass |
| 4 | 1728, 2069, 1843 | 1842 | 160,460-215,664 B | pass |
| 5 | 1542, 1998, 1730 | 1730 | 215,765-215,823 B | pass |

Before/after: median 9182 ms fixed vs 1609-2082 ms adaptive (4.4x-5.7x). **Agent check, unmet:** the spec's 1.5 s goal is exceeded by the median in 5 of 5 runs (1.6-2.1 s). The spec text is not edited.

## Test (2): LAN parity, dense flood 400x100, no link knob (2 rounds each, means)

| test run | plain bridge (B/s) | adaptive (B/s) | ratio | result |
|---|---|---|---|---|
| 1 | 534,057 | 534,415 | 1.00 | pass |
| 2 | 434,366 | 485,669 | 1.12 | pass |
| 3 | 428,830 | 453,898 | 1.06 | pass |
| 4 | 528,375 | 534,159 | 1.01 | pass |
| 5 | 532,229 | 534,012 | 1.00 | pass |

## Test (3): convergence under the dense flood (20 s, median over the last 5 s; goodput over the last 10 s at the downstream delay line)

| test run | median window (B) | bound | goodput (B/s) | goodput / R | result |
|---|---|---|---|---|---|
| 1 | 131,072 | 222,822 | 235,251 | 0.90 | pass |
| 2 | 131,072 | 222,822 | 183,163 | 0.70 | pass |
| 3 | 196,608 | 222,822 | 227,311 | 0.87 | pass |
| 4 | 159,744 | 222,822 | 240,297 | 0.92 | pass |
| 5 | 196,608 | 222,822 | 239,533 | 0.91 | pass |

The window is never below `FLOOR` and the first traced window is `INITIAL` in every run. Asserted goodput is >= 0.6 R (157,286). **Agent check:** the spec's 0.8 R (209,715) is met in 4 of 5 runs and missed in run 2 (0.70 R).

## Resume under flood: FAIL (blocker bead mosh-00q)

`a_leg_killed_mid_dense_flood_resumes_at_initial_and_ctrl_c_still_answers_in_time`: the leg is killed mid-flood, a second leg comes up, a `window` event with `window = INITIAL` (262144) appears on the new leg (this part holds), then Ctrl-C takes **15,808 ms** (run 1) and **16,727 ms** (rerun), bound 2,500 ms. Not a measurement artifact: it is deterministic.

Likely cause (read from `src/mosh/session.rs`, not separately instrumented): while the leg is down `SessionEndpoint::source_room()` uses `ring.capacity()` (mosh-hex.4: "ring cap rule unchanged while down"), so the keeper fills the ring with up to about 2 MiB of stale flood during the outage; after the resume that backlog drains at R (2 MiB / 256 KiB/s is 8 s, plus the data the outage accumulated in the server socket) ahead of the Ctrl-C echo. The window reset to `INITIAL` bounds only what is in flight, not what is queued in the ring. A fix is a production change in mosh-hex.4's code that contradicts its acceptance bullet and unit tests, so it needs an owner decision. Not built here.

## Server backlog and staleness (D3)

Backlog metric: Recv-Q of the keeper's server stream (FIONREAD); the keeper `window` trace and external `ss -xpn` sampling agree (before: ss only, p95 214,515-231,321 B).

Adaptive, trace `server_backlog` per Ctrl-C run above: p95 160,445-231,286 B, typically about 215,800 B = **0.82 s** staleness at R (max reading 231,632 B = 0.88 s). This exceeds the 250 ms threshold.

FIONREAD cap: Recv-Q saturates near the server socket buffer (`wmem_default` 212,992; readings up to about 232 KB). The p95 sits at the cap, so 0.82 s is a lower bound; the server also holds up to one rendered frame in userland (`phase2-baseline.md`, slow-client section) that Recv-Q does not count.

Latency attribution for the residual above 1.5 s: link propagation 0.30 s + window drain at `FLOOR` / R = 0.50 s + server-socket backlog 0.6-0.82 s = 1.4-1.6 s, matching the measured 1.5-2.1 s. The window is at or near `FLOOR` and cannot shrink further; the remaining latency is queued bytes in the server socket that the keeper does not control.

## D3 decision

D3's evidence bar for server pacing is met: `server_backlog` p95 about 215,800 B (0.82 s at R = 256 KiB/s), at the FIONREAD cap. **Follow-ups, parked for the human, not built here:** (a) server pacing / per-client `SO_SNDBUF`, which closes the 1.5 s gap; (b) goodput at `FLOOR` below 0.8 R (run 2: 0.70 R although `FLOOR` exceeds the 78 KiB BDP); (c) the resume-time ring backlog above (mosh-00q). The orchestrator parked (a) and (b) in `run.md`.

## Protocol check

- `git diff 1589edbd -- src/mosh/session.rs | grep -E '^[-+].*(enum SessionMsg|fn encode|fn next|SESSION_PROTOCOL_VERSION|ACK_EVERY|const MAX_DATA_CHUNK|const WINDOW)'`: no output.
- `cargo nextest run --locked --bin herdr -E 'test(codec_round_trips) | test(wire_layout)'`: `3 tests run: 3 passed`; neither mosh test is in the diff, so expectations are unmodified.

## Closing check

- `git diff --stat 1589edbd`: only `src/mosh/**`, `tests/mosh_*` and the run directory (evidence files).
- `bash scripts/mosh-diff-budget.sh`: `12 inserted upstream lines (budget 20), 0 untagged hunks`.
- `git diff 1589edbd -- Cargo.toml Cargo.lock`: empty.
- `cargo clippy --all-targets --locked -- -D warnings` and `cargo fmt --check`: clean.

## Flakiness (5 consecutive runs)

Tests (1), (2), (3) and the `yes`-probe and link self-tests passed in 5 of 5 consecutive runs (table rows above). The resume test failed in both of its runs, so 5 consecutive all-green runs of the file were not achieved.
