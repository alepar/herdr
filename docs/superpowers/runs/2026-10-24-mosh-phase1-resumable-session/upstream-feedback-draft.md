# mosh-phase1-resumable-session: super-code final-review and super-roast PR reports fail report-verdict --complete, degrading collection for the whole run

Plugin: superpowers 6.4.2-alepar6.6. Run: 34 beads (25 implementation + review/bookkeeping), 4 super-code invocations, 3 design + 4 code roast rounds, ~13 h wall, 2026-10-24.

## Defects
### 1. super-code final-review/cluster reports put findings under `###` headings, which `report-verdict --complete` rejects; review collection degrades for the rest of the run
- **Evidence:** friction "final-review-*.md and 4 cluster reports use ### finding headings; report-verdict --complete rejects them (empty finding section)"; parked "reviews/inv-c723b6b8 · degraded-verdict"; all 4 PR roasts "[low coverage] [review readiness incomplete] … provenance unavailable (inv-c723b6b8 incomplete claimed collection)"; the 4th invocation wrote no final-review file ("bundle unavailable").
- **Premise to verify:** the final-review/cluster prompt templates and the extractor grammar have drifted; no valid producer path emits list-marker findings for this template today.
- **Suggested fix shape:** one finding grammar enforced in both places, plus a contract test running every producer template's sample output through `report-verdict --complete`.

### 2. super-roast's own PR report fails `report-verdict --complete`, so `scope-dispositions --project` is unavailable to super-auto
- **Evidence:** friction "report-verdict --complete rejects the super-roast PR report ('finding text without a list marker in Unverified nits (spot-checked)') → finding-source/projection unavailable"; Metrics "repaired findings unavailable (use scope-dispositions --project)".
- **Premise to verify:** the "Unverified nits (spot-checked)" section is template-produced, not orchestrator-edited.
- **Suggested fix shape:** render that section as list-marked findings (or accept its format), covered by the same producer/extractor contract test.

### 3. Coordinator run-failure attribution diffs against the epic git base (already containing fork code) instead of the recorded run-start baseline
- **Evidence:** friction "coordinator run-failure attribution compared against base 992f6a45 (already contains fork code) instead of run-start baseline; caller re-attributed the 15th failure (client_mode idle-timeout) as run-introduced"; Recovery "same 15 fail at base 992f6a45 … run attribution: gap — open"; parked "tests/client_mode.rs … fails at head but passes at run-start baseline dec51b74".
- **Premise to verify:** a super-auto run-start baseline is reachable by super-code and currently ignored.
- **Suggested fix shape:** compare against the run-start baseline when recorded; fall back to the epic base only otherwise.

### 4. A no-change declined fix pass is counted FIXED, and the bead lands in both completed and parked
- **Evidence:** ledger `"id":"mosh-dqw.10" … "outcome":"FIXED" … "base":"541000cc…","head":"541000cc…","declined":true`; codeBuckets completed includes mosh-dqw.10 and "parked: mosh-dqw.10"; Metrics "fix-pass — entered 1 · FIXED 1".
- **Premise to verify:** declined + empty range means the fixer chose not to change code (not "already resolved").
- **Suggested fix shape:** a DECLINED/NO-CHANGE outcome reported separately; parked documented as a modifier or kept exclusive.

### 5. context-preflight warns of 200K windows while the run profile measures 1M windows for the same models
- **Evidence:** run.md "planner=200K/167K … warn: planner, finalReview on a 200K model"; profile "coordinator 599K/1000K (60%)" … "772K/1000K (77%)", "Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K".
- **Premise to verify:** preflight resolves windows from a stale alias table.
- **Suggested fix shape:** one shared model table for preflight and profiler.

### 6. tree-shape omits planner-added blocks edges, under-reporting depth and misnaming the critical path
- **Evidence:** Edge audit "tree-shape printed depth 2 with critical path 25 → 30, which omits the 30 <- 29 blocks edge that the bd dump shows"; Edge add "mosh-dqw.30 <- mosh-dqw.29 (planner) … tree-deps lists only mosh-dqw.25"; profile "graph lower bound 54m along mosh-dqw.25 → mosh-dqw.29 → mosh-dqw.30".
- **Premise to verify:** tree-shape reads a stale dependency snapshot rather than live bd edges.
- **Suggested fix shape:** build from live bd blocks edges or re-snapshot after each Edge add; assert edge counts match.

### 7. Re-entry planning scales with the cumulative epic, not the residual work
- **Evidence:** profiles "dispatched 5, landed 5, planned 16 … planning 10m (24%)", "dispatched 6 … planned 23 … planning 15m (28%)", "dispatched 2 … planned 25 … planning 6m (37%)".
- **Premise to verify:** "planned N" counts beads actually processed and re-planning closed beads is avoidable.
- **Suggested fix shape:** scope re-entry planning to open beads plus direct closed deps; report residual vs cumulative.

## Run metrics
### Judge panel
- 2026-10-24-mosh-phase1-resumable-session-roast-design-1.md: mode: design        iteration: 1 of 3 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 34 · rr 0.59 · rg 0.82 · fg 0.47 · unanimous 0.44 · ground-loo 0.75 (n=20) · reproduce 23/11/0 · refute 9/25/0 · ground 26/7/1
- 2026-10-24-mosh-phase1-resumable-session-roast-design-2.md: mode: design        iteration: 2 of 4 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 10 · rr 0.60 · rg 0.90 · fg 0.50 · unanimous 0.50 · ground-loo 0.83 (n=6) · reproduce 8/2/0 · refute 4/6/0 · ground 9/1/0
- 2026-10-24-mosh-phase1-resumable-session-roast-design-3.md: mode: design        iteration: 3 of 4 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 4 · rr 0.75 · rg 0.75 · fg 1.00 · unanimous 0.75 · ground-loo 1.00 (n=3) · reproduce 3/1/0 · refute 2/2/0 · ground 2/2/0
- 2026-10-24-mosh-phase1-resumable-session-roast-pr-1.md: mode: pr        iteration: 1 of 3 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 29 · rr 0.90 · rg 0.86 · fg 0.83 · unanimous 0.79 · ground-loo 0.88 (n=26) · reproduce 17/11/1 · refute 16/12/1 · ground 19/9/1
- 2026-10-24-mosh-phase1-resumable-session-roast-pr-2.md: mode: pr        iteration: 2 of 3 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 3 · rr 1.00 · rg 1.00 · fg 1.00 · unanimous 1.00 · ground-loo 1.00 (n=3) · reproduce 3/0/0 · refute 3/0/0 · ground 3/0/0
- 2026-10-24-mosh-phase1-resumable-session-roast-pr-3.md: mode: pr        iteration: 3 of 4 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 4 · rr 1.00 · rg 0.75 · fg 0.75 · unanimous 0.75 · ground-loo 0.75 (n=4) · reproduce 3/1/0 · refute 3/1/0 · ground 4/0/0
- 2026-10-24-mosh-phase1-resumable-session-roast-pr-4.md: mode: pr        iteration: 4 of 4 · independence: same-family (Claude) — seat-differentiated panel · rung: Workflow · seat-agreement: panels 4 · rr 0.75 · rg 1.00 · fg 0.75 · unanimous 0.75 · ground-loo 1.00 (n=3) · reproduce 3/1/0 · refute 4/0/0 · ground 3/1/0
### Fix loop
Metrics: completions — review clean 22 · after fix pass 2 · parked 1 · re-entry closes 0 · dispatched early 7 · cancelled 0
Metrics: fix-pass — entered 2 · FIXED 2 · BLOCKED 0 (legacy event lines); durable attempts 2 · historical FIXED 2; repaired findings unavailable (use scope-dispositions --project)
### Merge-back
Metrics: merges 24 · merge-failed 0 · rebase-conflicts 0 · seam-reviews 2 (fixed 0) · check-fails 0 (fixed 0)
Metrics: ledger-check ok · append-failed 0 · append-retried 0 · scope: observed ledger; completed after available tracker reconciliation · git-only [] · ledger-only-git [] · gaps: none in observed sources
- Merge: lines: 24 total · conflict 0 · seam-review fired 2 · check fail→fixed 0 · → blocker 0
### Coverage
- coverage-round-1 · canonical R-list: R1..R15 in coverage-round-1-requirements.md (+R16-R18 from r-new) · requirements: 15 · mapped: 14 · unmapped: 3 (R2, R10, R11) · auto flag-sweep mosh-dqw.2/.3/.8 → kept; auto GAP R2,R3,R9,R10,R11,R12 → applied (summary lines; scope already in bead bodies); auto UNOWNED-SEAM keeper rendezvous → owner mosh-dqw.1 (+spec lock fix); auto UNOWNED-SEAM argv flags/capability/MoshConfig → consumes lines; auto UNOWNED-SEAM link-frame format → owner mosh-dqw.2, edge mosh-dqw.5<-mosh-dqw.2; auto rejected OutageState, Leg-keeper, .8 breadth
- coverage-round-2 · requirements: 18 · mapped: 18 · unmapped: 0 · divergence: findings 17 → 14, novel 79%, widening: no · auto GAP R18,R11,R17,R16,keeper-lifetime(R19),R2 → applied; auto UNOWNED-SEAM keeper entry point, attacher preamble, capability format, injection encoding consumer, MoshConfig test override → applied; auto rejected touch-list seam, mosh-tree seam; auto ORPHAN mosh-dqw.3 → root goal element added
scope-filter: 2 in-scope · 1 punch-listed
### Timing
Profile: mosh-dqw · 1 invocation · 2026-10-24T06:51Z → 2026-10-24T09:50Z · wall 2h59m (task graph 2h41m, after the last landing 18m) · agents 91 (timed 91) · beads dispatched 11, landed 11, planned 11
Profile: bound — graph lower bound 2h41m along mosh-dqw.4 → mosh-dqw.8 → mosh-dqw.9 → mosh-dqw.11 (unlimited slots and merge lanes) · model of the run 2h41m (cap 14, one merge lane) vs actual 2h41m (100%) → graph-bound: the 4-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 2h41m = implement 2h07m · review 29s · merge 41s · planning 33m · ready 5s · startup 19s · dispatch 18s
Profile: bottleneck — mosh-dqw.9 — 1h27m on the critical path (54%) · implement 1h27m · dispatch 2s · implement on the path 1h27m: test 46m · poll 13m · build 43s · git 2s · read 30s · shell 8m · model 19m (longest: "cat >> tests/mosh_resume.rs <<'EOF'" 9m, "python3 - <<'EOF'" 9m) · dependents 1
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 11 landed beads
Profile: merge lane — busy 8m of 2h26m (6%) · seam reviews and fixes 0s (0% of busy) · 11 merge dispatches, first try not merged for 0 of 11 · queue wait 24s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — faster mosh-dqw.9 → 1h58m (−44m, 27%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — cut mosh-dqw.11 <- mosh-dqw.9 → 2h30m (−12m, 7%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-dqw.9 → 2h30m (−12m, 7%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — cut mosh-dqw.9 <- mosh-dqw.8 → 2h30m (−11m, 7%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-dqw.8 → 2h30m (−11m, 7%) — its 2 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-dqw.8 → 2h31m (−10m, 6%) — its implement time halved; first dispatch to last landing, planning retained
Profile: context — agents 92 (measured 92) · peak≥80% 0 · compacted 0 agents, 0 compactions · coordinator 599K/1000K (60%), 0 compactions · max coordinator 60%
Profile: context agent — coordinator peak 599K/1000K (60%) · first 164K · compactions 0
Profile: unmeasured — Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K
Profile: mosh-dqw · 1 invocation · 2026-10-24T10:24Z → 2026-10-24T11:17Z · wall 54m (task graph 43m, after the last landing 11m) · agents 46 (timed 46) · beads dispatched 5, landed 5, planned 16
Profile: bound — graph lower bound 43m (unlimited slots and merge lanes) · model of the run 43m (cap 14, one merge lane) vs actual 43m (100%) → bead-bound: mosh-dqw.19 alone set the pace — split it or make it faster
Profile: critical path — 43m = implement 30m · review 26s · merge 46s · planning 10m · ready 6s · startup 55s · dispatch 10s
Profile: bottleneck — mosh-dqw.19 — 32m on the critical path (74%) · implement 30m · review 26s · merge 46s · dispatch 8s · implement on the path 30m: test 7m · poll 19m · build 57s · read 16s · shell 5s · model 3m (longest: "for i in $(seq 55)" 2m, "for i in $(seq 23)" 2m) · dependents 0
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 5 landed beads
Profile: merge lane — busy 5m of 30m (16%) · seam reviews and fixes 1m (22% of busy) · 7 merge dispatches, first try not merged for 2 of 5 · queue wait 2m total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — faster mosh-dqw.19 → 28m (−15m, 35%) — its implement time halved; first dispatch to last landing, planning retained
Profile: context — agents 47 (measured 47) · peak≥80% 0 · compacted 0 agents, 0 compactions · coordinator 666K/1000K (67%), 0 compactions · max coordinator 67%
Profile: context agent — coordinator peak 666K/1000K (67%) · first 164K · compactions 0
Profile: unmeasured — Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K
Profile: mosh-dqw · 1 invocation · 2026-10-24T11:44Z → 2026-10-24T12:49Z · wall 1h05m (task graph 54m, after the last landing 11m) · agents 59 (timed 59) · beads dispatched 6, landed 6, planned 23
Profile: bound — graph lower bound 54m along mosh-dqw.25 → mosh-dqw.29 → mosh-dqw.30 (unlimited slots and merge lanes) · model of the run 54m (cap 14, one merge lane) vs actual 54m (99%) → graph-bound: the 3-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 54m = implement 36m · review 28s · merge 32s · planning 15m · ready 5s · startup 1m · dispatch 34s
Profile: bottleneck — mosh-dqw.29 — 16m on the critical path (29%) · implement 15m · dispatch 4s · implement on the path 15m: test 7m · build 1m · git 1s · read 9s · shell 11s · model 7m (longest: "cargo nextest run --locked -E 'test(mosh::)' 2>" 2m, "grep -n -A6 "^lint" justfile" 2m) · dependents 1 direct, 1 transitive
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 6 landed beads
Profile: merge lane — busy 4m of 33m (12%) · seam reviews and fixes 0s (0% of busy) · 6 merge dispatches, first try not merged for 0 of 6 · queue wait 11s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — cut mosh-dqw.29 <- mosh-dqw.25 → 45m (−9m, 17%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-dqw.25 → 45m (−9m, 16%) — its 2 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-dqw.29 → 46m (−8m, 14%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-dqw.25 → 48m (−6m, 11%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — cut mosh-dqw.30 <- mosh-dqw.29 → 50m (−4m, 7%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-dqw.29 → 50m (−4m, 7%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: context — agents 60 (measured 60) · peak≥80% 0 · compacted 0 agents, 0 compactions · coordinator 742K/1000K (74%), 0 compactions · max coordinator 74%
Profile: context agent — coordinator peak 742K/1000K (74%) · first 164K · compactions 0
Profile: unmeasured — Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K
Profile: mosh-dqw · 1 invocation · 2026-10-24T13:07Z → 2026-10-24T13:30Z · wall 23m (task graph 17m, after the last landing 6m) · agents 26 (timed 26) · beads dispatched 2, landed 2, planned 25
Profile: bound — graph lower bound 17m (unlimited slots and merge lanes) · model of the run 17m (cap 14, one merge lane) vs actual 17m (99%) → bead-bound: mosh-dqw.33 alone set the pace — split it or make it faster
Profile: critical path — 17m = implement 8m · review 27s · merge 45s · planning 6m · ready 7s · startup 1m · dispatch 8s
Profile: bottleneck — mosh-dqw.33 — 9m on the critical path (54%) · implement 8m · review 27s · merge 45s · dispatch 6s · implement on the path 8m: test 7m · git 1s · read 3s · shell 5s · model 1m (longest: "for i in 1 2 3 4 5" 4m, "mkdir -p $TMPDIR" 2m) · dependents 0
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 2 landed beads
Profile: merge lane — busy 1m of 5m (29%) · seam reviews and fixes 0s (0% of busy) · 2 merge dispatches, first try not merged for 0 of 2 · queue wait 4s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — faster mosh-dqw.33 → 13m (−3m, 21%) — its implement time halved; first dispatch to last landing, planning retained
Profile: context — agents 27 (measured 27) · peak≥80% 0 · compacted 0 agents, 0 compactions · coordinator 772K/1000K (77%), 0 compactions · max coordinator 77%
Profile: context agent — coordinator peak 772K/1000K (77%) · first 164K · compactions 0
Profile: unmeasured — Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K

### Bead graph
| id | type | title | what (first sentence of description) |
| --- | --- | --- | --- |
| mosh-dqw | epic | Mosh Phase 1: resumable remote session | Mosh Phase 1: link-level resumable remote session (local bridge + remote keeper over SSH) with outage banner. Spec: docs |
| mosh-dqw.1 | task | Seam contract: mosh module scaffold and leg interface | Inert compilable scaffold: mount line, mosh constants/MoshConfig (kill switch, grace), Leg/LegDialer/LegError, start_ui_ |
| mosh-dqw.10 | task | Remote attach path: attacher, dispatch and status capability hooks | Remote attach path (R1, R12, R16): attacher that validates the private rendezvous dir, finds or spawns the keeper, check |
| mosh-dqw.11 | task | Integration sweep: resumable remote session end to end | Unknown-unknowns net for the whole tree (spec Goal): verify --remote and saved-machine resume end to end on the combined |
| mosh-dqw.12 | task | review: mosh-dqw.5 | Review, fix and merge of mosh-dqw.5, tracked separately so mosh-dqw.5's dependents can start once it is implemented. The |
| mosh-dqw.13 | task | review: mosh-dqw.6 | Review, fix and merge of mosh-dqw.6, tracked separately so mosh-dqw.6's dependents can start once it is implemented. The |
| mosh-dqw.14 | task | review: mosh-dqw.4 | Review, fix and merge of mosh-dqw.4, tracked separately so mosh-dqw.4's dependents can start once it is implemented. The |
| mosh-dqw.15 | task | review: mosh-dqw.10 | Review, fix and merge of mosh-dqw.10, tracked separately so mosh-dqw.10's dependents can start once it is implemented. T |
| mosh-dqw.16 | task | review: mosh-dqw.7 | Review, fix and merge of mosh-dqw.7, tracked separately so mosh-dqw.7's dependents can start once it is implemented. The |
| mosh-dqw.17 | task | review: mosh-dqw.8 | Review, fix and merge of mosh-dqw.8, tracked separately so mosh-dqw.8's dependents can start once it is implemented. The |
| mosh-dqw.18 | task | review: mosh-dqw.9 | Review, fix and merge of mosh-dqw.9, tracked separately so mosh-dqw.9's dependents can start once it is implemented. The |
| mosh-dqw.19 | task | Fix: retain session-final END and flush final ACK across leg loss | Covers: [Should-fix] src/mosh/local_bridge.rs:986, [Should-fix] src/mosh/remote/keeper.rs:1202; src/mosh/remote/keeper.r |
| mosh-dqw.2 | task | Outage banner: LinkStatus, OutageState and client hooks | Outage banner (R6): the x-mosh.link.v1 kind and LinkStatus payload, per-endpoint OutageState, and the client observe/pai |
| mosh-dqw.20 | task | Fix: atomic on_leg_bytes commit (no dropped Deliver on protocol error) | Covers: [Should-fix] src/mosh/session.rs:507, [Should-fix] src/mosh/session.rs:1581, final-review Must-fix session.rs:49 |
| mosh-dqw.21 | task | Fix: saved-machine resume leg carries upstream --idle-timeout-v1 opt-i | Covers: [Should-fix] src/mosh/mod.rs:153, [Should-fix] src/mosh/ssh_leg.rs:649, final-review Must-fix mod.rs start_ui_br |
| mosh-dqw.22 | task | Fix: isolate mosh unit tests from the shared rendezvous dir and anchor | Covers: [Should-fix] src/mosh/remote/attacher.rs:430, [Should-fix] src/mosh/local_bridge.rs:2036, [Nit] tests/mosh_resum |
| mosh-dqw.23 | task | Fix: record implementation divergences in the design spec's Post-Imple | Covers: final-review Must-fix 'design spec Post-Implementation Notes — undocumented divergences' |
| mosh-dqw.24 | task | Bridge Closing phase: single drain-then-close with one deadline | Covers: [Should-fix] src/mosh/local_bridge.rs:2266 |
| mosh-dqw.25 | task | Keeper Closing phase: drain to_server before shutdown, one removal pre | Covers: [Nit] src/mosh/remote/keeper.rs:198 |
| mosh-dqw.26 | task | Reconnect-leg spawn error is Retryable | Covers: [Must-fix] src/mosh/ssh_leg.rs dial_command — reconnect-leg spawn_leg error classified Fatal instead of Retryabl |
| mosh-dqw.27 | task | on_leg_bytes caller coverage: DATA plus a bad message in one write | Covers: [Should-fix] src/mosh/local_bridge.rs:606 |
| mosh-dqw.28 | task | Test isolation and causality sweep (C4) | Covers: [Nit] src/mosh/local_bridge.rs:1899, [Should-fix] tests/mosh_fallback.rs:156, [Should-fix] tests/mosh_harness.rs |
| mosh-dqw.29 | task | Bridge Closing phase and bounded leg reap (local_bridge.rs, ssh_leg.rs | Split from mosh-dqw.24: This one bead rewrites all of the bridge's teardown (Closing state, ~400 production lines in loc |
| mosh-dqw.3 | task | Fork release hygiene: update manifest override and diff budget | Fork release hygiene (R14, R15): build-time update-manifest URL overrides and the diff-budget audit script. |
| mosh-dqw.30 | task | Cross-endpoint closing-order test (tests/mosh_integration.rs; after mo | Split from mosh-dqw.24: This one bead rewrites all of the bridge's teardown (Closing state, ~400 production lines in loc |
| mosh-dqw.31 | task | review: mosh-dqw.25 | Review, fix and merge of mosh-dqw.25, tracked separately so mosh-dqw.25's dependents can start once it is implemented. T |
| mosh-dqw.32 | task | review: mosh-dqw.29 | Review, fix and merge of mosh-dqw.29, tracked separately so mosh-dqw.29's dependents can start once it is implemented. T |
| mosh-dqw.33 | task | Fix: Closing waits exit when the worker thread behind them has died | Covers: [Should-fix] src/mosh/local_bridge.rs:1195, final-review Must-fix src/mosh/local_bridge.rs tick_closing Phase B  |
| mosh-dqw.34 | task | Fix: mosh_integration closing-order test asserts only what the spec gu | Covers: [Should-fix] tests/mosh_integration.rs:169 |
| mosh-dqw.4 | task | Session protocol: SessionMsg, SessionEndpoint and ReplayRing | Sans-IO session protocol (R3, R4): message codec, per-direction offsets with exactly-once dedup, the one flow-control ru |
| mosh-dqw.5 | task | Frame splitter and link-frame injection encoder | Whole-frame splitter and x-mosh.link.v1 injection-frame encoder (R5, R6), so the client never sees a torn frame and stat |
| mosh-dqw.6 | task | SSH leg dialer: probe, re-selection, leg commands and failure classes | SSH leg dialer (R1, R2, R12, R13): capability probe and candidate re-selection, leg-0 vs reconnect-leg commands (reconne |
| mosh-dqw.7 | task | Keeper daemon: parked server sessions, drain and grace | Remote keeper daemon (R1-R5, R7-R10, R16): parks the server connection per token across legs, drains while down, enforce |
| mosh-dqw.8 | task | Local bridge: resumable client pump and start_ui_bridge resume path | Local bridge (R1-R7, R9, R10, R12): per-client resumable pump (leg I/O thread plus a separate client-writer thread, pre- |
| mosh-dqw.9 | task | Configuration smoke: end-to-end resume tests over fake ssh | End-to-end configuration smoke (R1-R13, R16, R17, R19): resume, stall, saved-machine health, grace expiry, multi-client, |

| dependent | blocker | reason (from `blocked-by` line, or `unstated`) |
| --- | --- | --- |
| mosh-dqw.10 | mosh-dqw.1 | consumes keeper rendezvous protocol and mosh module tree |
| mosh-dqw.10 | mosh-dqw.4 | consumes SessionMsg (END{keeper-unavailable} encoding) |
| mosh-dqw.11 | mosh-dqw.5 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.3 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.1 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.4 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.10 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.6 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.9 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.2 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.7 | consumes all leaves (integration sweep) |
| mosh-dqw.11 | mosh-dqw.8 | consumes all leaves (integration sweep) |
| mosh-dqw.12 | mosh-dqw.5 | unstated |
| mosh-dqw.13 | mosh-dqw.6 | unstated |
| mosh-dqw.14 | mosh-dqw.4 | unstated |
| mosh-dqw.15 | mosh-dqw.10 | unstated |
| mosh-dqw.16 | mosh-dqw.7 | unstated |
| mosh-dqw.17 | mosh-dqw.8 | unstated |
| mosh-dqw.18 | mosh-dqw.9 | unstated |
| mosh-dqw.24 | mosh-dqw.25 | the required cross-endpoint test (client-gone with queued client bytes, then the server re |
| mosh-dqw.29 | mosh-dqw.25 | the required cross-endpoint test (client-gone with queued client bytes, then the server re |
| mosh-dqw.30 | mosh-dqw.25 | the required cross-endpoint test (client-gone with queued client bytes, then the server re |
| mosh-dqw.30 | mosh-dqw.29 | the end-to-end test (client-gone with queued client bytes reaches the server, then the ses |
| mosh-dqw.31 | mosh-dqw.25 | unstated |
| mosh-dqw.32 | mosh-dqw.29 | unstated |
| mosh-dqw.4 | mosh-dqw.1 | consumes mosh module tree |
| mosh-dqw.5 | mosh-dqw.2 | consumes LINK_KIND |
| mosh-dqw.5 | mosh-dqw.1 | consumes mosh module tree |
| mosh-dqw.6 | mosh-dqw.1 | consumes Leg and LegDialer interface |
| mosh-dqw.7 | mosh-dqw.4 | consumes SessionEndpoint and ReplayRing |
| mosh-dqw.8 | mosh-dqw.5 | consumes FrameSplitter and encode_link_frame |
| mosh-dqw.8 | mosh-dqw.4 | consumes SessionEndpoint |
| mosh-dqw.8 | mosh-dqw.2 | consumes LinkStatus payload type |
| mosh-dqw.8 | mosh-dqw.6 | consumes SshLegDialer and capability probe |
| mosh-dqw.9 | mosh-dqw.10 | consumes attacher, dispatch and status capability |
| mosh-dqw.9 | mosh-dqw.7 | consumes Keeper |
| mosh-dqw.9 | mosh-dqw.8 | consumes LocalBridge resume path |

## Design questions
### Should the step-back be mandatory at the code-roast cap round?
- **Evidence:** run.md "stepBackCode-round-4: local — cap round … (step-back not dispatched; see friction log)"; "roastCodeExit: capped".
- For mandatory: a shared cause is exactly what a capped exit hides, and the punch list inherits it. Against: no further round can act on a redesign verdict at the cap unless the skill raises the cap, so the dispatch has no consumer.

If upstream decides otherwise, please state the position explicitly so downstream can reconcile against words rather than silence.

## Doc gaps
### 1. Implementers background short test runs and lose time polling
- **Evidence:** profile "1h07m running tests and 18m polling background runs, 51% of their 2h47m · 0 Bash call(s) ran into the 10-minute tool limit"; "24m running tests and 22m polling background runs, 81% of their 58m".
- **Suggested fix shape:** implementer/fixer prompts: foreground with explicit timeout when under the tool limit.
### 2. No guidance for repos whose run dir is gitignored or that lack a tracker
- **Evidence:** friction "bd init --stealth", ".worktrees/ … via .git/info/exclude", "docs/* is upstream-gitignored; run-directory files are force-added (git add -f) on every commit", ".superpowers/ … excluded via .git/info/exclude".
- **Suggested fix shape:** pre-flight detects ignored run dir/missing tracker, documents the info/exclude + stealth option; helpers force-add run files.
### 3. super-auto's round-1 design-roast invocation can omit `--review-readiness`
- **Evidence:** friction "round 1 roast ran without --review-readiness (caller omission) → [low coverage] qualifier"; roast-design-1 "review readiness: coverage unknown, provenance unknown — basis: \"\"".
- **Suggested fix shape:** canonical invocation carries `--review-readiness` every round, or super-roast flags its absence for super-auto callers.

## Already fixed — do not re-litigate
none

## Not established
- By default: the coordinator's ledger-append path is fire-and-forget and can lose a line without the coordinator noticing, so every ledger-derived count in `## Run metrics` (Fix loop, Merge-back) is a lower bound. The `Metrics: ledger-check` line is the one cross-check that exists.
- `scope-dispositions --project` projection unavailable (defects 1–2): no actionable-group counts or repaired-finding counts.
- `already-merged --landed` reconciliation not run; Merge-back counts come from ledger lines only.
- Coverage round `requirements:` lines are recorded in run.md approvals text, not as standalone lines.
- Analyst model: opus. Dropped by triage: graph-shape empty for a finished run (profile carries the graph), missing project toolchain at pre-flight (project-specific), remainder-capped carry-forward (speculative).
- Single-run observations; no cross-run comparison.

## Verification bar
- Run `report-verdict --complete` over fixture outputs of every producer template (super-code final review, cluster reports, super-roast PR/design reports) in CI.
- Replay a super-auto run on a repo with an existing failing test at run start and a run-introduced regression; check attribution names the run owner.
- Re-run tree-shape after an Edge add and compare against `bd list --json` edge count.

---
If a premise above is wrong, stop and say so rather than improvising a larger change.
