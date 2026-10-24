# super-auto run — 2026-10-24-mosh-phase1-resumable-session

flags: planOneShot=true skipPlanRoast=false skipCodeRoast=false autonomous=true
phase: design
codeMechanism: Workflow
runTag: mosh-phase1-resumable-session-20261024T052906
context: Context: claude · planner=200K/167K · implementer=200K/167K · reviewer=200K/167K · finalReview=200K/167K · assembler=200K/167K · warn: planner, finalReview on a 200K model → set ANTHROPIC_DEFAULT_OPUS_MODEL=<model>[1m] or config.models.<role> to a 1M model · warn: implementer, reviewer, assembler on a 200K model → set ANTHROPIC_DEFAULT_SONNET_MODEL=<model>[1m] or config.models.<role> to a 1M model

idea: Implement Phase 1 of HERDR_MOSH_VISION.md (repo root, branch `mosh`, push to remote `fork` = github.com/alepar/herdr; never push to `origin`). Fully autonomous; both roasts (design roast and code roast) ON. Design inputs: HERDR_MOSH_VISION.md §4.1, §5 (Phase 1 acceptance), §6 (minimal-diff rules + Phase 1 touch list), decisions D1=B (link-level resumable session: local bridge + remote keeper), D2=60s grace, D5=outage banner included in Phase 1, D6=accept late execution. Detailed verified touch map with file:line: /tmp/claude-1001/-var-home-alepar-AleCode-herdr/f90071ef-0086-4b3f-982c-c58ab7dd70ba/scratchpad/herdr-mosh-touch-map.md (§2.1 is the Phase 1 spec). Background reports in the same dir: herdr-remote-report.md, mosh-report.md. Hard constraints: all new code under src/mosh/ mounted via one #[path] line in src/remote/attach.rs; every edit to an upstream file is a one-statement hook tagged `// mosh-fork hook`; no wire enum/PROTOCOL_VERSION/hello-field changes; no new Cargo dependencies; tests in fork files only (src/mosh #[cfg(test)] modules and tests/mosh_*.rs with fake ssh on PATH). Conventional-commit messages (repo .githooks enforce them).
spec: 2026-10-24-mosh-phase1-resumable-session-design.md
branch: super-auto/mosh-phase1-resumable-session
base: mosh
