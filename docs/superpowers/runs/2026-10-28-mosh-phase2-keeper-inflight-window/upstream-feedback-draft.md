# 2026-10-28-mosh-phase2-keeper-inflight-window: prior-run review provenance gaps leave every later review bounded

Plugin: 6.4.2-alepar6.6. Run: 18 beads (12 implementation/fix + 6 review), 6 super-code invocations, 2 design + 3 code roast rounds, about 9 h wall, 2026-10-28.

## Defects
### 1. Review-input provenance from a prior run's incomplete collections poisons every later review on the same workspace
- **Evidence:** friction: "assembler: review input selection: provenance unavailable; captured source available (inherited Phase 1 incomplete collections inv-c723b6b8, inv-ed080a9e)"; "round 3 assembler skipped as known-unavailable (same inherited provenance gap twice)"; all three roast-pr verdicts carry "[low coverage] [review readiness incomplete]"; all 6 final reviews "not ready (unreviewed: whole-epic coverage and omission provenance remain incomplete)"; no whole-range review exists for this run.
- **Premise to verify:** the inherited collections are selected because the prior run's run.md/archives sit inside the new run's captured base..head (both runs merged into the same base branch), not because this run depends on them.
- **Suggested fix shape:** scope review-input provenance to the current run's own invocations (or let a run record a prior run's incomplete collections as inherited-and-accepted at launch) so one bad run does not degrade every later run cut from the same base.

### 2. scope-dispositions --project counts only final-review sources, never groups one finding across invocations, and its --keys view is empty while punch-listed keys exist
- **Evidence:** projection: "sources": 6, "sourceKinds": ["final-review"], "occurrences": 158, "occurrenceGroups": 158, "repaired": 0, "outstanding": 158, while fix beads mosh-hex.12/.13/.15/.16/.17 landed review-clean; the same human-decision Must-fix recurs in inv-d659cb48, inv-d1b970ad, inv-caeccbf2 yet groups == occurrences; every Metrics line says "repaired findings unavailable (use scope-dispositions --project)"; friction: "scope-dispositions --project --keys returned an empty projection although round-1 punch-listed keys exist".
- **Premise to verify:** roast reports and fix-bead closures are intended projection sources, and repeated final-review findings carry a stable key to group on (distinct from super-plan-2s3, which is about report-format extraction).
- **Suggested fix shape:** ingest roast and fix-bead dispositions, group occurrences by finding key across sources, and make --keys return the punch-list the run actually recorded.

### 3. Implementer commits carry AI Co-Authored-By trailers that the project's instructions forbid; review grades it minor and history must be rewritten after the fact
- **Evidence:** friction: "implementer commits b34b4f85, bf412b27, c2705d1b, 952fcdfd carry Co-Authored-By lines despite AGENTS.md (no AI co-author lines); the global constraint in the epic did not reach implementer commit habits"; ledger "Task 6 (mosh-hex.6): minor [attribution-line]"; friction: "stripped Co-Authored-By trailers from 6 run commits by recreating mosh..HEAD with git commit-tree (filter-branch unavailable)".
- **Premise to verify:** the trailer comes from a harness attribution reminder implementers follow by default, and the epic Global Constraints do not reach implementer prompts in a form that overrides it.
- **Suggested fix shape:** pass the project's commit-message policy into implementer and merge prompts explicitly, and have the merge step lint commit messages so violations are caught before landing.

### 4. A re-dispatched bead reuses its old task branch stacked on a stale pre-review parent and fails setup
- **Evidence:** ledger: "Task 5 (mosh-hex.5): pending retry [cause: reused task branch stacked on a stale pre-review parent commit]"; profile: "rework — 2m ... mosh-hex.5 2m (SETUP_FAILED; landed later)".
- **Premise to verify:** branch reuse on re-dispatch is intended, and the stack parent (mosh-hex.9) changed between the block and the re-dispatch.
- **Suggested fix shape:** on re-dispatch of a blocked or failed bead, re-derive its branch from the current stack parent or integration head (or rebase it) before handing it to the implementer.

### 5. Cumulative final reviews re-raise already-parked human escalations as fresh Must-fix items
- **Evidence:** parked: "final-review-inv-d659cb48 · escalation · Must-fix: human decision to accept closing Phase 2 on the amended bounds"; inv-d1b970ad and inv-caeccbf2 both open "Must fix before landing … Human decision: accept closing Phase 2 on the amended bounds (Ctrl-C <= 2.5 s, goodput >= 0.6 R)".
- **Premise to verify:** final-review prompts receive no list of parked/escalated items.
- **Suggested fix shape:** pass parked escalations into the final-review input so reviewers mark them already escalated instead of minting new Must-fix occurrences (also shrinks defect 2's double counting).

### 6. Process-sweep cleanup exits 4 and retains task worktrees because of uninspectable host processes the run does not own
- **Evidence:** run.md: "worktreesKept: .worktrees/ task worktrees mosh-hex.1-.5 and /tmp/sp-fd54fa48 — process cleanup incomplete (exit 4: uninspectable sshd-session pids, not run-owned)"; "processSweep: stopped 0 · survived 0 · unknown 2 · kept 0 — unknown: sshd-session 1097853, 403694 (LAN client sessions, cwd/exe inspection failed)". Same pids blocked cleanup in the prior run.
- **Premise to verify:** the ownership test can distinguish uninspectable processes that are not descendants of run agents from run-owned ones.
- **Suggested fix shape:** treat uninspectable processes that are not run descendants as not run-owned; remove the worktree and record a warning instead of blocking cleanup.

### 7. Run bookkeeping cannot represent a bead filed mid-run and prints '?' for its task index and outcome
- **Evidence:** ledger: "Task ? (mosh-hex.9): BLOCKED [cause: ...]", later "Task 7 (mosh-hex.9)"; profile: "rework — 1m ... mosh-hex.9 1m (?; never landed)", "availability/readiness timing unknown for 1 bead(s)".
- **Premise to verify:** task numbering and outcome classification are assigned at plan time and not updated when a bead joins mid-run.
- **Suggested fix shape:** assign the task index and record the attempt outcome (BLOCKED by planner) when a bead enters dispatch.

## Run metrics
### Judge panel
```
== 2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
super-roast verdict: Should-fix (7 confirmed)
mode: design        iteration: 1 of 3
independence: same-family (Claude) — seat-differentiated panel · rung: Workflow
seat-agreement: panels 32 · rr 0.81 · rg 0.63 · fg 0.63 · unanimous 0.53 · ground-loo 0.65 (n=26) · reproduce 8/24/0 · refute 4/28/0 · ground 16/16/0
== 2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-2.md
super-roast verdict: Should-fix (3 confirmed) [converged]
mode: design        iteration: 2 of 3
delta vs prior: 1 new confirmed (0 Blocking) · 0 carried (0 Blocking) · 5 resolved · 2 regressed (0 Blocking) · 0 punch-listed (open)
independence: same-family (Claude) — seat-differentiated panel · rung: Workflow
seat-agreement: panels 8 · rr 0.50 · rg 0.75 · fg 0.50 · unanimous 0.38 · ground-loo 0.75 (n=4) · reproduce 5/3/0 · refute 1/7/0 · ground 5/3/0
== 2026-10-28-mosh-phase2-keeper-inflight-window-roast-pr-1.md
super-roast verdict: Nit (4 confirmed) [low coverage] [review readiness incomplete]
mode: pr        iteration: 1 of 3
independence: same-family (Claude) — seat-differentiated panel · rung: Workflow
seat-agreement: panels 15 · rr 0.73 · rg 0.73 · fg 0.73 · unanimous 0.60 · ground-loo 0.82 (n=11) · reproduce 6/9/0 · refute 2/13/0 · ground 6/9/0
== 2026-10-28-mosh-phase2-keeper-inflight-window-roast-pr-2.md
super-roast verdict: Should-fix (3 confirmed) [low coverage] [review readiness incomplete]
mode: pr        iteration: 2 of 3
delta vs prior: 3 new confirmed (0 Blocking) · 0 carried (0 Blocking) · 1 resolved · 0 regressed (0 Blocking) · 3 punch-listed (open)
independence: same-family (Claude) — seat-differentiated panel · rung: Workflow
seat-agreement: panels 3 · rr 0.67 · rg 1.00 · fg 0.67 · unanimous 0.67 · ground-loo 1.00 (n=2) · reproduce 3/0/0 · refute 2/1/0 · ground 3/0/0
== 2026-10-28-mosh-phase2-keeper-inflight-window-roast-pr-3.md
super-roast verdict: Nit (1 confirmed) [low coverage] [review readiness incomplete]
mode: pr        iteration: 3 of 3
delta vs prior: 1 new confirmed (0 Blocking) · 0 carried (0 Blocking) · 3 resolved · 0 regressed (0 Blocking) · 3 punch-listed (open)
independence: same-family (Claude) — seat-differentiated panel · rung: Workflow
seat-agreement: panels 1 · rr 1.00 · rg 1.00 · fg 1.00 · unanimous 1.00 · ground-loo 1.00 (n=1) · reproduce 1/0/0 · refute 1/0/0 · ground 1/0/0
```
### Fix loop
```
Metrics: completions — review clean 12 · after fix pass 0 · parked 0 · re-entry closes 0 · dispatched early 5 · cancelled 0
Metrics: fix-pass — entered 0 · FIXED 0 · BLOCKED 0 (legacy event lines); durable attempts 0 · historical FIXED 0; repaired findings unavailable (use scope-dispositions --project)
```
### Merge-back
```
Metrics: merges 12 · merge-failed 0 · rebase-conflicts 0 · seam-reviews 0 (fixed 0) · check-fails 0 (fixed 0)
Metrics: ledger-check ok · append-failed 0 · append-retried 0 · scope: observed ledger; completed after available tracker reconciliation · git-only [] · ledger-only-git [] · gaps: none in observed sources
Merge: lines: 12 (all 'rebase clean · seam-review none · check pass')
already-merged --landed: task-mosh-hex.2 d4df9740b9a102f23bc1b80cabbcdc2eb6ce5c39 b9ea542dcee97413d0dfa460da4c3c150e954720; task-mosh-hex.1 044e69cd73a3c88fd85e5714b086e14ff6855694 2eb57ee7a5d5fca7429a92265ad5e3865b2138c6
```
### Coverage
```
- coverage-round-1 · canonical R-list: R1 "Ctrl-C under flood at 300 ms RTT shows the result within 1.5 s", R2 "adaptive delay-based in-flight window, 2 MiB only as cap", R3 "no LAN throughput loss (window >= 1 MiB)", R4 "session protocol and wire unchanged", R5 "delay-and-rate link emulation without root/tc", R6 "baseline and after-change measured; D3 decided without building pacing", R7 "fork files only, hook budget and deps unchanged" · requirements: 7 · mapped: 5 · unmapped: 2 (R4, R7) · auto GAP R4 → mosh-hex.4 amended; auto GAP R7 → mosh-hex.5 amended; auto GAP R5 → mosh-hex.1 retitled; auto GAP R8 (leg resume) → mosh-hex.5 amended; auto GAP R11 (source back-pressure) → mosh-hex.4 amended; auto GAP R1-miss → mosh-hex.5 amended; auto GAP R10 (RTT source) → mosh-hex.4 amended; auto UNOWNED-SEAM backlog metric + measurement helper → mosh-hex.3 owns; auto UNOWNED-SEAM after-change evidence + D3 → mosh-hex.5 owns; NEEDS-SPEC mosh-hex.4 declined (leaf) · r-new appended as R8–R11
- coverage-round-2 · requirements: 11 · mapped: 11 · unmapped: 0 · divergence: findings 9 → 9 · novel 8/9 (89%) · widening: yes · auto GAP baseline ordering → edge mosh-hex.4<-mosh-hex.3; auto GAP server slow-client → mosh-hex.3 amended; auto GAP R4 verify → mosh-hex.5 amended; auto UNOWNED-SEAM leg-kill → mosh-hex.5 consumes signal_leg; auto UNOWNED-SEAM INITIAL/trace leg → mosh-hex.4/.5 amended; auto GAP R1 conditional → mosh-hex.3 budget check; auto GAP metric cap → mosh-hex.3/.5 amended; auto GAP harness location → rejected (evidence does not hold); auto UNOWNED-SEAM trace capture → mosh-hex.4 owns
scopeFilter-round-2: 2 in-scope · 1 punch-listed
scopeFilter-round-1: 1 in-scope · 3 punch-listed
```
### Timing
```
Profile: mosh-hex · 1 invocation · 2026-10-28T08:29Z → 2026-10-28T09:55Z · wall 1h26m (task graph 58m, after the last landing 29m) · agents 48 (timed 48) · beads dispatched 5, landed 4, planned 6
Profile: bound — graph lower bound 58m along mosh-hex.3 → mosh-hex.4 (unlimited slots and merge lanes) · model of the run 58m (cap 14, one merge lane) vs actual 58m (100%) → graph-bound: the 2-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 58m = implement 29m · review 1m · merge 49s · planning 27m · ready 8s · startup 16s · dispatch 13s
Profile: bottleneck — mosh-hex.4 — 19m on the critical path (33%) · implement 17m · review 1m · merge 49s · dispatch 6s · implement on the path 17m: test 6m · poll 6m · build 1m · git 1s · bd 2s · read 12s · shell 14s · model 3m (longest: "sleep 5" 2m, "PATH=$HOME/.local/bin:$PATH" 2m) · dependents 2 direct, 2 transitive
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 4 landed beads
Profile: merge lane — busy 4m of 40m (10%) · seam reviews and fixes 0s (0% of busy) · 4 merge dispatches, first try not merged for 0 of 4 · queue wait 2m total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — cut mosh-hex.4 <- mosh-hex.3 → 46m (−11m, 20%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.4 → 49m (−9m, 15%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-hex.3 → 50m (−8m, 14%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.3 → 52m (−6m, 10%) — its implement time halved; first dispatch to last landing, planning retained
Profile: mosh-hex · 1 invocation · 2026-10-28T09:58Z → 2026-10-28T10:24Z · wall 26m (task graph 20m, after the last landing 7m) · agents 26 (timed 26) · beads dispatched 1, landed 0, planned 6
Profile: bound — no model: nothing landed
Profile: critical path — 20m = implement 16m · blocker 2m · planning 36s · ready 17s · startup 30s · dispatch 13s
Profile: bottleneck — mosh-hex.5 (did not land) — 18m on the critical path (93%) · implement 16m · blocker 2m · dispatch 11s · implement on the path 16m: test 11s · poll 12m · git 7s · bd 5s · read 6s · shell 15s · model 4m (longest: "for i in $(seq 9)" 5m, "for i in $(seq 9)" 3m) · 2 attempts (BLOCKED, BLOCKED) · dependents 1 direct, 1 transitive
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 0 landed beads
Profile: redo — on the critical path, up to retries 2m (mosh-hex.5) — from a bead's first attempt to its final one; the what-ifs model the real saving
Profile: merge lane — busy 0s of 0s (0%) · seam reviews and fixes 0s (0% of busy) · 0 merge dispatches, first try not merged for unknown (no merge results) · queue wait 0s total, peak 0 waiting · stack-parent wait 0s
Profile: mosh-hex · 1 invocation · 2026-10-28T10:25Z → 2026-10-28T10:37Z · wall 12m (task graph 5m, after the last landing 7m) · agents 25 (timed 25) · beads dispatched 1, landed 0, planned 6
Profile: bound — no model: nothing landed
Profile: critical path — 5m = blocker 1m · planning 2m · ready 6s · startup 33s · dispatch 5s · unmeasured-gap 2s
Profile: diagnosis — planning 2m (55% of first dispatch to last task step) · planning dominates; bead bottleneck below covers task work only
Profile: bottleneck — mosh-hex.9 (did not land) — 1m on the critical path (29%) · blocker 1m · dispatch 3s · dependents 1
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 0 landed beads
Profile: merge lane — busy 0s of 0s (0%) · seam reviews and fixes 0s (0% of busy) · 0 merge dispatches, first try not merged for unknown (no merge results) · queue wait 0s total, peak 0 waiting · stack-parent wait 0s
Profile: unmeasured — availability/readiness timing unknown for 1 bead(s): waits unmeasured; schedule estimates retain dispatch as the release boundary; 1 dispatched bead(s) did not land (not in the model): mosh-hex.9; Claude context windows from the model table: claude-opus-5-5 1000K, claude-sonnet-5-5 1000K
Profile: mosh-hex · 1 invocation · 2026-10-28T10:38Z → 2026-10-28T11:37Z · wall 59m (task graph 51m, after the last landing 8m) · agents 41 (timed 41) · beads dispatched 3, landed 3, planned 7
Profile: bound — graph lower bound 48m along mosh-hex.9 → mosh-hex.5 → mosh-hex.6 (unlimited slots and merge lanes) · model of the run 48m (cap 14, one merge lane) vs actual 51m (95%) → graph-bound: the 3-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 51m = implement 42m · review 39s · merge 44s · blocker 1m · planning 4m · ready 14s · startup 51s · dispatch 19s
Profile: bottleneck — mosh-hex.6 — 17m on the critical path (34%) · implement 16m · review 39s · merge 44s · dispatch 6s · implement on the path 16m: test 6m · poll 6m · read 8s · shell 12s · model 3m (longest: "for i in $(seq 55)" 2m, "for i in $(seq 11)" 2m) · dependents 0
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 2m over 1 of 3 landed beads — retry 2m (1)
Profile: redo — on the critical path, up to retries 2m (mosh-hex.5) — from a bead's first attempt to its final one; the what-ifs model the real saving
Profile: merge lane — busy 3m of 31m (8%) · seam reviews and fixes 0s (0% of busy) · 3 merge dispatches, first try not merged for 0 of 3 · queue wait 5s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — cut mosh-hex.6 <- mosh-hex.5 → 32m (−16m, 33%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — cut mosh-hex.5 <- mosh-hex.9 → 35m (−13m, 28%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-hex.9 → 38m (−10m, 20%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-hex.5 → 40m (−8m, 17%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.6 → 40m (−8m, 17%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.9 → 41m (−7m, 14%) — its implement time halved; first dispatch to last landing, planning retained
Profile: mosh-hex · 1 invocation · 2026-10-28T11:55Z → 2026-10-28T12:44Z · wall 50m (task graph 43m, after the last landing 7m) · agents 28 (timed 28) · beads dispatched 2, landed 2, planned 9
Profile: bound — graph lower bound 43m along mosh-hex.12 → mosh-hex.13 (unlimited slots and merge lanes) · model of the run 43m (cap 14, one merge lane) vs actual 43m (100%) → graph-bound: the 2-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 43m = implement 33m · review 39s · merge 5m · planning 4m · ready 4s · startup 52s · dispatch 9s
Profile: bottleneck — mosh-hex.13 — 33m on the critical path (78%) · implement 28m · review 39s · merge 5m · dispatch 6s · implement on the path 28m: test 4m · poll 21m · git 1s · read 7s · shell 9s · model 2m (longest: "for i in $(seq 90)" 8m, "for i in $(seq 90)" 8m) · dependents 0
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 2 landed beads
Profile: merge lane — busy 5m of 33m (16%) · seam reviews and fixes 0s (0% of busy) · 2 merge dispatches, first try not merged for 0 of 2 · queue wait 4s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — faster mosh-hex.13 → 29m (−14m, 33%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — cut mosh-hex.13 <- mosh-hex.12 → 42m (−1m, 3%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-hex.12 → 42m (−1m, 3%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.12 → 42m (−1m, 3%) — its implement time halved; first dispatch to last landing, planning retained
Profile: mosh-hex · 1 invocation · 2026-10-28T12:54Z → 2026-10-28T13:21Z · wall 27m (task graph 19m, after the last landing 8m) · agents 32 (timed 32) · beads dispatched 3, landed 3, planned 12
Profile: bound — graph lower bound 19m along mosh-hex.15 → mosh-hex.17 (unlimited slots and merge lanes) · model of the run 19m (cap 14, one merge lane) vs actual 19m (99%) → graph-bound: the 2-bead dependency chain set the pace — reshaping it is the lever
Profile: critical path — 19m = implement 11m · review 22s · merge 46s · planning 5m · ready 9s · startup 1m · dispatch 10s
Profile: bottleneck — mosh-hex.15 — 7m on the critical path (38%) · implement 7m · dispatch 2s · implement on the path 7m: test 4m · build 1m · git 4s · read 4s · shell 9s · model 1m (longest: "PATH=$PATH:/var/home/alepar/.local/bin" 2m, "cargo fmt" 1m) · dependents 1 direct, 1 transitive
Profile: waits — ready→start, summed per bead (beads wait at the same time, so not wall time): 0s over 0 of 3 landed beads
Profile: merge lane — busy 2m of 8m (25%) · seam reviews and fixes 0s (0% of busy) · 3 merge dispatches, first try not merged for 0 of 3 · queue wait 6s total, peak 1 waiting · stack-parent wait 0s
Profile: what-if estimate — cut mosh-hex.17 <- mosh-hex.15 → 15m (−3m, 17%) — payoff if this edge were cut — whether it can be is a design call; first dispatch to last landing, planning retained
Profile: what-if estimate — split mosh-hex.15 → 15m (−3m, 17%) — its 1 dependent(s) wait only for a contract slice ≈30% of its implement time; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.15 → 15m (−3m, 17%) — its implement time halved; first dispatch to last landing, planning retained
Profile: what-if estimate — faster mosh-hex.17 → 17m (−2m, 11%) — its implement time halved; first dispatch to last landing, planning retained
```

### Bead graph
| id | type | title | what (first sentence of description) |
| --- | --- | --- | --- |
| mosh-hex | epic | Mosh Phase 2: adaptive keeper in-flight window | Phase 2 of HERDR_MOSH_VISION.md: an adaptive, delay-based sender window keeps about one link of output in flig |
| mosh-hex.1 | task | Link-emulation harness: delay-and-rate link knob for the fake ssh | Spec §4. Add a `link` knob to tests/mosh_harness.rs holding `<one-way delay ms> <rate bytes/s>`; when present  |
| mosh-hex.10 | task | review: mosh-hex.9 | Review, fix and merge of mosh-hex.9, tracked separately so mosh-hex.9's dependents can start once it is implem |
| mosh-hex.11 | task | review: mosh-hex.5 | Review, fix and merge of mosh-hex.5, tracked separately so mosh-hex.5's dependents can start once it is implem |
| mosh-hex.12 | task | Fix: dense-flood outage test that exercises the INITIAL read cap end to end | Covers: [Nit] tests/mosh_resume.rs:452; tests/mosh_resume.rs:461; final-review untested item 'Server-side effe |
| mosh-hex.13 | task | Fix: one timing policy for wall-clock tests (condition waits, serialised group,  | Covers: final-review [Must-fix] Decide CI placement of the wall-clock pacing tests in tests/mosh_pacing.rs; cl |
| mosh-hex.14 | task | review: mosh-hex.12 | Review, fix and merge of mosh-hex.12, tracked separately so mosh-hex.12's dependents can start once it is impl |
| mosh-hex.15 | task | Fix: gate only Linux-only observations in the mosh_resume outage test | Covers: [Should-fix] tests/mosh_resume.rs:19 |
| mosh-hex.16 | task | Fix: unit test that leg_down resets a grown window and caps down reads at INITIA | Covers: [Should-fix] src/mosh/session.rs:641 |
| mosh-hex.17 | task | Fix: visible skips for opt-in and tool-missing mosh timing tests | Covers: final-review [Must-fix] Make the opt-in and tool-missing skips in tests/mosh_pacing.rs and tests/mosh_ |
| mosh-hex.18 | task | review: mosh-hex.15 | Review, fix and merge of mosh-hex.15, tracked separately so mosh-hex.15's dependents can start once it is impl |
| mosh-hex.2 | task | InflightWindow controller in src/mosh/window.rs | Spec §1 and ## Key abstractions (InflightWindow entry, carried verbatim in the root epic digest). Pure sans-IO |
| mosh-hex.3 | task | Measure the fixed-window baseline: flood rate, Ctrl-C latency, server backlog | Spec §5 and §6. Evidence leaf, against the current (fixed 2 MiB window) code, using the link harness. Writes d |
| mosh-hex.4 | task | Wire InflightWindow into SessionEndpoint; size source reads; keeper window trace | Spec §2, §3, §7 and the ## Key abstractions SessionEndpoint/Keeper/LocalBridge entries. SessionEndpoint owns o |
| mosh-hex.5 | task | Acceptance: Ctrl-C under flood at 300 ms RTT, LAN throughput, convergence; D3 ev | Spec §5 and §6. tests/mosh_pacing.rs (non-ignored tests): (1) Ctrl-C under flood: set_link(150, R) with R and  |
| mosh-hex.6 | task | Integration sweep: Phase 2 adaptive window end to end | Root integration sweep. Verify the goal's main flow end to end over the combined tree: flood at 150 ms one-way |
| mosh-hex.7 | task | review: mosh-hex.3 | Review, fix and merge of mosh-hex.3, tracked separately so mosh-hex.3's dependents can start once it is implem |
| mosh-hex.8 | task | review: mosh-hex.4 | Review, fix and merge of mosh-hex.4, tracked separately so mosh-hex.4's dependents can start once it is implem |
| mosh-hex.9 | task | Cap keeper source reads at the reset window while the leg is down | Orchestrator decision 2026-10-28 (option A of blocker mosh-00q; supersedes mosh-hex.4's 'ring cap rule unchang |

| dependent | blocker | reason (from `blocked-by` line, or `unstated`) |
| --- | --- | --- |
| mosh-hex.10 | mosh-hex.9 | unstated |
| mosh-hex.11 | mosh-hex.5 | unstated |
| mosh-hex.13 | mosh-hex.12 | consumes the rewritten mosh_resume dense-flood outage test (both edit tests/mosh_resume.rs) |
| mosh-hex.14 | mosh-hex.12 | unstated |
| mosh-hex.17 | mosh-hex.15 | consumes the moved timing_lock / ss helpers in tests/mosh_harness.rs (both edit tests/mosh_resume.rs |
| mosh-hex.18 | mosh-hex.15 | unstated |
| mosh-hex.3 | mosh-hex.1 | consumes link knob + Remote::set_link/clear_link |
| mosh-hex.4 | mosh-hex.2 | consumes InflightWindow |
| mosh-hex.4 | mosh-hex.3 | consumes fixed-window baseline recorded in evidence/phase2-baseline.md (the baseline must be measure |
| mosh-hex.5 | mosh-hex.1 | consumes link knob + Remote::set_link/clear_link |
| mosh-hex.5 | mosh-hex.4 | consumes adaptive window + keeper `window` trace events |
| mosh-hex.5 | mosh-hex.3 | consumes chosen profile constants (R, terminal size) in evidence/phase2-baseline.md |
| mosh-hex.5 | mosh-hex.9 | consumes the outage source-read cap (keeper stops filling the ring while the leg is down) |
| mosh-hex.6 | mosh-hex.1 | consumes all leaves (integration sweep) |
| mosh-hex.6 | mosh-hex.2 | consumes all leaves (integration sweep) |
| mosh-hex.6 | mosh-hex.5 | consumes all leaves (integration sweep) |
| mosh-hex.6 | mosh-hex.4 | consumes all leaves (integration sweep) |
| mosh-hex.6 | mosh-hex.3 | consumes all leaves (integration sweep) |
| mosh-hex.7 | mosh-hex.3 | unstated |
| mosh-hex.8 | mosh-hex.4 | unstated |

## Design questions
### Should a blocker the orchestrator decides in an autonomous run resolve inside the same super-code invocation?
Evidence: per-invocation tail after the last landing 29m, 7m, 7m, 8m, 7m, 8m; invocation 2 'beads dispatched 1, landed 0'; invocation 3 'landed 0', planning 55%; both ended on blockers the orchestrator then decided (friction: invocations 2 and 3).
For: resolving in-invocation saves the fixed final-review/recovery tail and re-planning, about 15 minutes in this run. Against: an invocation boundary gives a clean final-review and recovery checkpoint around a decision that changes the spec.
If upstream decides otherwise, please state the position explicitly so downstream can reconcile against words rather than silence.

## Doc gaps
none (the cap-round step-back question was already raised in the prior run's feedback)

## Already fixed — do not re-litigate
- Prior-run triage already covers: super-plan-2s3 (review report producer/extractor contract), super-plan-1s3 (context-preflight 200K vs profiler 1M), super-plan-tco (callers omit evidence-backed reviewReadiness). Defect 1 sits next to super-plan-tco; check both before filing.

## Not established
- By default: the coordinator's ledger-append path is fire-and-forget and can lose a line without the coordinator noticing, so every ledger-derived count in `## Run metrics` (Fix loop, Merge-back) is a lower bound. The `Metrics: ledger-check` line is the one cross-check that exists.
- The graph shape handed to the analyst was computed on the final dump after every bead closed (leaves 0); the bead table above is the authoritative graph.
- Analyst model: opus. Single run; no measurement of the per-invocation tail's composition.

## Verification bar
- Re-run a two-run sequence on one base branch where the first run leaves an incomplete review collection, and check the second run's review-input selection reports available provenance for its own range.
- Run scope-dispositions --project on a run with landed fix beads and repeated final-review findings and check repaired > 0 and grouped counts.
- Inspect implementer commits from a run whose AGENTS.md forbids AI trailers.

---
