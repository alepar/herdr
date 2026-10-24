# super-auto run — 2026-10-24-mosh-phase1-resumable-session

flags: planOneShot=true skipPlanRoast=false skipCodeRoast=false autonomous=true
phase: design
codeMechanism: Workflow
runTag: mosh-phase1-resumable-session-20261024T052906
context: Context: claude · planner=200K/167K · implementer=200K/167K · reviewer=200K/167K · finalReview=200K/167K · assembler=200K/167K · warn: planner, finalReview on a 200K model → set ANTHROPIC_DEFAULT_OPUS_MODEL=<model>[1m] or config.models.<role> to a 1M model · warn: implementer, reviewer, assembler on a 200K model → set ANTHROPIC_DEFAULT_SONNET_MODEL=<model>[1m] or config.models.<role> to a 1M model

idea: Implement Phase 1 of HERDR_MOSH_VISION.md (repo root, branch `mosh`, push to remote `fork` = github.com/alepar/herdr; never push to `origin`). Fully autonomous; both roasts (design roast and code roast) ON. Design inputs: HERDR_MOSH_VISION.md §4.1, §5 (Phase 1 acceptance), §6 (minimal-diff rules + Phase 1 touch list), decisions D1=B (link-level resumable session: local bridge + remote keeper), D2=60s grace, D5=outage banner included in Phase 1, D6=accept late execution. Detailed verified touch map with file:line: /tmp/claude-1001/-var-home-alepar-AleCode-herdr/f90071ef-0086-4b3f-982c-c58ab7dd70ba/scratchpad/herdr-mosh-touch-map.md (§2.1 is the Phase 1 spec). Background reports in the same dir: herdr-remote-report.md, mosh-report.md. Hard constraints: all new code under src/mosh/ mounted via one #[path] line in src/remote/attach.rs; every edit to an upstream file is a one-statement hook tagged `// mosh-fork hook`; no wire enum/PROTOCOL_VERSION/hello-field changes; no new Cargo dependencies; tests in fork files only (src/mosh #[cfg(test)] modules and tests/mosh_*.rs with fake ssh on PATH). Conventional-commit messages (repo .githooks enforce them).
spec: 2026-10-24-mosh-phase1-resumable-session-design.md
epic: mosh-dqw
branch: super-auto/mosh-phase1-resumable-session
base: mosh

approvals:
- top-split · auto · mosh-dqw.1 LEAF, mosh-dqw.2 LEAF, mosh-dqw.3 LEAF, mosh-dqw.4 LEAF, mosh-dqw.5 LEAF, mosh-dqw.6 LEAF, mosh-dqw.7 LEAF, mosh-dqw.8 LEAF, mosh-dqw.9 LEAF, mosh-dqw.10 LEAF
- coverage-round-1 · canonical R-list: R1..R15 in coverage-round-1-requirements.md (+R16-R18 from r-new) · requirements: 15 · mapped: 14 · unmapped: 3 (R2, R10, R11) · auto flag-sweep mosh-dqw.2/.3/.8 → kept; auto GAP R2,R3,R9,R10,R11,R12 → applied (summary lines; scope already in bead bodies); auto UNOWNED-SEAM keeper rendezvous → owner mosh-dqw.1 (+spec lock fix); auto UNOWNED-SEAM argv flags/capability/MoshConfig → consumes lines; auto UNOWNED-SEAM link-frame format → owner mosh-dqw.2, edge mosh-dqw.5<-mosh-dqw.2; auto rejected OutageState, Leg-keeper, .8 breadth
