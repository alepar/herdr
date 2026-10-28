# Phase 2 acceptance: adaptive in-flight window (mosh-hex.5)

Headline: the acceptance tests as specified cannot be satisfied, and the reason is the flood, not
the window. The spec probe (`yes`) delivers about 3-6 KB/s, so test (3) fails (window never leaves
`INITIAL`, goodput is 2% of R) and test (1) passes without discriminating anything (it also passed
with the fixed window, 0.31 s). A content-dense flood (`base64 /dev/urandom`, 400x100) does
saturate the 256 KiB/s link, and under it the adaptive window cuts Ctrl-C from 8.7-9.2 s (fixed
window) to 1.5-1.9 s, but still misses the 1.5 s bound in 3 of 3 runs with the window converged.
Residual latency is server-socket backlog (the R1-miss path). D3 evidence for server pacing is met.
The bead is reported failed/blocked; no bound was relaxed and no pacing was built.

## Header

- Measured source: branch `task-mosh-hex.5` = integration tip `eb8583cf` plus the mosh-hex.4 branch
  (`af57c1a8`, merge `e8955401`) plus this task's test changes. Before-numbers: `eb8583cf`
  (fixed 2 MiB window, mosh-hex.3 baseline code) with the same test file copied in.
- Date: 2026-10-28. Host: `Linux blacky 7.0.12-201.fc44.x86_64`, quiet host.
- Constants (from `phase2-baseline.md`, provisional there): `ONE_WAY_DELAY_MS = 150`,
  `PROFILE_RATE = 262144` (256 KiB/s), `PROFILE_SIZE = (80, 24)`. Dense-flood evidence runs use 400x100.
- Window constants mirrored in the test file: `FLOOR` 128 KiB, `INITIAL` 256 KiB, `CHUNK` 64 KiB;
  bound for test (3) = `max(FLOOR, 2*R*0.3 s) + CHUNK` = 222,822 B at R = 256 KiB/s.
- All runs: `TMPDIR=/tmp/sp-fd54fa48/mosh-hex.5 PATH=$HOME/.local/bin:$PATH cargo nextest run --locked --test mosh_pacing --no-capture`.

## Test results with the spec probe (`yes`), adaptive window

| test | result | numbers |
|---|---|---|
| (1) `ctrl_c_under_a_flood_answers_within_1_5_s_at_300_ms_rtt` | pass | 311, 324, 324 ms; trace `server_backlog` p95 0 |
| (2) `lan_flood_throughput_matches_the_plain_bridge` | pass | plain 3027 / 2976 B/s, adaptive 3057 / 3276 B/s (means 3002 vs 3167) |
| (3) `the_window_converges_near_the_bandwidth_delay_product_under_a_slow_flood` | FAIL | median window 262144 > bound 222822; goodput 6318 B/s vs 0.8 R = 209715 |
| resume `a_leg_killed_mid_flood_resumes_at_initial_and_ctrl_c_still_answers_in_time` | pass | new-leg `window` event = 262144 (`INITIAL`); after-resume Ctrl-C 309 ms |

Why (3) fails: with a 3-6 KB/s source the window is never the limit, so no epoch is window-limited,
the controller never grows or shrinks, and the window stays at `INITIAL` (256 KiB), which is above
the `2*R*0.3 s + CHUNK` bound (INITIAL is 262144, the bound is 222822). Goodput is the source rate.
Neither assertion exercises the controller. This is the missing amendment noted in the mosh-hex.3
report: "the spec R rule has no solution; plan owner must decide D3/spec amendment before
mosh-hex.5". No decision was recorded in the bead or its comments.

## Ctrl-C latency, before and after

Spec probe, 80x24, 150 ms one way, 256 KiB/s (3 runs each):

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| before (fixed window, `phase2-baseline.md` (c)) | 325 ms | 311 ms | 309 ms |
| after (adaptive window, test (1)) | 311 ms | 324 ms | 324 ms |

Identical within noise: the probe never fills any window.

Dense flood (`SATURATING_FLOOD`, `base64 /dev/urandom`), 400x100, same link
(`saturating_flood_evidence`, ignored, run by hand against each code version):

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| before (fixed 2 MiB window, `eb8583cf`) | 9195 ms | 9170 ms | 8733 ms |
| after (adaptive window) | 1856 ms | 1538 ms | 1703 ms |

Command: `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture saturating_flood`.
The adaptive window is a 5x improvement and removes the 8-9 s drain of 2 MiB at 256 KiB/s, but the
1.5 s bound is missed in 3 of 3 runs (1538 ms is 38 ms over).

Link goodput (bytes written by the downstream delay line, `/proc/<pid>/io` wchar, 8 s):

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| before (fixed window) | 262,195 B/s | 262,216 B/s | 260,165 B/s |
| after (adaptive window) | 245,877 B/s | 209,566 B/s | 171,958 B/s |

Adaptive goodput is 66-94% of R: runs 2 and 3 are below 0.8 R (209,715), run 3 clearly. The window
sits at FLOOR (131072) for most epochs (p50 131072 in runs 2 and 3) while the link needs only the
BDP of 78 KiB, so it is not under-sized by that measure; the dip needs investigation (see Concerns
in the report).

## Server backlog and staleness (D3)

Backlog metric: Recv-Q of the keeper's server stream (= FIONREAD), per `phase2-baseline.md`. The
two sources agree: `server_backlog` in the keeper `window` trace and external `ss -xpn` sampling.

| run | trace p50 / p95 / max (B) | ss p50 / p95 / max (B) | window p50 / max (B) | epochs |
|---|---|---|---|---|
| after 1 | 160,853 / 214,877 / 215,610 | 159,816 / 231,829 / 232,307 | 140,544 / 327,680 | 20 |
| after 2 | 160,194 / 214,263 / 231,632 | 123,757 / 231,098 / 231,632 | 131,072 / 131,072 | 23 |
| after 3 | 160,820 / 214,555 / 215,627 | 160,289 / 231,490 / 231,691 | 131,072 / 131,072 | 20 |
| before (fixed), ss only | n/a (no trace) | 70,031 / 231,032 / 231,587 | n/a | n/a |

Staleness = backlog / R: p95 of the trace is about 214,500 B / 262,144 B/s = **0.82 s**
(max 231,632 B = 0.88 s). This exceeds the 250 ms threshold.

FIONREAD cap: Recv-Q saturates near the server socket buffer (`wmem_default` 212,992; readings up
to 232 KB seen). The p95 sits at the cap, so 0.82 s is a lower bound on the true staleness; the
server also holds up to one rendered frame in userland beyond the socket (`phase2-baseline.md`,
slow-client section), which Recv-Q does not count. A saturated reading cannot size a pacing fix; it
shows that a backlog of at least the cap exists.

## Latency attribution (R1-miss path)

Run 2 (1538 ms), window converged to FLOOR: link propagation 0.30 s (150 up + 150 down), window
drain FLOOR / R = 131,072 / 262,144 = 0.50 s, server backlog 160-215 KB / R = 0.61-0.82 s. Window
and backlog overlap in the pipe only partly; the sum 0.30 + 0.50 + 0.6..0.8 = 1.4-1.6 s matches the
measured 1.5-1.9 s. The part above what the window alone gives (about 0.8 s: 0.30 + 0.50) is the
server-socket backlog. The window is at its floor and cannot shrink further, so the remaining
latency is not the window; it is queued bytes in the server socket that the keeper does not
control. Pre-fill from `phase2-baseline.md` predicted exactly this case (backlog at the cap gives
1.71 s at 256 KiB/s).

## D3 decision

D3 evidence for server pacing is met. Measured at the supported bottom R = 256 KiB/s under a
saturating flood: `server_backlog` p95 = 214,555 B = 0.82 s staleness (> 250 ms), at the FIONREAD
cap (lower bound). Server pacing (per-client, `clamp(SRTT/2, 16, 250)` ms) and the per-client
`SO_SNDBUF` option are **parked as a measured follow-up, not built here**. Alternatively the
supported bottom moves up: at R = 1 MiB/s the same backlog is 0.2 s and the budget closes
(`phase2-baseline.md` budget table); that is a spec decision for the plan owner. This task did not
relax the 1.5 s bound.

## Protocol check

- `git diff 1589edbd -- src/mosh/session.rs | grep -nE '^[-+].*(enum SessionMsg|TYPE_|fn encode|fn next|SESSION_PROTOCOL_VERSION|ACK_EVERY|MAX_DATA_CHUNK|const WINDOW)'`:
  hits are only uses of `MAX_DATA_CHUNK` and `TYPE_DATA` in new/rewritten code and tests
  (lines 120, 122, 188, 208, 214, 235, 250, 263, 280 of the diff); no `enum SessionMsg`,
  `fn encode`, `fn next`, `SESSION_PROTOCOL_VERSION`, `ACK_EVERY` or `const WINDOW` line changes.
- `cargo nextest run --locked --bin herdr -E 'test(codec_round_trips) | test(wire_layout)'`:
  `3 tests run: 3 passed` (`codec_round_trips_byte_by_byte_and_in_random_chunks`,
  `wire_layout_and_json_field_names`, and an unrelated surface-delta decoder test); neither mosh
  test name appears in the diff, so their expectations are unmodified. (Filter note: these tests
  live in the `herdr` bin target, not `--lib`.)

## Closing check

- `git diff --stat 1589edbd`: `evidence/phase2-baseline.md`, `src/mosh/local_bridge.rs`, `src/mosh/mod.rs`,
  `src/mosh/remote/keeper.rs`, `src/mosh/session.rs`, `src/mosh/window.rs`, `tests/mosh_harness.rs`,
  `tests/mosh_pacing.rs` (plus this file): only `src/mosh/**`, `tests/mosh_*` and the run directory.
- `bash scripts/mosh-diff-budget.sh`: `12 inserted upstream lines (budget 20), 0 untagged hunks`.
- `git diff 1589edbd -- Cargo.toml Cargo.lock`: empty.
- `cargo clippy --all-targets --locked -- -D warnings`: clean. `cargo fmt --check`: clean.

## Flakiness (5 consecutive runs)

Not recorded: the five-run criterion requires all tests to pass, and test (3) fails
deterministically with the specified flood. A repeat would only measure the same failure.
