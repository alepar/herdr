# Final review (bounded): epic mosh-hex, integration branch super-auto/mosh-phase2-keeper-inflight-window

Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head e862f3c47be609b208714842929584ae30b34642 (both verified with `git rev-parse --verify`). The invocation/source binding was recorded in the ledger before any write.

## Scope and route

This is a bounded captured-source review, not a whole-range review. The review-inputs sidecar has `status: unavailable`, `sourceAvailable: true`, `wholeRangeReady: false`, no omissions, and five retained provenance gaps (listed under Untested scope). The failed assembly and the original sidecar were left untouched. No whole-range package was built or loaded.

The captured range is 117 commits and 223 changed paths. It contains all of Mosh Phase 1 (squash `be183064` and its history) plus the Phase 2 epic `mosh-hex` (commits `7a1076be..e862f3c4`). The epic's own product delta (`be183064..e862f3c4`) touches `src/mosh/{window,session,mod,local_bridge}.rs`, `src/mosh/remote/keeper.rs`, `tests/mosh_harness.rs` and `tests/mosh_pacing.rs`. I read that delta in full, along with the Phase 2 design doc and the epic (`bd show mosh-hex`). Fully inspected captured hunks are listed in bounded-coverage.json. Files that are new in the captured range but were read only for their Phase 2 delta (session.rs, keeper.rs, local_bridge.rs, mosh_harness.rs) are not claimed as covered.

## Step 1: independent findings against the spec

1. **The epic is incomplete on this branch (blocking).** Only mosh-hex.1–.4 are merged. mosh-hex.5 is open and was BLOCKED twice. It is the acceptance task: Ctrl-C under a flood at 300 ms RTT, LAN throughput parity, window convergence, and re-recording the D3 evidence. mosh-hex.6 (integration sweep) and mosh-hex.9 (outage read cap) are also open. As a result, spec §5's three acceptance tests do not exist on the branch: `tests/mosh_pacing.rs` holds only the harness self-tests, the probe check and two `#[ignore]` baseline evidence tests. The epic's stated goal, Ctrl-C answered within 1.5 s at 300 ms RTT, is not verified by any test at head.

2. **Known product defect: a resume under flood still answers Ctrl-C in about 16 s (blocking; mosh-00q / mosh-hex.9).** At head, `SessionEndpoint::source_room()` and `source_read_size()` in `src/mosh/session.rs` use `ring.capacity()` while no leg is up. The keeper's `server_reader` in `src/mosh/remote/keeper.rs` reads whole 64 KiB chunks into the 16 MiB ring for the whole outage. After resume the window correctly restarts at INITIAL, but that stale backlog drains at R ahead of the Ctrl-C echo. Measured on the mosh-hex.5 branch: 15.8 s and 16.7 s against a 2.5 s bound. The orchestrator's fix is recorded in commit 3f3fbcf5's design note: cap reads at INITIAL while down. It is not implemented. mosh-hex.9 is BLOCKED because that rule makes keeper ring overflow unreachable. That breaks `tests/mosh_resume.rs::ring_overflow_during_an_outage_ends_the_session_without_a_hang`, `keeper.rs::drain_while_down_captures_server_output` and `keeper.rs::overflow_ends_session_next_resume_unknown`. It also reverses the Phase 1 drain-while-down policy in HERDR_MOSH_VISION.md, which exists because the server's control lane has no cap. A plan owner has to decide this.

3. **The acceptance bound was relaxed without a human decision (blocking for landing).** The design's Post-Implementation Notes record that the dense-flood Ctrl-C latency with the adaptive window is 1.54–1.86 s, so the 1.5 s goal is unmet. The goodput floor of 0.8 R is also unmet: measured goodput is 66–94 % of R. An autonomous orchestrator decision moved the acceptance to ≤ 2.5 s, ≤ 0.35× the fixed-window latency, and goodput ≥ 0.6 R. The ledger carries this as `Human action` (progress.md line 34). `docs/superpowers/specs/INDEX.md` still claims "within 1.5 s". The epic's headline claim needs the human's sign-off, or a spec amendment, before the branch lands. The server-pacing / SO_SNDBUF follow-up is parked for the same decision. D3 evidence shows the server-socket backlog at p95 about 214.5 KB, at least 0.82 s of staleness, above the 250 ms clamp.

4. **Controller (`src/mosh/window.rs`): matches spec §1 and the key-abstraction contract; no defect found.** Sampling is gated on an ACK advance of at least `ACK_EVERY_BYTES`. Karn's rule holds through the `record_start`/`sent_end` reset, and `SessionEndpoint` passes `offset.max(fresh_end)` so replays never reach `on_send`. Records are coalesced per quantum and bounded at `MAX_RECORDS` without being dropped. The epoch boundary is a byte offset, a hold epoch follows every change, a sample-less epoch holds, and the window is clamped to FLOOR..=CAP. The unit tests cover each rule, plus a line-model simulation at 256 KiB/s and 1 MiB/s. Accepted tradeoffs per the spec: `base_rtt` never ages, so a same-leg RTT rise pins the window at FLOOR until the next leg; a coalesced record can over-read an RTT under small sends; the upstream direction runs at INITIAL.

5. **Integration seams (session/keeper/bridge): coherent.** The window is fed only from `SessionEndpoint`: `poll_send` reports fresh sends and window-limited, the ACK path calls `on_ack`, and both `set_up` and `leg_down` reset it. The extra window-limited signal in `push_source` (occupancy ≥ window) implements the key-abstractions "gap settled" note. The keeper sizes reads with `source_read_size()`, which rounds up to 4 KiB and caps at one chunk. As a consequence ring occupancy can exceed the window by less than 4 KiB, but bytes in flight cannot. That consequence is documented and tested. The keeper's `window` trace calls FIONREAD only when a trace dir is set. In the local bridge, the go-channel now carries a read size. While down with a full ring it still reads a whole chunk, keeping the Phase 1 overflow rule. No upstream file is touched by Phase 2: the hook hunks in src/cli, src/client, src/remote and src/update are Phase 1 and unchanged in count. No wire or `SESSION_PROTOCOL_VERSION` change, and no new dependency.

6. **Minor (own):** the fake-ssh `link` branch runs `perl … | sh -c "$last" | perl …; exit $?`, so the leg's exit status is the trailing delay line's, not the herdr bridge's. A test that asserts on an ssh exit code under `set_link` would see perl's status. No current test does.

## Step 2: ledger triage

Recurring clusters: none recorded. Parked findings: none (parked 0). BLOCKED-AUTH: none. Metrics are consistent (merges 4, review clean 4, ledger-check ok).

- `[provisional-constants]` (mosh-hex.3): `PROFILE_RATE`/`PROFILE_SIZE` are provisional pending the plan owner's decision on the acceptance profile. This is part of finding 3 and is resolved by the same human decision. It must be settled before mosh-hex.5 can land.
- `[unmeasured-saturating-backlog]` (mosh-hex.3): superseded. The dense-flood measurement in the design's post-implementation note measured it (p95 about 214.5 KB). The remaining action is the human decision in finding 3.
- `[test-adapted-to-impl]` (mosh-hex.4, keeper `peer_end_drains_to_server_before_shutdown`): acceptable. Production END paths use `queue_final_end`, except the bridge's `on_stop`, where dropping queued input under Stopped semantics is intended in Phase 1.
- `[rounding-exceeds-window]`, `[hold-epoch-sampling]`, `[non-atomic-knob-write]`, `[doc-drift]`, `[doc-formatting]`, `[missing-evidence]` ×2: cosmetic or conservative, safe to defer.

## Verdict
not ready (epic incomplete: acceptance mosh-hex.5, outage cap mosh-hex.9 and sweep mosh-hex.6 open; known 16 s Ctrl-C after resume; relaxed 1.5 s goal needs human sign-off; bounded review only, no full-suite run)

## Must fix before landing
- Land mosh-hex.5: the spec §5 acceptance tests (Ctrl-C under flood at 150 ms one way, LAN throughput parity against the plain bridge, window convergence) and the re-recorded D3 evidence. None exists at head, so the epic goal is unverified.
- Fix mosh-00q / mosh-hex.9: while no leg is up, `SessionEndpoint::source_room`/`source_read_size` (src/mosh/session.rs) let the keeper's `server_reader` fill the 16 MiB ring, so Ctrl-C after a resume under flood takes 15.8–16.7 s. A plan-owner decision is needed on the Phase 1 ring-overflow tests (mosh_resume.rs ring_overflow_during_an_outage…, keeper.rs drain_while_down_captures_server_output and overflow_ends_session_next_resume_unknown) and on the HERDR_MOSH_VISION.md control-lane drain policy.
- Get human sign-off on the autonomous relaxation of the acceptance (1.5 s to 2.5 s Ctrl-C, 0.8 R to 0.6 R goodput, dense flood at 400x100, provisional PROFILE_RATE/PROFILE_SIZE) and on the parked server pacing / SO_SNDBUF follow-up, or amend the spec and INDEX.md, which still claim 1.5 s.
- Run mosh-hex.6 and the full suite (nextest, clippy -D warnings, fmt --check, just check). No full-suite measurement of this branch exists yet.

## Untested scope
- Whole-range review is still outstanding. This bounded review covered only the hunks in bounded-coverage.json, and a whole-range review is required before landing whatever this draft says.
- Provenance gap: run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection (docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md at e862f3c4).
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete claimed collection.
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete claimed final report.
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 invalid failed assembly archive (missing bounded evidence final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md).
- Provenance gap: run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete assembled bundle (digest.md).
- src/mosh/session.rs, src/mosh/remote/keeper.rs, src/mosh/local_bridge.rs and tests/mosh_harness.rs: only the Phase 2 delta (be183064..e862f3c4) was read; the Phase 1 bulk of each captured new-file hunk was not reviewed.
- Phase 1 product and test files were not inspected: HERDR_MOSH_VISION.md, scripts/mosh-diff-budget.sh, src/mosh/{mod,link_frames,outage,ring,ssh_leg}.rs, src/mosh/remote/{mod,attacher}.rs, tests/mosh_{fallback,integration,multi_client,resume,status}.rs.
- None of the run artifacts under docs/superpowers/runs/ (Phase 1 and Phase 2 roasts, coverage, profiles, evidence, prior bounded reviews) were inspected, except the Phase 2 design doc.
- No full-suite or clippy/fmt run of head, and no live link measurement of the adaptive window on this branch. The adaptive-window latency numbers come from the unmerged mosh-hex.5 branch, per the design note.
- The `#[ignore]` baseline evidence tests in tests/mosh_pacing.rs (baseline_lan_flood_rate_per_terminal_size, baseline_ctrl_c_latency_at_the_profile) are evidence-only by spec §5 and do not run in CI.

## Deferred OK
- [hold-epoch-sampling] src/mosh/window.rs: the ACK that closes a hold epoch is sampled into that epoch. This is conservative and changes no decision.
- [rounding-exceeds-window] src/mosh/session.rs source_read_size: ring occupancy may exceed the window by < 4 KiB. Bytes in flight never do; documented and tested.
- [test-adapted-to-impl] keeper.rs peer_end_drains_to_server_before_shutdown uses queue_final_end, which matches the production END paths.
- [non-atomic-knob-write] tests/mosh_harness.rs set_link uses fs::write. A leg started concurrently could read an empty knob; no current test races it.
- [doc-drift] phase2-baseline.md cites the WINDOW line as 32 instead of 33.
- [doc-formatting] an over-long SessionEndpoint doc-comment line in src/mosh/session.rs.
- [missing-evidence] task-2 and task-4 reports record evidence only in prose.
- [unmeasured-saturating-backlog] superseded by the dense-flood measurement in the design's post-implementation note. The decision it feeds is under Must fix.
- Own minor: the fake-ssh link branch returns the trailing delay line's exit status, not the bridge's; no test depends on it.
- base_rtt never ages, so a same-leg RTT rise pins the window at FLOOR until reconnect. The spec accepts this (§1, §9).
