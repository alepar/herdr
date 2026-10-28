# Phase 2 acceptance: adaptive in-flight window (mosh-hex.5, amended)

Headline: on the recut base (integration tip `7ab9be9e`, which includes mosh-hex.4 as merged and mosh-hex.9's outage source-read cap), the whole `mosh_pacing` file (tests (1), (2), (3), resume under flood, the `yes` probe and the link self-tests) passes in 5 of 5 consecutive runs.
The spec's 1.5 s Ctrl-C goal is an unmet agent check (median 1.49-2.06 s; met in 1 of 5 runs). The 0.8 R goodput goal is met in 2 of 5 runs (agent check). Server pacing is the measured follow-up (D3), parked, not built.
Every number below was measured on the recut base; none is carried over from the earlier attempts except the fixed-window figure (see its section).

## Header

- Measured source: branch `task-mosh-hex.5` = integration tip `7ab9be9e` + this task's two test/evidence commits (the earlier attempts' commits `357680f9` and `6a62a988`, replayed by cherry-pick; kept at tag `mosh-hex.5-attempt2`).
- Date 2026-10-28. Host `Linux blacky 7.0.12-201.fc44.x86_64`, quiet host. All runs under `TMPDIR=/tmp/sp-fd54fa48/mosh-hex.5`.
- Profile: `ONE_WAY_DELAY_MS = 150`, `PROFILE_RATE = 262144` (256 KiB/s), dense flood size 400x100.
- Flood: `(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)`, about 508 KiB/s on LAN (about 1.98 x R), so the link saturates.
  The `yes` probe (about 3 KB/s) is kept only for `flood_probe_prints_the_marker_after_ctrl_c_under_interactive_sh`.
- Window constants mirrored in the test file: `FLOOR` 128 KiB, `INITIAL` 256 KiB, `CHUNK` 64 KiB; test (3) bound = `max(FLOOR, 2*R*0.3 s) + CHUNK` = 222,822 B.
- Command (5 times, back to back): `cargo nextest run --locked --test mosh_pacing --no-capture --no-fail-fast`; each run `9 tests run: 9 passed, 3 skipped` in 108-110 s.

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
| 1 | 1599, 1490, 1330 | 1489 | 214,156 / 230,650 / 215,658 B (0.82-0.88 s) | pass |
| 2 | 1583, 1745, 1869 | 1744 | 214,165-214,732 B (0.82 s) | pass |
| 3 | 1627, 1629, 2684 | 1628 | 214,336-215,418 B (0.82 s) | pass |
| 4 | 1570, 2144, 2004 | 2004 | 213,619-215,233 B (0.82 s) | pass |
| 5 | 1598, 2061, 2402 | 2061 | 213,788-214,904 B (0.82 s) | pass |

Before/after: median 9182 ms fixed vs 1489-2061 ms adaptive (4.5x-6.2x). **Agent check, unmet:** the spec's 1.5 s goal is exceeded by the median in 4 of 5 runs (1489 ms in run 1 is the only median under it). The spec text is not edited.

## Test (2): LAN parity, dense flood 400x100, no link knob (2 rounds each, means)

| test run | plain bridge (B/s) | adaptive (B/s) | ratio | result |
|---|---|---|---|---|
| 1 | 546,976 | 541,056 | 0.99 | pass |
| 2 | 540,120 | 546,973 | 1.01 | pass |
| 3 | 540,165 | 530,630 | 0.98 | pass |
| 4 | 539,337 | 541,261 | 1.00 | pass |
| 5 | 530,233 | 536,050 | 1.01 | pass |

## Test (3): convergence under the dense flood (20 s, median over the last 5 s; goodput over the last 10 s at the downstream delay line)

| test run | median window (B) | bound | goodput (B/s) | goodput / R | result |
|---|---|---|---|---|---|
| 1 | 131,072 | 222,822 | 182,344 | 0.70 | pass |
| 2 | 147,456 | 222,822 | 234,937 | 0.90 | pass |
| 3 | 131,072 | 222,822 | 189,823 | 0.72 | pass |
| 4 | 131,072 | 222,822 | 207,112 | 0.79 | pass |
| 5 | 131,072 | 222,822 | 212,086 | 0.81 | pass |

The window is never below `FLOOR` and the first traced window is `INITIAL` in every run (asserted by the test). Asserted goodput is >= 0.6 R (157,286). **Agent check:** the spec's 0.8 R (209,715) is met in 2 of 5 runs (runs 2 and 5) and missed in runs 1, 3, 4 (0.70-0.79 R).

## Resume under flood: pass

`a_leg_killed_mid_dense_flood_resumes_at_initial_and_ctrl_c_still_answers_in_time`: the leg is killed mid-flood, a second leg comes up, a `window` event with `window = INITIAL` (262144) appears on the new leg, then Ctrl-C is answered within the amended 2,500 ms bound in all 5 runs: **1951, 1605, 2017, 1779, 1822 ms**.
This was 15,808 ms and 16,727 ms on the earlier base (hex.4 before review, without mosh-hex.9): the keeper filled the ring with stale flood while the leg was down. mosh-hex.9's outage source-read cap removed it; Task 5 made no production change.

## Server backlog and staleness (D3)

Backlog metric: Recv-Q of the keeper's server stream (FIONREAD); the keeper `window` trace and external `ss -xpn` sampling agree (before: ss only, p95 214,515-231,321 B).

Adaptive, trace `server_backlog` per Ctrl-C run above: p95 213,619-230,650 B, typically about 214,500 B = **0.82 s** staleness at R (max 230,650 B = 0.88 s). This exceeds the 250 ms threshold.

FIONREAD cap: Recv-Q saturates near the server socket buffer (`wmem_default` 212,992; readings up to about 232 KB). The p95 sits at the cap, so 0.82 s is a lower bound; the server also holds up to one rendered frame in userland (`phase2-baseline.md`, slow-client section) that Recv-Q does not count.

Latency attribution for the residual above 1.5 s: link propagation 0.30 s + window drain at `FLOOR` / R = 0.50 s + server-socket backlog 0.6-0.82 s = 1.4-1.6 s, matching the measured 1.3-2.7 s per-run range. The window is at or near `FLOOR` and cannot shrink further; the remaining latency is queued bytes in the server socket that the keeper does not control.

## D3 decision

D3's evidence bar for server pacing is met: `server_backlog` p95 about 214,500 B (0.82 s at R = 256 KiB/s), at the FIONREAD cap. **Follow-ups, parked for the human, not built here:** (a) server pacing / per-client `SO_SNDBUF`, which closes the 1.5 s gap; (b) goodput at `FLOOR` below 0.8 R (0.70-0.72 R in runs 1 and 3 although `FLOOR` exceeds the 78 KiB BDP). The orchestrator parked both in `run.md`; no new beads filed.

## Protocol check

- `git diff 1589edbd -- src/mosh/session.rs | grep -E '^[-+].*(enum SessionMsg|fn encode|fn next|SESSION_PROTOCOL_VERSION|ACK_EVERY|const MAX_DATA_CHUNK|const WINDOW)' | wc -l`: `0`.
- `cargo nextest run --locked --bin herdr -E 'test(codec_round_trips) | test(wire_layout)'`: `3 tests run: 3 passed`; neither mosh test is in the diff, so expectations are unmodified.

## Closing check

- `git diff --name-only 1589edbd` filtered to exclude `src/mosh/`, `tests/mosh_` and the run directory: empty.
- `bash scripts/mosh-diff-budget.sh`: `12 inserted upstream lines (budget 20), 0 untagged hunks`.
- `git diff 1589edbd -- Cargo.toml Cargo.lock | wc -l`: `0`.
- `cargo fmt --check`: clean.

## Flakiness (5 consecutive runs)

The full `mosh_pacing` file (9 tests) passed in 5 of 5 consecutive runs on the recut base; every row above is from those runs.
