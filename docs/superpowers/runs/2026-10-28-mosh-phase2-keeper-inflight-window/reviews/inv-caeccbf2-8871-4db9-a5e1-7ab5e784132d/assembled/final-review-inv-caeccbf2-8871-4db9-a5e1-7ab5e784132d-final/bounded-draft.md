# Final review (bounded): epic mosh-hex, Mosh Phase 2 adaptive keeper in-flight window

Invocation: inv-caeccbf2-8871-4db9-a5e1-7ab5e784132d
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 08086fb3ea0e038af33eb05ff14d8d8616b53fe9 (both verified with `git rev-parse --verify <sha>^{commit}`).
Route: bounded. review-inputs.json reports status unavailable, sourceAvailable true, wholeRangeReady false, 262 selected, 0 omitted, 5 provenance gaps. The failed assembly and the original sidecar were preserved; no whole-range package was built.
Caller checkout: branch super-auto/mosh-phase2-keeper-inflight-window at 08086fb3 and clean at dispatch. This review made no change to it apart from the authorized ledger binding append and this bundle's files.

## Scope actually read

- The full changed-path inventory (262 paths). The product and test paths are HERDR_MOSH_VISION.md, scripts/mosh-diff-budget.sh, the upstream hook files (src/cli/status.rs, src/client/mod.rs, src/client/shell/composition.rs, src/remote/{attach,host,saved}.rs, src/update.rs), src/mosh/*, src/mosh/remote/* and tests/mosh_*.rs. All other paths are run documents under docs/superpowers/runs/ and docs/superpowers/specs/INDEX.md.
- Read whole: src/mosh/window.rs, the Phase 2 design doc, docs/superpowers/specs/INDEX.md. These are the only hunks listed in bounded-coverage.json.
- Read only as Phase 2 sub-diffs (1604c5fa..08086fb3), so the whole-file captured hunks are not counted as inspected: src/mosh/session.rs (also the head lines 440-640 for set_up, resume and ACK), src/mosh/remote/keeper.rs (also server_reader at head), src/mosh/local_bridge.rs, src/mosh/mod.rs, tests/mosh_harness.rs.
- Partly read: tests/mosh_pacing.rs (header, gate, constants, the four acceptance tests), tests/mosh_resume.rs (skip sites only), HERDR_MOSH_VISION.md (the drain-policy line 79, known issue 3), and the server client-writer lanes and send timeouts in src/server/client_transport.rs and src/server/headless{,/render}.rs as context.
- Commit trailers across the range.

## Step 1: own findings (before the ledger)

1. The epic goal is not met as written, and the branch asserts an autonomously amended bound. The epic and spec §5 promise a Ctrl-C answer within 1.5 s at 300 ms RTT and goodput of at least 0.8 R. tests/mosh_pacing.rs asserts `CTRL_C_BOUND = 2500 ms` and `goodput >= 0.6 R`. Per the design's post-implementation note, the 1.5 s goal was met 1 of 5 times and 0.8 R 2 of 5. docs/superpowers/specs/INDEX.md and the epic description still claim 1.5 s. The amendment is recorded only as an orchestrator decision in an autonomous run, so a human has to accept it, or reopen the goal, before this lands. The same holds for the parked server pacing, per-client SO_SNDBUF and goodput-at-FLOOR follow-ups.
2. The branch reverses Phase 1's keeper drain-while-down policy. `SessionEndpoint::source_room` now caps reads at the reset window (INITIAL, 256 KiB) while no leg is up, so keeper ring overflow is unreachable and an outage backlog stays in the herdr server. The motivation is sound: post-resume Ctrl-C took 15.8-16.7 s with the ring-cap rule. But it is again only an orchestrator decision, and HERDR_MOSH_VISION.md §4.1 line 79 and known issue 3 still describe the old policy. The vision doc gives the server's uncapped control lane as the reason to keep draining. I checked that lane at head: it carries mode toggles, endpoint snapshots and broadcasts, and render output goes through the latest-wins slot. So the accepted risk looks small, but it is unmeasured.
3. Six Phase 2 commits carry `Co-Authored-By: Claude Sonnet 5.5` trailers: 3c79a360, 84e801c6, b34b4f85, bf412b27, c2705d1b and 952fcdfd. The epic's Global Constraints and the repo AGENTS.md forbid AI co-author lines. Another 66 trailers sit on Phase 1 commits inside the same captured range.
4. Every Phase 2 acceptance test is `#[ignore]`: Ctrl-C under a dense flood, LAN parity against the plain bridge, window convergence, and leg-kill resume. The default suite and CI never exercise the epic's goal. Only window.rs's synthetic line model and the session and keeper unit tests guard the controller. The margins are thin: a median of 2061 ms against 2500 ms, single runs up to 2684 ms, and a_leg_killed failed twice at 2705 ms under load. When an opted-in run lacks `perl` or `ss`, the test returns early and passes; the SKIP line shows only with `--no-capture`.
5. src/mosh/window.rs is correct against spec §1 as far as I can trace it. Records are coalesced per ACK_EVERY_BYTES quantum and never dropped (at MAX_RECORDS they merge into the newest record). Sampling is gated on an advance of at least one quantum and applies Karn's rule through `sent_end`, record_start and the endpoint's `fresh_end`. Epochs use a byte-offset boundary closed by the first ACK above it. The hold epoch after a change, the shrink/grow/hold law, the clamps to FLOOR and CAP, and reset to INITIAL all match the spec. Implausible inputs are ignored. The unit tests, including the per-tick line simulation at 256 KiB/s and 1 MiB/s, cover the law, the hold epochs, Karn's rule and sample-less epochs. Nothing must-fix here.
6. Integration seam (own minor, Phase 2-introduced). On resume, `resume_from` moves `send_next` to the peer's recv offset without trimming the ring, so in-flight is measured from the old ring start. Before the drop the window may have grown well above INITIAL, up to CAP. In that case the bytes the peer delivered whose ACKs died with the leg can exceed INITIAL. `poll_send` then finds no room until the peer's re-announced ACK arrives (ACK_EVERY, 200 ms, plus one RTT) and marks the fresh controller window-limited. This is bounded, cannot deadlock, and was impossible in Phase 1, where the window was always the 2 MiB cap. It is untested.
7. keeper `trace_window` runs a FIONREAD ioctl and trace-file I/O under the global session-table lock. This happens only under HERDR_MOSH_TRACE, so production is unaffected, but it perturbs the pacing tests that rely on traces.
8. keeper `server_reader` computes `want` under the lock and then blocks in `read` outside it. If the window shrinks or the leg drops meanwhile, one read can overshoot the new room by at most one sized read (64 KiB or less). The overshoot is bounded and harmless.

## Step 2: ledger triage

- Recurring minor [missing-evidence] x4 (mosh-hex.2, .4, .9, .17): process hygiene in task reports, no product effect. Deferred OK.
- [attribution-line] mosh-hex.6, graded minor: misgraded. It violates a stated epic constraint and AGENTS.md (see own finding 3). Must fix before landing.
- [silent-skip] mosh-hex.12 (tests/mosh_resume.rs, `ss` missing) and [untested-skip-path] mosh-hex.13 name skipped tests, so by rule they are triaged as must-fix. mosh-hex.17 (d367dd9b) made the skips print SKIP and moved the acceptance tests to `#[ignore]`. The remaining must-fix is the ignored acceptance coverage in own finding 4. The tool-missing early return is now visible with `--no-capture`, which is acceptable once the human decides CI placement.
- [thin-margin] mosh-hex.5, [flaky-single-sample-bound] mosh-hex.13 and [flaky-test-risk] mosh-hex.6 are folded into own finding 4 (CI placement and margin decision).
- [unmet-agent-check] mosh-hex.5 names a known unmet product goal, so it is misgraded as minor. Folded into own finding 1, must fix (human decision).
- [provisional-constants] mosh-hex.3: settled by the mosh-hex.5 amendment. The leftover PROVISIONAL comments are deferred.
- [unmeasured-saturating-backlog] mosh-hex.3: closed by saturating_flood_evidence (p95 about 214.5 KB). The D3 decision it feeds is in own finding 1.
- [hold-epoch-sampling], [rounding-exceeds-window], [test-adapted-to-impl], [non-atomic-knob-write], [doc-drift], [doc-formatting], [mirrored-constants], [mirrored-helpers], [weak-cap-assertion], [coverage-gap], [timing-based-test], [long-running-test], [red-phase-skipped], [clippy-not-rerun], [evidence-not-retained], [opt-in-scope]: none affects shipped behaviour. All deferred OK (clippy is covered by the outstanding sweep).
- No parked completions, no parked findings and no BLOCKED-AUTH lines in the ledger. The Metrics lines report ledger-check ok and no landed≠completed.

## Verdict
not ready (bounded review only; epic goal relaxed without human sign-off, drain-while-down reversal undecided, AI co-author trailers, acceptance tests ignored, no full-suite run)

## Must fix before landing
- Human decision: accept closing Phase 2 on the amended bounds (Ctrl-C <= 2.5 s, goodput >= 0.6 R) in place of the epic's 1.5 s and 0.8 R, which are unmet (1/5 and 2/5 runs), and decide on the parked server pacing / per-client SO_SNDBUF / goodput-at-FLOOR follow-ups. Then update the epic text and docs/superpowers/specs/INDEX.md, which still claim 1.5 s.
- Human decision: accept the reversal of Phase 1's keeper drain-while-down policy (source reads capped at INITIAL while down; keeper ring overflow unreachable), which is recorded only as an orchestrator decision. Then update HERDR_MOSH_VISION.md §4.1 line 79 and known issue 3 to match.
- Remove the `Co-Authored-By: Claude Sonnet 5.5` trailers from Phase 2 commits 3c79a360, 84e801c6, b34b4f85, bf412b27, c2705d1b and 952fcdfd (and the 66 on Phase 1 commits in the range), or land as a squash with a clean message. The epic Global Constraints and AGENTS.md forbid them; ledger minor [attribution-line] mosh-hex.6 is misgraded.
- The Phase 2 acceptance tests in tests/mosh_pacing.rs are all `#[ignore]`, so no default or CI run exercises the epic goal. Run `cargo nextest run --locked --test mosh_pacing --run-ignored all --no-capture` at head and record the result, and have the human decide CI placement given the thin margins (median 2061 of 2500 ms; single runs to 2705 ms).
- Run the full-suite sweep at head (cargo nextest run --locked --no-fail-fast, cargo clippy --all-targets --locked -- -D warnings, cargo fmt --check, just check). No full-suite measurement of this branch exists yet; the sweep was deferred to the caller.

## Untested scope
- whole-epic review incomplete; bounded inspected hunks only; all remaining hunks and provenance gaps remain unreviewed
- A whole-range review of 5608de06..08086fb3 remains outstanding regardless of this draft's verdict.
- Provenance gap: docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection.
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete claimed collection.
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete claimed final report.
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 invalid failed assembly archive (missing bounded evidence final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md).
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete assembled bundle (digest.md).
- Whole-file captured hunks not inspected (Phase 2 sub-diffs only): src/mosh/session.rs, src/mosh/remote/keeper.rs, src/mosh/local_bridge.rs, src/mosh/mod.rs, tests/mosh_harness.rs.
- Not inspected: src/mosh/{link_frames,outage,ring,ssh_leg}.rs, src/mosh/remote/{attacher,mod}.rs, upstream hook files (src/cli/status.rs, src/client/mod.rs, src/client/shell/composition.rs, src/remote/{attach,host,saved}.rs, src/update.rs), scripts/mosh-diff-budget.sh, tests/mosh_{fallback,integration,multi_client,status}.rs.
- Partly inspected only: tests/mosh_pacing.rs, tests/mosh_resume.rs, HERDR_MOSH_VISION.md.
- All run documents under docs/superpowers/runs/ except the Phase 2 design doc were not inspected (roasts, evidence, profiles, prior bounded review bundles).
- Long outage after the drain-while-down reversal: server control-lane memory growth, and the 30 s OBSERVER_WRITE_TIMEOUT on ObserveTerminal clients if such a stream is carried over a mosh session, are untested.
- Post-resume stall when the pre-drop window exceeded INITIAL (ring not trimmed to the peer's recv offset until the re-announced ACK, about 200 ms plus RTT) is untested.
- Pacing and link-knob tests have not been run on macOS; the Ctrl-C and LAN-parity tests are not Linux-gated.
- No live link measurement of head; the acceptance numbers come from task branches and the evidence file, not from this commit.

## Deferred OK
- Recurring minor [missing-evidence] x4 (mosh-hex.2, .4, .9, .17): report hygiene only.
- [hold-epoch-sampling] window.rs: the ACK closing a hold epoch is sampled into it; conservative, no wrong decision.
- [rounding-exceeds-window] session.rs source_read_size: ring occupancy may pass the window by under 4 KiB; bytes in flight never do; documented and tested.
- [test-adapted-to-impl] keeper.rs peer_end_drains_to_server_before_shutdown now uses queue_final_end; the raw end_now path with data in flight lost that coverage.
- [non-atomic-knob-write] tests/mosh_harness.rs set_link truncates then writes; test-only race.
- [doc-drift], [doc-formatting], [red-phase-skipped], [clippy-not-rerun], [long-running-test], [timing-based-test], [evidence-not-retained], [opt-in-scope]: hygiene; the clippy rerun is covered by the outstanding sweep.
- [mirrored-constants] and [mirrored-helpers] in tests/mosh_pacing.rs and tests/mosh_resume.rs; only INITIAL drift is asserted.
- [weak-cap-assertion] backlog_plateau; replayed_down discriminates the rule.
- [coverage-gap] split-pane test at 24 KiB/s; upstream-at-INITIAL asserted only in session unit tests.
- [provisional-constants] settled by the mosh-hex.5 amendment; stale PROVISIONAL comments in tests/mosh_pacing.rs can be cleaned up later.
- [unmeasured-saturating-backlog] closed by saturating_flood_evidence.
- Own minor: keeper trace_window does a FIONREAD ioctl and trace I/O under the session-table lock; trace-only.
- Own minor: server_reader sizes its read under the lock and reads outside it; overshoot is at most one sized read.
- Own minor: replay-period window-limited flags can grant one early grow in the first post-resume epoch, followed by a hold epoch.
- Own minor: the keeper RING_OVERFLOW branch in server_reader is now unreachable; keep it as a guard or remove it later.
- base_rtt never ages, so a same-leg RTT rise pins the window at FLOOR until reconnect; the spec accepts this (§1, §9).
