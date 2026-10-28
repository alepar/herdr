# Final review (bounded): epic mosh-hex, Phase 2 adaptive keeper in-flight window

Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 003f61b3140e66f4388795be5e3a2b03387fa662 (both verified with `git rev-parse --verify`). Caller checkout observed at HEAD 003f61b3, clean status, before and during the review; nothing was switched or restored.

## Scope and route

This is a bounded captured-source review. The review-input sidecar reports sourceAvailable:true, wholeRangeReady:false, status:unavailable, 248 changed paths selected and 0 omitted, with five retained provenance gaps (Phase 1 run.md:43 inv-c723b6b8 incomplete claimed collection; run.md:56 inv-ed080a9e incomplete claimed collection, incomplete claimed final report, invalid failed assembly archive missing final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md, incomplete assembled bundle digest.md). No whole-range package was built.

The captured range covers Phase 1 and Phase 2 (135 commits, 14,925 inserted lines outside docs/superpowers and .superpowers across 27 product/test paths). I read the full changed-path inventory, the Phase 2 design spec, the epic (`bd show mosh-hex`), and then concentrated on the Phase 2 seams: `src/mosh/window.rs` in full, every upstream hook hunk, `scripts/mosh-diff-budget.sh`, and the Phase 2 deltas (be183064..003f61b3) of `src/mosh/session.rs`, `src/mosh/remote/keeper.rs`, `src/mosh/local_bridge.rs`, `tests/mosh_harness.rs`, `tests/mosh_resume.rs` and the gating/acceptance portion of `tests/mosh_pacing.rs`. Only files read in full against the captured hunk are claimed in the coverage file; the Phase 2 deltas read inside larger new-file hunks are not claimed, and are listed as unreviewed below.

## Step 1: own view of the Phase 2 diff against the spec

What holds up:

- `InflightWindow` (window.rs) matches the spec's control law: per-quantum coalesced send records bounded by `MAX_RECORDS` (coalesced, never dropped), sampling only on ACKs advancing at least `ACK_EVERY_BYTES`, Karn's rule via `sent_end`/`record_start`, per-leg `base_rtt`, epoch boundary as a byte offset, hold epoch after every change, shrink by 3/4 to `FLOOR`, grow by one chunk to `CAP` only when window-limited, idle never grows. It is sans-IO and reads no clock. The line-model simulation test exercises the law against the spec's delay line at 256 KiB/s and 1 MiB/s.
- `SessionEndpoint` is the only feeder: `fresh_end` carries across legs so replays are never reported to the controller; `set_up` and `leg_down` both reset; `poll_send` uses `window.current()` for room and signals window-limited; `push_source` signals window-limited when occupancy reaches the window (the key-abstraction "gap settled here"). No session-protocol constant, codec or receiver ack policy changed.
- Keeper `server_reader` sizes reads with `source_read_size()`; wake-ups are unchanged and sufficient (ACK processing in `run_leg` ends with `kick.notify_all()`, and `admit` kicks on resume). The `window` trace event carries leg, window, base_rtt, epoch_min and FIONREAD backlog, and measures nothing without a trace dir.
- Local bridge preserves the Phase 1 overflow-ends-session behaviour while down (reads a whole chunk when room is 0), consistent with the 2026-10-28 design note.
- Upstream diff budget: Phase 2 adds no upstream lines; the 12 hook lines in the range are all tagged `// mosh-fork hook`. No Cargo.toml/Cargo.lock change.

Cross-task and spec-level findings:

- [Important, needs human decision] The epic's stated goal is not met. Spec and epic promise Ctrl-C under flood within 1.5 s at 300 ms RTT; the shipped acceptance asserts `CTRL_C_BOUND = 2500 ms` (tests/mosh_pacing.rs) and goodput >= 0.6 R instead of 0.8 R. The ledger records 1.5 s met in 1/5 runs and 0.8 R in 2/5. The relaxation was an autonomous-run orchestrator amendment; run.md parks "Must-fix: human decision to accept closing Phase 2 on the amended bounds ... and on parked server pacing / SO_SNDBUF and goodput at FLOOR" as an escalation. No human acceptance is recorded in the captured source.
- [Important, needs human decision] Phase 2 reverses Phase 1's drain-while-down policy (HERDR_MOSH_VISION.md §4.1): while down, keeper reads cap at INITIAL, keeper ring overflow becomes unreachable, and the Phase 1 overflow tests were rewritten (`tests/mosh_resume.rs` ring_overflow test replaced; keeper.rs `overflow_ends_session_next_resume_unknown` replaced). The rationale (server control lane carries only rare small messages; render lane coalesces) is recorded as an orchestrator decision (blocker mosh-ctt), not a human one. Residual risk: during a long outage (grace up to 600 s) the server's unbounded control `VecDeque` grows with every control message and the keeper never drains it; and if the keeper's server connection is ever an observer connection, `OBSERVER_WRITE_TIMEOUT` (30 s, src/server/client_transport.rs:45) would disconnect a reader stalled that long. Neither is tested.
- [Important, process] Six of the ten Phase 2 non-merge commits carry `Co-Authored-By: Claude Sonnet 5.5` trailers (for example 3c79a360, 84e801c6, c2705d1b, 952fcdfd, bf412b27), against repo AGENTS.md ("no AI co-author lines") and the epic's global constraint. Commit subjects feed preview release notes, so this needs a history rewrite or squash before landing.
- [Untested] The Phase 2 acceptance tests in tests/mosh_pacing.rs (Ctrl-C under dense flood, LAN parity, convergence, resume-under-flood, two-client independence, split-pane typing) return early and pass unless `HERDR_MOSH_PACING_TESTS=1` is set and `perl` and `ss` exist. `just check` / the default nextest sweep therefore reports them green without exercising the phase's acceptance; a skip is indistinguishable from a pass in the summary. `#[ignore]` (as the evidence tests already use) would make the skip visible. Likewise tests/mosh_resume.rs's dense-flood outage test silently passes when `ss` is missing.
- [Minor] Thin margins on the opt-in wall-clock bounds: test (1)'s median reached 2061 ms with single samples to 2684 ms against 2500 ms; the resume case asserts a single sample against 2500 ms and failed twice at 2705 ms under load (ledger, mosh-hex.13). As acceptance evidence these are flaky on a loaded host.
- [Minor] `trace_window` does a FIONREAD ioctl and a trace-file write while holding the keeper's table lock (in `run_leg`'s per-read path and on leg up). Trace-only, so not a production cost, but it lengthens the critical section on traced hosts and can perturb the timing tests that rely on the trace.
- [Minor] Test crates mirror crate-private constants (FLOOR, INITIAL, CHUNK in mosh_pacing.rs; KEEPER_RESET_WINDOW, DENSE_SIZE, SATURATING_FLOOD, timing_lock, server_backlog duplicated into mosh_resume.rs). Only INITIAL drift is asserted.

No correctness defect was found in window.rs or in the inspected session/keeper/local-bridge deltas.

## Step 2: ledger triage (progress.md)

Metrics report merges 9, parked 0, fix-pass entered 0, ledger-check ok, so the ledger appears complete for its observed sources. Recurring minor: `[missing-evidence]` x3 across mosh-hex.2, .4, .9 (process-class: reports carry prose evidence only). Grouped observation: a process gap in task reports, not a product defect; deferred OK, but it is part of why this review cannot rely on recorded test evidence.

Triage of deferred minors:

- mosh-hex.6 `[attribution-line]` Co-Authored-By trailer: confirmed across six commits; must fix before landing (see above).
- mosh-hex.5 `[unmet-agent-check]` 1.5 s goal met 1/5 and 0.8 R met 2/5: misgraded as minor; this is the epic's headline goal and needs explicit human acceptance before landing.
- mosh-hex.12 `[silent-skip]` dense-flood outage test passes when `ss` is missing, and mosh-hex.13 `[untested-skip-path]`: a skipped test reported as passing; per the triage rule it is must-fix (make skips visible) or must be explicitly accepted by the human together with the opt-in gate.
- mosh-hex.5 `[thin-margin]`, mosh-hex.13 `[flaky-single-sample-bound]`, mosh-hex.6 `[flaky-test-risk]` (two_clients test stalled once): the acceptance evidence is flaky under load; acceptable only because the tests are opt-in, which is itself the untested-scope problem above. Recommend a median for the resume case.
- mosh-hex.12 `[weak-cap-assertion]`: the plateau step does not discriminate the reverted rule; only `replayed_down` does. Deferred OK since `replayed_down` covers it.
- mosh-hex.6 `[coverage-gap]` split-pane test at 24 KiB/s, upstream-controller-at-INITIAL not asserted end to end (it is asserted in the session unit test `a_flood_grows_the_window_on_either_side_and_sparse_input_keeps_initial`): deferred OK.
- mosh-hex.2 `[hold-epoch-sampling]`: conservative, verified in window.rs `close_epoch`; deferred OK.
- mosh-hex.4 `[rounding-exceeds-window]` (ring may exceed window by < 4 KiB; in-flight never does), `[test-adapted-to-impl]`, `[doc-formatting]` (over-long SessionEndpoint doc line confirmed): deferred OK.
- mosh-hex.1 `[non-atomic-knob-write]`: test-only race on set_link; deferred OK.
- mosh-hex.3 `[doc-drift]`, `[unmeasured-saturating-backlog]` (closed by mosh-hex.5 evidence), `[provisional-constants]` (settled by the amendment): deferred OK.
- mosh-hex.5 `[clippy-not-rerun]`, `[mirrored-constants]`; mosh-hex.9 `[red-phase-skipped]`, `[timing-based-test]`, `[long-running-test]` (~44 s); mosh-hex.12 `[mirrored-helpers]`; mosh-hex.13 `[evidence-not-retained]` (opt-in run logs only under /tmp): deferred OK, but the last means no in-repo record of the opt-in acceptance runs exists.

Parked items from run.md (escalations, not ledger `parked` lines): server pacing / per-client SO_SNDBUF to close the ~0.8 s server-backlog staleness; goodput at FLOOR 66-94% of R; acceptance on amended bounds. All three await the human. No BLOCKED-AUTH lines.

No full-suite measurement of this branch exists yet; the caller runs the sweep after this invocation, so the branch is not treated as tested here.

## Unreviewed

- All provenance gaps listed under Scope and route (Phase 1 run.md:43 and run.md:56 collections, final report, failed assembly archive, digest.md).
- Whole-file hunks not claimed in coverage: src/mosh/session.rs, src/mosh/remote/keeper.rs, src/mosh/local_bridge.rs, src/mosh/mod.rs, src/mosh/link_frames.rs, src/mosh/outage.rs, src/mosh/remote/attacher.rs, src/mosh/remote/mod.rs, src/mosh/ring.rs, src/mosh/ssh_leg.rs (Phase 1 code was not reviewed; Phase 2 deltas of the first three were read but their full captured hunks were not).
- All tests/mosh_*.rs hunks (mosh_harness.rs, mosh_pacing.rs, mosh_resume.rs partly read; mosh_fallback.rs, mosh_integration.rs, mosh_multi_client.rs, mosh_status.rs not read).
- HERDR_MOSH_VISION.md and all docs/superpowers and .superpowers run artifacts beyond the Phase 2 design and progress.md.

## Verdict
not ready (bounded review: epic's 1.5 s goal unmet and amended bounds await human acceptance; AI co-author trailers on six commits; acceptance tests skip silently by default; no full-suite run)

## Must fix before landing
- Human decision: accept closing Phase 2 on the amended bounds (Ctrl-C <= 2.5 s, goodput >= 0.6 R) instead of the epic's 1.5 s and 0.8 R, with server pacing / SO_SNDBUF and goodput at FLOOR parked.
- Human decision: accept the reversal of Phase 1's drain-while-down policy (keeper reads capped at INITIAL while down, keeper ring overflow unreachable) recorded only as an orchestrator decision.
- Remove the Co-Authored-By trailers from the six Phase 2 commits (rewrite or squash) per AGENTS.md and the epic constraint.
- Make the opt-in and tool-missing skips in tests/mosh_pacing.rs and tests/mosh_resume.rs visible (for example #[ignore] with a documented opt-in run) or have the human explicitly accept silent-pass skips.

## Untested scope
- Whole-range review outstanding: bounded route; provenance gaps for Phase 1 run.md:43 inv-c723b6b8 and run.md:56 inv-ed080a9e (collection, final report, failed assembly archive, digest.md) unresolved.
- Phase 1 product code (session.rs, keeper.rs, local_bridge.rs, ssh_leg.rs, attacher.rs, remote/mod.rs, ring.rs, outage.rs, link_frames.rs, mosh/mod.rs) not reviewed as whole hunks in this invocation.
- Phase 2 acceptance tests do not run in the default suite (HERDR_MOSH_PACING_TESTS opt-in); opt-in run logs are not retained in the repo.
- Long outage behaviour after the drain-while-down reversal: unbounded server control-lane growth and any server-side send timeout on a stalled keeper reader are untested.
- No full-suite measurement of this branch exists yet (deferred to the caller).

## Deferred OK
- hold-epoch-sampling (window.rs close_epoch): conservative, no wrong decision.
- rounding-exceeds-window: ring may pass the window by under 4 KiB; in-flight never does; documented and tested.
- trace_window does FIONREAD and a trace write under the keeper table lock; trace-only.
- Mirrored constants and helpers in tests/mosh_pacing.rs and tests/mosh_resume.rs.
- non-atomic-knob-write in Remote::set_link (test-only).
- weak-cap-assertion in backlog_plateau; replayed_down discriminates.
- split-pane test at 24 KiB/s and upstream INITIAL asserted only in session unit tests.
- missing-evidence recurring process minor across mosh-hex.2, .4, .9.
- doc-drift, doc-formatting, red-phase-skipped, clippy-not-rerun, long-running-test, timing-based-test minors.
