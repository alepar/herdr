# Final review (bounded) — epic mosh-hex, Phase 2 adaptive keeper in-flight window

Integration branch `super-auto/mosh-phase2-keeper-inflight-window`, captured range
`5608de0608918aba2f9d501e7793b984a0ca9cfa..42b512f7cd8c106032ac69764768d33b55fb4f3e` (both verified with
`git rev-parse --verify`). Caller checkout observed clean at HEAD 42b512f7 during the review; nothing was
switched, restored or edited.

## Route and scope

Bounded captured-source review. `review-inputs.json` reports `status: unavailable`, `sourceAvailable: true`,
`wholeRangeReady: false`, 207 selected, 0 omitted, 5 provenance gaps (all in the Phase 1 run log
`docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md` at lines 43 and 56, invocations
inv-c723b6b8 and inv-ed080a9e). No whole-range package was built or loaded. The failed assembly and its
sidecar were preserved.

The captured range spans two phases: Phase 1 (commit be183064, the resumable session, ~12.5k lines of
`src/mosh` and tests) and Phase 2 (this epic, be183064..42b512f7: 7 product/test files, +1596/-56).
Every `src/mosh` file except `window.rs` is a new file across the range, so its only captured hunk is the
whole file. This pass read the whole-file hunks of `window.rs`, `mod.rs`, `tests/mosh_pacing.rs`, the
Phase 2 design doc and all upstream hook hunks; it read the Phase 2 sub-diffs (be183064..42b512f7) of
`session.rs`, `remote/keeper.rs` and `local_bridge.rs` plus nearby call sites, but did not read those
whole-file hunks, so they are recorded as unreviewed in coverage.

## Spec and task state

Epic `mosh-hex` goal: Ctrl-C under an output flood answers within 1.5 s at 300 ms RTT, via an adaptive
delay-based sender window. Children: mosh-hex.1-.4 closed and merged; mosh-hex.5 (acceptance: Ctrl-C bound,
LAN throughput parity, convergence, D3 evidence) is OPEN and BLOCKED behind blocker bead `mosh-4pz`; its work
sits on `task-mosh-hex.5` (357680f9), not on this branch. mosh-hex.6 (integration sweep) is OPEN and not run.

## Step 1 — own view (diff against spec)

### Must fix / blocks landing

1. The epic's headline requirement is not demonstrated on this branch, and the only measurement says it is
   not met. `tests/mosh_pacing.rs` at head contains only the harness self-tests, the probe check and two
   `#[ignore]` baseline evidence tests (its own header says the acceptance tests "are added here later").
   None of spec §5's three acceptance tests (Ctrl-C <= 1.5 s, LAN throughput >= 0.9x plain bridge, window
   convergence + goodput >= 0.8 R) exist on the branch. The mosh-hex.5 report and `mosh-4pz` record that,
   as specified, the convergence test fails deterministically (window stuck at INITIAL 262144 > bound 222822;
   goodput 6.3 KB/s) and the Ctrl-C test passes without discriminating (0.31 s with either window), because
   the spec probe `yes` produces only ~3 KB/s of screen diffs. Under a dense flood the adaptive window gets
   Ctrl-C to 1.54-1.86 s (vs 8.7-9.2 s fixed) and misses 1.5 s in 3 of 3 runs. Landing now ships a control
   law whose acceptance is open; this needs the plan owner's decision on `mosh-4pz` (probe/profile, supported
   bottom rate, D3 server pacing) and a merged, passing mosh-hex.5.

2. Possible throughput regression under a saturating flood is unexplained. mosh-hex.5 measured adaptive
   client-side goodput of 172-246 KB/s (66-94% of R = 256 KiB/s) against ~262 KB/s with the fixed window. At
   300 ms RTT the BDP is ~77 KB, below FLOOR (128 KiB), so the window alone should not cap throughput. One
   plausible explanation is that the server's latest-wins slot now coalesces more frames, which lowers
   delivered bytes without lowering useful updates, but nobody has shown that. Either explain it with
   evidence or treat it as a regression against spec §5's 0.8 R floor. The ledger's Human action line also
   asks for this investigation.

### Verified as correct against the spec (no finding)

- `InflightWindow` (`src/mosh/window.rs`) matches §1: coalescing per `ACK_EVERY_BYTES` quantum and keeping the
  earlier instant, bounded at `MAX_RECORDS` by coalescing rather than dropping; sampling only on ACKs that
  advance by at least 64 KiB; `first_send_of(acked-1)` read before records are popped; byte-offset epoch
  boundary closed by the first ACK above it; hold epoch after every change; shrink to `max(FLOOR, 3/4 w)` when
  q > 50 ms, grow by one chunk only when window-limited; reset to INITIAL with no samples. It is sans-IO and
  takes time as an input. The compile-time assert `FLOOR <= INITIAL <= CAP && MAX_DATA_CHUNK <= FLOOR` holds.
  The line-model simulation test covers the 256 KiB/s and 1 MiB/s convergence bound at the unit level.
- `SessionEndpoint` wiring (§2): `fresh_end` survives leg resets, so replays are never reported (Karn), and
  this is covered by `replayed_bytes_are_never_reported_to_the_controller`. `set_up` and `leg_down` reset the
  controller at `ring.start_offset()`. `poll_send` room uses `window.current()`. The window-limited signal is
  raised both when `poll_send` has bytes and no room and in `push_source` once occupancy reaches the window
  (the key-abstractions "gap settled here"). The ring-cap rule while down is unchanged. Production ring caps
  (8 MiB local, 16 MiB keeper) exceed `WINDOW + MAX_DATA_CHUNK`, so the 4 KiB read round-up cannot overflow
  while up.
- Keeper (§3, §7): `server_reader` reads `source_read_size()` bytes. A `window` trace (leg, window, base_rtt,
  epoch_min, server_backlog via FIONREAD) is emitted on leg up and once per closed epoch, and it measures
  nothing when tracing is off. Local bridge sizes client reads symmetrically and keeps the down/full-ring
  whole-chunk read that triggers overflow.
- Constraints: no new upstream hook lines in Phase 2 (all hooks in the range are Phase 1 lines tagged
  `// mosh-fork hook`). No session or wire protocol change is visible in the inspected diffs. No new
  dependency (FIONREAD uses the existing `libc`).

### Minor (own view)

- `src/mosh/remote/keeper.rs` `trace_window` runs the FIONREAD ioctl and the trace file write while holding
  the session table lock (called inside `shared.lock()` in `run_leg`). This only happens under
  `HERDR_MOSH_TRACE`, but it lengthens lock hold time on the ACK path in traced runs, which are exactly the
  runs used for timing evidence.
- Window-limited flags raised during a post-resume replay (before the first fresh send sets the epoch
  boundary) carry into the first real epoch and can grant one early grow. This is harmless (the grow is one
  chunk and the next epoch holds).
- `src/mosh/mod.rs`: the doc line "`true` means try the resume path; `false` ... takes the plain upstream
  bridge" sits on `idle_opt_in` instead of `platform_gate`, which it describes (Phase 1, cosmetic).

## Step 2 — ledger triage

No `Recurring minor:`, `Recurring blocker:`, `parked`/`parked finding` or `BLOCKED-AUTH` lines. One task
BLOCKED line (mosh-hex.5) and one Human action line.

- Task 5 BLOCKED (mosh-hex.5, blocker `mosh-4pz`): must be resolved before landing (see Must fix). It is a
  plan-mandated spec problem, so the plan owner must decide.
- [provisional-constants] (mosh-hex.3): `PROFILE_RATE`/`PROFILE_SIZE` are provisional because the spec's R
  rule has no solution. This is the root cause of the mosh-hex.5 block and is misgraded as minor. Must fix
  through the owner decision.
- [unmeasured-saturating-backlog] (mosh-hex.3): closed by the mosh-hex.5 dense-flood measurement (backlog p95
  ~214.5 KB = 0.82 s staleness, at the FIONREAD cap). That evidence is not on this branch. D3 (server pacing)
  is met and parked, not built, and should be filed as a follow-up bead when mosh-hex.5 lands.
- [hold-epoch-sampling] (mosh-hex.2): the ACK closing a hold epoch is sampled into it. Conservative, no wrong
  decision. Deferred OK.
- [rounding-exceeds-window] (mosh-hex.4): ring occupancy may exceed the window by < 4 KiB while bytes in flight
  never do. Documented and tested. Deferred OK.
- [test-adapted-to-impl] (mosh-hex.4): `peer_end_drains_to_server_before_shutdown` now uses
  `queue_final_end`. The raw immediate-END path (`end_now`, used by local_bridge `on_stop`) is no longer
  exercised by that test while data is in flight. Deferred OK as a test-coverage gap: production `on_stop`
  semantics are unchanged from Phase 1.
- [non-atomic-knob-write] (mosh-hex.1): `set_link` writes the knob with truncate-then-write, so a concurrent
  leg could read an empty file. Test-only flake risk. Deferred OK.
- [doc-drift] (mosh-hex.3), [doc-formatting] (mosh-hex.4), [missing-evidence] (mosh-hex.2, mosh-hex.4): report
  and doc hygiene. Deferred OK.

## Unreviewed in this bounded pass

- Whole-file hunks of `src/mosh/session.rs`, `src/mosh/remote/keeper.rs`, `src/mosh/local_bridge.rs` and
  `tests/mosh_harness.rs` (only their Phase 2 sub-diffs were read).
- All other Phase 1 product and test files: `src/mosh/{link_frames,outage,ring,ssh_leg}.rs`,
  `src/mosh/remote/{attacher,mod}.rs`, `tests/mosh_{fallback,integration,multi_client,resume,status}.rs`,
  `scripts/mosh-diff-budget.sh` and `HERDR_MOSH_VISION.md`.
- All run docs other than the Phase 2 design doc (Phase 1 run directory; Phase 2 roast/coverage/run/evidence
  files, of which `evidence/phase2-baseline.md` was read only in part), and `docs/superpowers/specs/INDEX.md`.
- Provenance gaps: Phase 1 `run.md:43` inv-c723b6b8 (incomplete claimed collection) and `run.md:56`
  inv-ed080a9e (incomplete claimed collection, incomplete claimed final report, invalid failed assembly archive
  missing `final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md`, incomplete assembled bundle digest.md).
  Phase 1's final review evidence is therefore incomplete.
- No full-suite measurement of this branch exists (the sweep is deferred to the caller, and mosh-hex.6 was not
  run). The branch is untested as a whole.

## Verdict
not ready (epic acceptance unmet: mosh-hex.5 blocked on mosh-4pz, Ctrl-C bound missed under dense flood, no acceptance tests or full-suite run on branch)

## Must fix before landing
- mosh-hex.5 / mosh-4pz: the plan owner must decide the acceptance probe/profile (spec `yes` probe floods ~3 KB/s and cannot exercise the window), the supported bottom rate, and D3; then merge passing spec §5 acceptance tests (Ctrl-C <= 1.5 s, LAN throughput >= 0.9x plain bridge, convergence/goodput)
- Ctrl-C under a dense flood at 256 KiB/s, 300 ms RTT measured 1.54-1.86 s with the adaptive window (3/3 runs over 1.5 s); the epic goal is unmet as specified until the bound, profile or server pacing changes
- Unexplained adaptive goodput of 66-94% of R under a saturating flood (vs ~262 KB/s fixed window) must be explained with evidence (for example server-side frame coalescing) or fixed against the spec's 0.8 R floor
- [provisional-constants] mosh-hex.3 minor is misgraded: PROFILE_RATE/PROFILE_SIZE have no rule-derived value and block acceptance; resolve with the owner decision above
- mosh-hex.6 integration sweep (cargo nextest + clippy -D warnings + fmt --check) has not run on this branch

## Untested scope
- No full-suite measurement of this branch exists; the sweep is deferred to the caller
- Spec §5 acceptance tests are absent from the branch (they exist only on task-mosh-hex.5, one failing)
- LAN dense-flood throughput, adaptive vs fixed window or plain bridge, was never measured (the spec probe is ~3 KB/s)
- Raw immediate-END (end_now) path while data is in flight is no longer covered by peer_end_drains_to_server_before_shutdown
- Bounded review: whole-file hunks of src/mosh/session.rs, src/mosh/remote/keeper.rs, src/mosh/local_bridge.rs and tests/mosh_harness.rs were not read (Phase 2 sub-diffs only)
- Bounded review: Phase 1 files src/mosh/{link_frames,outage,ring,ssh_leg}.rs, src/mosh/remote/{attacher,mod}.rs, tests/mosh_{fallback,integration,multi_client,resume,status}.rs, scripts/mosh-diff-budget.sh, HERDR_MOSH_VISION.md were not reviewed
- Bounded review: run docs other than the Phase 2 design doc and docs/superpowers/specs/INDEX.md were not reviewed
- Provenance gap: Phase 1 run.md:43 inv-c723b6b8 incomplete claimed collection
- Provenance gap: Phase 1 run.md:56 inv-ed080a9e incomplete claimed collection, incomplete claimed final report, invalid failed assembly archive (missing final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md), incomplete assembled bundle digest.md
- A whole-range final review remains outstanding

## Deferred OK
- [hold-epoch-sampling] mosh-hex.2: ACK closing a hold epoch sampled into it; conservative, no wrong decision
- [rounding-exceeds-window] mosh-hex.4: ring occupancy may exceed the window by < 4 KiB; bytes in flight never do; documented and tested
- [test-adapted-to-impl] mosh-hex.4: peer_end test switched to queue_final_end; production on_stop semantics unchanged from Phase 1
- [non-atomic-knob-write] mosh-hex.1: set_link truncate-then-write; test-only flake risk
- [unmeasured-saturating-backlog] mosh-hex.3: measured on task-mosh-hex.5 (p95 ~214.5 KB, 0.82 s); file D3 server pacing / SO_SNDBUF as a follow-up bead when mosh-hex.5 lands
- [doc-drift] mosh-hex.3, [doc-formatting] mosh-hex.4, [missing-evidence] mosh-hex.2 and mosh-hex.4: report and doc hygiene
- keeper trace_window does FIONREAD and trace I/O under the session table lock; only under HERDR_MOSH_TRACE
- Replay-time window-limited flags can grant one early grow in the first post-resume epoch; one chunk, followed by a hold epoch
- src/mosh/mod.rs platform_gate doc comment is attached to idle_opt_in (Phase 1, cosmetic)
