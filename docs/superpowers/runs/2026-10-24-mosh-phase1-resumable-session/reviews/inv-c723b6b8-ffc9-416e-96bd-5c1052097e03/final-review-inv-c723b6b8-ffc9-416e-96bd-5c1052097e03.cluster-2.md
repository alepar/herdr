# Final review — cluster 2 (run-docs-root) — epic mosh-dqw

Invocation: inv-c723b6b8-ffc9-416e-96bd-5c1052097e03
Source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`). Review-source binding recorded in the ledger (dispatch-text.mjs bind-review-source, exit 0) before this file was written.
Caller checkout observed: HEAD = 2bd5303c, working tree clean at review start. Nothing in the caller checkout was switched or edited.

## Scope

The 21 files of `docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/` listed for cluster `run-docs-root` (slices 000002-000018, 000020-000022, 000029): the design spec (the epic's authoritative spec per `bd show mosh-dqw`), the two design roasts and the step-back, amend-design-1, the coverage rounds and ledger, the design findings ledger and overrides, friction.md and run.md. Source files were opened only to check named risks: the upstream hook lines (`git diff` over the touched upstream files), `src/mosh/local_bridge.rs` (resume trace), `src/mosh/remote/keeper.rs` (server reader), `src/mosh/mod.rs` / `src/mosh/ssh_leg.rs` (HERDR_REMOTE_BINARY hint).

## What holds

- The design spec is internally coherent after redesign cycle 1 and the round-2 punch list. All eight round-2 Should-fix/Nit findings are visibly applied: the 20 s pre-WELCOME bound and reconnect-leg Retryable class 4, the 2 MiB WINDOW with OpenSSH parity, session-final END ordering after ring bytes, no `LC_ALL` on the leg Command, END on `should_stop`, the leg-0 fallback "accepted limitation", and the idle-exit race resolution through existing paths.
- The spec's footprint claim is reconciled: the Post-Implementation Note (mosh-dqw.10) states 12 upstream lines, and the actual upstream diff has exactly 12 inserted hook lines (attach.rs 2, saved.rs 1, host.rs 2, status.rs 2, update.rs 2, client/mod.rs 2, composition.rs 1), each tagged `// mosh-fork hook`.
- Requirements R1-R20 (coverage-round-2-requirements.md) trace to the spec's acceptance criteria; coverage-ledger.md C1-C44 records a disposition for every gap and seam raised.
- run.md records both roast rounds, the redesign cycle, convergence at round 2 (0 Blocking), approvals and the parked escalation. friction.md records the environment workarounds (stealth beads, `.git/info/exclude`, `git add -f`, zig install, pre-existing baseline failures).
- The HERDR_REMOTE_BINARY hint deferred from mosh-dqw.6 is printed by the caller (`src/mosh/mod.rs:126`), as the spec's Discovery step 3 requires.

## Findings

1. Post-Implementation Notes omit known spec divergences (Important). The spec instructs: "append a dated note here [for] anything that diverged from the assumptions above". Only the 12-line footprint note was added. Divergences disclosed in task reports and the deferred-minor ledger, and confirmed where checked, are not recorded, so the epic's spec still asserts behaviour the code does not have:
   - §Ring caps point 4/5 says keeper ring overflow during an outage ends the session and closes the server stream. `keeper.rs` `server_reader` gates on `may_read_source()` (below cap while down), so a ring that reaches exactly 16 MiB back-pressures the server until grace expiry instead of overflowing (mosh-dqw.7 deferred `ring-cap-edge`). The local side was changed the opposite way (client reader always fed while down, mosh-dqw.8 report) so it does overflow; the asymmetry is undocumented in the spec.
   - §Observability lists `resume{replayed_up, replayed_down}`; `local_bridge.rs:683` always emits `"replayed_down": null` (mosh-dqw.8 `doc-drift`).
   - §Legs "Failure classification" has no class for an ssh spawn error; `dial_command` classifies it `Fatal` even on a reconnect leg (mosh-dqw.6 `error-classification`), so a transient EAGAIN ends a resumable session.
   - Acceptance "Client exit ends its keeper session within 2 s when the leg is alive": the mosh-dqw.9 product observation (graceful `--remote` quit racing a server hang-up leaves the keeper session parked until grace) is a known exception that the spec does not mention.
2. Stale spec text (Minor). §Components table names the outage unit `src/client/mosh_outage.rs`; the file is `src/mosh/outage.rs` mounted by `#[path]` (as §Outage banner "Mount" says). §Outage banner Hook 2 gives two different `paint` signatures in consecutive lines (2-arg, then 3-arg with `endpoint_label`); the code uses the 3-arg form. §Upstream touch list still totals 11, and the Goal/§Unchanged behavior/acceptance say "about 11"; only the appended note says 12.
3. findings-ledger-design.md is stale (Minor). Its header says "through r1" and it lists the r1 Blocking finding and most Should-fix items as `open`, although run.md records round 2 as converged with 21 resolved and 0 Blocking. A reader of the committed ledger alone sees an open Blocking design finding.
4. Acceptance checklist unticked (Minor). Every acceptance criterion in the spec is still `- [ ]`, with no mapping to the tests that pin it; traceability lives only in task reports and the deferred-minor ledger.
5. Environment paths in committed run docs (Minor). run.md and roast-design-2.md embed absolute local paths (`/tmp/claude-1001/...` scratchpad inputs, `/var/home/alepar/...` worktree). The scratchpad inputs were copied into `inputs/`, so nothing is lost, but the references point at ephemeral locations.

## Verdict
not ready (spec Post-Implementation Notes omit four known divergences: keeper exact-cap back-pressure, replayed_down null, Fatal ssh spawn on reconnect, graceful-quit session parked until grace)

## Must fix before landing
- Append dated Post-Implementation Notes to 2026-10-24-mosh-phase1-resumable-session-design.md recording the keeper exact-16-MiB back-pressure (vs §Ring caps point 4/5 overflow), `resume.replayed_down` emitted as null, reconnect-leg ssh spawn failure classified Fatal, and the graceful `--remote` quit / server hang-up race that parks the keeper session until grace, or fix the code so the spec holds

## Untested scope
- Acceptance "16 MiB clipboard image while the leg is up completes" and "in-flight never exceeds 2 MiB" are evidenced only by 16x1 MiB pastes plus unit tests (mosh-dqw.9 deferred), which the spec does not acknowledge
- Acceptance "no ClientShellSnapshot and no full PaneSurface after resume" is evidenced only by the server_connect-count proxy that §Observability describes; no client-side frame assertion
- run.md parked escalation: leg-0 ssh stderr diagnostics printing over the TUI during an outage is unverified and still needs the human ssh check it names
- Windows remote / non-Unix local gate in the spec's acceptance is inspection-only (no cross-compile), as recorded in the deferred minors

## Deferred OK
- Stale §Components path `src/client/mosh_outage.rs`, the duplicated Hook 2 `paint` signature and the touch-list "Total 11" text (the appended note already states 12)
- findings-ledger-design.md left at "through r1" with r1 items marked open; run.md records the round-2 convergence
- Spec acceptance checklist left unticked with no criterion-to-test mapping
- Absolute local scratchpad and worktree paths embedded in run.md and roast-design-2.md
