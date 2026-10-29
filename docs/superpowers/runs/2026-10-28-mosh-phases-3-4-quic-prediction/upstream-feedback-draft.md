# 2026-10-28-mosh-phases-3-4-quic-prediction: merge agents add Co-Authored-By trailers the repo forbids, and the history rewrite that removes them breaks SHA-only ledger reconciliation

Plugin: superpowers 6.4.2-alepar6.6. Run type: super-auto, autonomous, both roasts on. This is an unscrubbed draft parked for the human's review.

## Defects

### 1. Merge agents write Co-Authored-By trailers that the repo's AGENTS.md forbids, in every phase
- **Evidence:** "merge agent wrote Co-Authored-By trailers on merge commits d07035dc, 1f4be7cc despite AGENTS.md line 237 (same defect as Phase 2)"; "stripped Co-Authored-By trailers from 5 merge commits"; "[phase 5] merge agent again added Co-Authored-By on 2 merges". The caller removed them twice with tree-preserving rewrites.
- **Premise to verify:** the trailer comes from the harness attribution reminder reaching the merge subagent, and the merge prompt does not relay the repo's commit-message rules.
- **Suggested fix shape:** have the merge prompt read the repo's commit rules (AGENTS.md/CLAUDE.md) and rank them above harness attribution, or reject a merge commit with a forbidden trailer before landing.

### 2. Ledger reconciliation matches landings by SHA only and reports "ok" while every landing is unmatched
- **Evidence:** after a tree-identical rewrite, "reconciliation reports all landings as ledger-only-git and treated reopened sweep mosh-19g.7 as an unconfirmed pending closure (stopReason closure-unconfirmed)". The final line reads "Metrics: ledger-check ok · … ledger-only-git [26 ids] · gaps: ledger/Git identities differ".
- **Premise to verify:** tree or patch-id equivalence is a sound fallback when the trees match.
- **Suggested fix shape:** fall back to tree or patch-id identity when a SHA is missing; never print "ok" with a non-empty ledger-only-git list; document that history must not be rewritten between super-code invocations (or fix defect 1 so no rewrite is needed).

### 3. A triage edge onto a review bead stalls an invocation that has ready work one merge away
- **Evidence:** "coordinator invocation 1 returned stalled with mosh-19g.1.7 ready: a triage edge 1.7 <- review bead 1.15 cleared only when 1.6 merged, after round 3 found 0 ready". Detector round 3: "0 ready · … peak in-flight 0".
- **Premise to verify:** the no-progress guard runs before ready work is re-queried once the merge lane drains.
- **Suggested fix shape:** re-query ready beads after the merge lane drains and before declaring a stall, or point triage edges at the landing instead of at a review bead.

### 4. A merge-lane hold persists after its cause is gone
- **Evidence:** invocation 4 stopped integration-blocked (untracked directory in the integration worktree); after removal, "invocation 5 returned recovery-held: the merge-lane hold from invocation 4 needs a caller clear record". A manual `Recovery … clear` was needed.
- **Premise to verify:** the hold's condition (a clean integration worktree) can be checked mechanically at launch.
- **Suggested fix shape:** re-check the hold condition at launch and clear it automatically when it passes.

### 5. Dispatch sent a tree hash as HEAD to the reviewer
- **Evidence:** "Task 30 (mosh-19g.2.21): pending retry [cause: report swapped commit and tree hashes; dispatch passed tree as HEAD] … This is a pipeline defect".
- **Premise to verify:** dispatch takes HEAD from the implementer report text without validation.
- **Suggested fix shape:** check `git cat-file -t <head>` is `commit` and on the task branch, or derive HEAD from the branch.

### 6. coverage-inputs captures trailing punctuation into the Bead id
- **Evidence:** "header 'Bead: <id>.' (sentence-final period) is captured into the id → unmatched-spec; the Bead regex takes [^ \t]* so punctuation leaks in".
- **Premise to verify:** bead ids never end in punctuation.
- **Suggested fix shape:** restrict the capture to the id character class or strip trailing `.,;:)`.

### 7. Top-up queries exhaust their budget in rounds the edge audit already called graph-bound
- **Evidence:** "round 1 — 3 ready · topped-up 4 · … top-up queries 40/40 · … idle slots 11"; "round 2 — 2 ready · … 40/40 · idle slots 12", next to "Edge audit: … The graph is the binding constraint, not the cap".
- **Premise to verify:** query yield is predictable from the audit's achievable width.
- **Suggested fix shape:** stop topping up once achievable width ≤ in-flight + ready, or after several empty queries.

### 8. Inherited provenance gaps make every later final review and code roast "bounded" for the whole run
- **Evidence:** "Final review: bounded (captured source available; omission provenance unavailable)" in all 7 invocations; every code roast round ran "review input selection: provenance unavailable" from two incomplete Phase 1 collections; one invocation's final-review file (inv-36aa01c2) was never written.
- **Premise to verify:** provenance can be re-established from the current run's base commit.
- **Suggested fix shape:** offer a re-baseline at run start that treats the run base as provenance-complete; make a missing final-review file a visible error.

### 9. Findings bookkeeping drifts from the roast reports
- **Evidence:** "punch-list projection (scope-dispositions --project --keys) exit 3: no retained source extraction" (sources 0); findings-ledger.md keeps r1 and r2 rows `open` that roast-pr-2.md and roast-pr-3.md mark resolved by named fix commits.
- **Premise to verify:** roast rounds are meant to retain the extraction scope-dispositions reads and to drive resolution in the ledger.
- **Suggested fix shape:** persist the extraction each round and write `resolved@rN` back to the rows a later round resolves.

## Run metrics

### Judge panel
- design r1: seat-agreement panels 48 · rr 0.77 · rg 0.73 · fg 0.54 · unanimous 0.52 · same-family (Claude)
- design r2: panels 10 · rr 0.80 · rg 0.80 · fg 0.60 · unanimous 0.60 · converged
- pr r1: panels 26 · rr 0.85 · rg 0.81 · fg 0.73 · unanimous 0.69 · bounded
- pr r2: panels 3 · rr 1.00 · rg 0.67 · fg 0.67 · bounded
- pr r3: panels 4 · all 1.00 · bounded
### Fix loop
- Code roast: r1 11 confirmed (8 in scope, 5 punch-listed), r2 2 confirmed (2 in scope), r3 1 confirmed; exit capped at 3, 0 Blocking open.
### Merge-back
- 38 merges, 0 merge-failed, 2 rebase conflicts (seam review cleared), 0 check fails; 1 fix pass on a task.
### Coverage
- Round 1: requirements 13 · mapped 13 · unmapped 0; round 2: 16 · 14 · 2 (both applied); divergence 19 → 15, widening no.
### Timing
- Not established (no `Profile:` lines; see below).
### Bead graph
- 25 planned leaves + 11 fix/regression beads; design depth 14, width 1.8; edge audits repeatedly report achievable width 2-3 against a cap of 14.

## Design questions

- **Cross-phase regressions surface only at the caller's sweep.** "REGRESSION: mosh_resume … passes at run base, fails on branch; bisect → merge 87f46203 … No task review or mergeCheck caught it (task tests were filtered, sweep deferred)". Should super-code run a between-invocation regression sweep, or the tests importing changed modules, on the integration tip? For: catches regressions near their bisect point. Against: full-suite cost (4754 tests here) and flaky environment failures (14 pre-existing).
- **Literal user constraints loosened without a flag.** The idea text set "upstream diff budget <= 20 lines"; the root design raised it to 30 as an agent decision and no roast flagged it; an acceptance clause and the goal's wrong-prediction bound were also loosened. Should a check diff the idea text's numeric constraints against the spec and acceptance amendments and flag any loosening for the human?

## Doc gaps

- **Coverage reviewers see too little of each bead.** Both reviewers spent their NEEDS-SPEC re-run because "only Goal sections + first sentences reach them, so owns/acceptance detail in long bead descriptions was invisible"; a further request was dropped silently.
- **Implementer evidence format.** "Recurring minor ×14 across 14 tasks [process-class: missing-evidence]": the implementer template does not carry the structured-evidence format the reviewer checks.
- **Evidence logs committed into the tree.** Agents committed run logs under evidence/, counted as 595 untagged upstream lines by the project's budget script; nothing says logs belong outside the repo.
- **Merged task worktrees keep build output.** Disk pressure forced the caller to remove target/ from 35 merged task worktrees (275 GB → 30 GB) while cleanup kept worktrees after process inspection failures.

## Already fixed — do not re-litigate

- none

## Not established

- `Profile:` lines: none in the ledger. The invoking session did not append run-profile output after each invocation (an orchestration omission in this run), so timing is unavailable.
- The ledger-append path is fire-and-forget and can lose a line; no loss was observed here.
- Analyst model: opus.

## Verification bar

Each defect quotes this run's friction log, ledger or report; reproduce against 6.4.2-alepar6.6 before building a fix.

---
