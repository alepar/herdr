# Final review (bounded) — epic mosh-hex, branch super-auto/mosh-phase2-keeper-inflight-window

Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 0fbc133e58aa1a295f27cf1a458ef51696e09e1e
(both verified with `git rev-parse --verify`). The caller checkout was clean and at 0fbc133e when the review ran.

Bounded route: review-inputs.json says sourceAvailable true, wholeRangeReady false, status unavailable,
no omissions, and 5 retained provenance gaps (Phase 1 run.md:43 and :56). The captured range covers Phase 1
and Phase 2 (215 paths). I read the full changed-path inventory. In detail I inspected only the Phase 2
product delta (`git diff be183064..0fbc133e -- src tests scripts`), the Phase 2 spec and the Phase 2 run log.
No whole-range package was built.

## Step 1: own view against the spec

Phase 2 product delta: src/mosh/window.rs (new), src/mosh/session.rs, src/mosh/remote/keeper.rs,
src/mosh/local_bridge.rs, src/mosh/mod.rs (one `mod window;` line), tests/mosh_harness.rs and
tests/mosh_pacing.rs. No upstream file is touched in Phase 2, so the hook budget stays at 12. The diff shows
no change to Cargo files, session protocol constants or the codec.

What matches the spec:
- `InflightWindow` (window.rs) implements spec §1 as written: coalesced send records per ACK quantum with a
  bound and coalescing, never dropping; samples only on ACKs that advance at least `ACK_EVERY_BYTES`; Karn by
  reporting only fresh bytes (`SessionEndpoint::fresh_end`); a byte-offset epoch boundary; a hold epoch after
  every change; shrink by 3/4 down to FLOOR when q > 50 ms, otherwise grow by one chunk only when the epoch was
  window-limited; reset to INITIAL on leg up and leg down. It is sans-IO (time is an input). The unit tests
  include a line-model simulation at 256 KiB/s and 1 MiB/s.
- SessionEndpoint (spec §2, §3): `source_room()`, `source_read_size()` (rounds up to 4 KiB, at most one
  chunk), `poll_send` room taken from the controller, window-limited set both by `poll_send` with no room and
  by `push_source` reaching the window (the key-abstractions gap). The ring-cap rule is kept while down.
- Keeper: `server_reader` sizes reads with `source_read_size()`; a `window` trace (window, base_rtt,
  epoch_min, FIONREAD `server_backlog`) is emitted on leg up and once per epoch, only when tracing is on. The
  local bridge client reader is sized the same way, and while down with a full ring it still reads a whole
  chunk, so ring-cap overflow semantics are kept.
- Harness: a Perl delay line behind the `link` knob, `set_link`, `clear_link`, `linked_legs`, and self-tests
  for RTT and rate.

Cross-task and spec-level findings:

1. (Blocking) The epic's headline requirement is neither tested nor met on this branch. tests/mosh_pacing.rs
   at head has only the harness self-tests, the probe check and two `#[ignore]` baseline evidence tests. Its
   header says the acceptance tests "are added here later". mosh-hex.5 (acceptance) and mosh-hex.6
   (integration sweep) are open in `bd show mosh-hex` (6/8 children closed). The spec's own post-implementation
   note records Ctrl-C at 1.54-1.86 s with the adaptive window under a dense flood, against the 1.5 s goal. An
   orchestrator amendment moves the bound to 2.5 s and to 0.35x the fixed-window latency. That relaxes the
   epic's headline acceptance during an autonomous run, so a human should confirm it.
2. (Blocking, cross-task composition; design-mandated, so the human must decide) Resume under flood defeats
   the Phase 2 goal. While a leg is down, `SessionEndpoint::source_room()` returns
   `ring.capacity() - occupancy` (session.rs, spec §2 "the ring cap while down"). The keeper ring is
   `KEEPER_RING_CAP` = 16 MiB, so during an outage `server_reader` keeps draining the flood from the server
   into the ring. After resume, everything above `fresh_end` is fresh data, sent at the adaptive window
   (FLOOR at 256 KiB/s), and all of it sits ahead of the Ctrl-C echo. The server's latest-wins slot never gets
   to coalesce it. The second mosh-hex.5 dispatch measured Ctrl-C after resume at 15.8 s and 16.7 s (progress
   ledger, Task 5 BLOCKED, second entry). This seam exists only once Phase 1's drain-while-down meets the
   Phase 2 goal. No task owned it, and mosh-hex.4's closed acceptance requires the ring-cap rule unchanged. The
   ledger lists options: (A) cap reads while down at the reset window, which needs a spec §2 amendment, a
   session.rs change, a rewrite of the mosh-hex.4 test, and a check that Phase 1's no-loss resume still
   holds; (B) assert Ctrl-C only after the backlog has drained; (C) park the bullet.
3. (Important, unexplained) Adaptive goodput under a saturating flood was measured at 66-94% of R, against
   about 262 KB/s with the fixed window. At 300 ms RTT the BDP (~77 KB) is below FLOOR (128 KiB), so the
   window alone should not cap throughput. One unshown explanation is more server-side coalescing. The spec's
   0.8 R floor was relaxed to 0.6 R by the same orchestrator amendment. This needs evidence or a human
   decision.
4. (Minor, own view) During post-resume replay the controller is frozen at INITIAL. Replayed bytes are never
   reported, so `epoch_boundary` stays `None` and no epoch closes until fresh bytes go out. Replay of the
   at-most-one-window of bytes in flight at leg-down therefore runs at INITIAL/RTT (about 850 KiB/s at 300 ms),
   where Phase 1 used 2 MiB. This is bounded by the pre-outage window and is moot under option (A) of
   finding 2.
5. (Minor, own view) `trace_window` runs a FIONREAD ioctl and appends a trace file while holding the keeper's
   session-table lock (run_leg). It happens only under `HERDR_MOSH_TRACE`, but that is exactly how the pacing
   tests measure, so trace I/O can perturb timing across sessions.
6. (Minor, own view) Coalescing keeps the earliest instant per 64 KiB quantum. With a source-bound trickle
   in sub-quantum sends, samples over-read by the spread of sends within the quantum, which can cause a
   spurious shrink. The spec accepts this ("never under-reads"). Growth after a flood starts is
   +1 chunk per two epochs.

## Step 2: ledger triage

- Recurring minor / Recurring blocker lines: none in the ledger.
- BLOCKED-AUTH lines: none.
- Parked completions and parked findings: none. Metrics show parked 0 and fix-pass 0.
- `[provisional-constants]` (mosh-hex.3) is misgraded. PROFILE_RATE and PROFILE_SIZE at head are still
  80x24 with the `yes` probe, which the evidence shows cannot load the link. They block acceptance, so this is
  must-fix and is resolved only when the amended mosh-hex.5 lands.
- `[unmeasured-saturating-backlog]` (mosh-hex.3): measured on the unlanded task-mosh-hex.5 branch (p95 about
  214.5 KB, at least 0.82 s of staleness). It closes when mosh-hex.5 lands. D3 pacing and SO_SNDBUF are parked
  in run.md for the human.
- `[test-adapted-to-impl]` (mosh-hex.4): the raw immediate-END path with data in flight lost its keeper test.
  This is untested scope, not a skip.
- `[hold-epoch-sampling]`, `[rounding-exceeds-window]`, `[non-atomic-knob-write]`, `[doc-drift]`,
  `[doc-formatting]`, `[missing-evidence]` x2: deferrable.
- Ledger human action: mosh-hex.5 acceptance profile and D3 decision, plus the goodput investigation. These
  are still open.

## Verdict
not ready (spec §5 acceptance absent and blocked twice on mosh-hex.5; post-resume Ctrl-C at 15.8-16.7 s needs a design decision; no full-suite run)

## Must fix before landing
- Merge a passing mosh-hex.5: spec §5 acceptance tests (Ctrl-C under flood, LAN parity >= 0.9x plain bridge, convergence and goodput) are absent from tests/mosh_pacing.rs at head
- Human must confirm the orchestrator amendment that relaxed the epic goal (1.5 s to <= 2.5 s and <= 0.35x fixed; goodput 0.8 R to 0.6 R); the measured adaptive Ctrl-C was 1.54-1.86 s at 256 KiB/s and 300 ms RTT
- Resume under flood: while down, source_room() uses the 16 MiB keeper ring cap, so the keeper buffers the outage flood and post-resume Ctrl-C took 15.8-16.7 s; the design mandates this, so the human picks option A, B or C from the mosh-hex.5 blocker
- Explain with evidence, or fix, adaptive goodput at 66-94% of R under a saturating flood (fixed window about 262 KB/s; BDP below FLOOR)
- [provisional-constants] mosh-hex.3 minor is misgraded: PROFILE_RATE/PROFILE_SIZE and the `yes` FLOOD_PROBE at head cannot load the link; resolve via the amended mosh-hex.5
- mosh-hex.6 integration sweep (cargo nextest, clippy -D warnings, fmt --check) has not run; no full-suite measurement of this branch exists

## Untested scope
- No full-suite measurement of this branch exists; the sweep is deferred to the caller
- Bounded review: only window.rs, tests/mosh_pacing.rs, the Phase 2 spec and the Phase 2 run.md were read whole; session.rs, remote/keeper.rs, local_bridge.rs, mod.rs and tests/mosh_harness.rs were read only as their Phase 2 sub-diffs (be183064..head), and their whole-file captured hunks remain unreviewed
- Bounded review: Phase 1 product files (src/mosh/{link_frames,outage,ring,ssh_leg}.rs, src/mosh/remote/{attacher,mod}.rs, tests/mosh_{fallback,integration,multi_client,resume,status}.rs), the upstream hook files (src/cli/status.rs, src/client/*, src/remote/*, src/update.rs), scripts/mosh-diff-budget.sh and HERDR_MOSH_VISION.md were not reviewed
- Bounded review: Phase 1 and Phase 2 run documents other than the Phase 2 design and run.md, plus docs/superpowers/specs/INDEX.md, were not reviewed
- Raw immediate-END (end_now) path with data in flight is no longer covered by peer_end_drains_to_server_before_shutdown
- LAN dense-flood throughput, adaptive against the plain bridge, has never been measured on a landed commit
- Replay-phase throughput on high-BDP links (window frozen at INITIAL until the first fresh send) is untested
- Provenance gap: Phase 1 run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection
- Provenance gap: Phase 1 run.md:56 inv-ed080a9e-8cc4-431e-9718-215f84dd7131 incomplete claimed collection, incomplete claimed final report, invalid failed assembly archive (missing final-review-inv-ed080a9e-8cc4-431e-9718-215f84dd7131.md), incomplete assembled bundle (digest.md)
- A whole-range final review remains outstanding

## Deferred OK
- [hold-epoch-sampling] mosh-hex.2: the ACK that closes a hold epoch is sampled into it; conservative, no wrong decision
- [rounding-exceeds-window] mosh-hex.4: ring occupancy may exceed the window by under 4 KiB; bytes in flight never do; documented and tested
- [test-adapted-to-impl] mosh-hex.4: peer_end test switched to queue_final_end; coverage loss tracked under Untested scope
- [non-atomic-knob-write] mosh-hex.1: set_link truncates then writes; test-only flake risk
- [unmeasured-saturating-backlog] mosh-hex.3: measured on task-mosh-hex.5; closes when it lands; D3 pacing/SO_SNDBUF parked for the human in run.md
- [doc-drift] mosh-hex.3, [doc-formatting] mosh-hex.4, [missing-evidence] mosh-hex.2 and mosh-hex.4: report and doc hygiene
- keeper trace_window does FIONREAD and trace-file I/O under the session-table lock; only under HERDR_MOSH_TRACE, but it can perturb pacing-test timing
- Per-quantum coalescing over-reads RTT for sub-quantum trickles, which can cause a spurious shrink; the spec accepts this
