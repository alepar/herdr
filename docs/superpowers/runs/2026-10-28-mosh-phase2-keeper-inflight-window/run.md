# super-auto run — 2026-10-28-mosh-phase2-keeper-inflight-window

flags: planOneShot=f skipPlanRoast=f skipCodeRoast=f autonomous=t
phase: roast-design
codeMechanism: Workflow
runTag: mosh-phase2-keeper-inflight-window-20261028T074108
context: Context: claude · planner=200K/167K · implementer=200K/167K · reviewer=200K/167K · finalReview=200K/167K · assembler=200K/167K · warn: planner, finalReview on a 200K model → set ANTHROPIC_DEFAULT_OPUS_MODEL=<model>[1m] or config.models.<role> to a 1M model · warn: implementer, reviewer, assembler on a 200K model → set ANTHROPIC_DEFAULT_SONNET_MODEL=<model>[1m] or config.models.<role> to a 1M model

idea: Implement Phase 2 of HERDR_MOSH_VISION.md (repo root, branch `mosh` at be183064 which now contains Phase 1; push only to remote `fork` = github.com/alepar/herdr; never push to `origin`). Fully autonomous; both roasts (design roast and code roast) ON. Phase 2 = keeper in-flight window (§4.2): while a leg is up, the keeper's replay ring becomes an RTT-sized in-flight window so the server's single latest-wins render slot coalesces frames, giving link-adaptive pacing with no server change. Decision D3 is now decided: keeper window first; per-client server pacing at clamp(SRTT/2,16,250) ms only if measurements show socket-buffer staleness (record the measurement; do not build it otherwise). Acceptance (§5): Ctrl-C is responsive under an output flood at 300 ms RTT. Phase 0's netem harness was never built, so include the minimum test harness needed to measure this acceptance (delayed/lossy link emulation inside the fork's test harness, no root or tc required for CI; keep it in fork files). Phase 1 design and code for context: docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/ (design spec, report.md) and src/mosh/. Hard constraints (same as Phase 1): all new code under src/mosh/ (already mounted); every edit to an upstream file is a one-statement hook tagged `// mosh-fork hook`, upstream diff budget stays <= 20 lines (scripts/mosh-diff-budget.sh, currently 12); no wire enum/PROTOCOL_VERSION/hello-field changes; no new Cargo dependencies; tests in fork files only (src/mosh #[cfg(test)] modules and tests/mosh_*.rs); conventional-commit messages without AI co-author lines (repo AGENTS.md). Footprint of 12 lines was accepted by the human on 2026-10-28.
spec: 2026-10-28-mosh-phase2-keeper-inflight-window-design.md
epic: mosh-hex
branch: super-auto/mosh-phase2-keeper-inflight-window
base: mosh
roastDesignRound: 2
findingsLedgerDesign: findings-ledger-design.md
roast-design: 2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md, 2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-2.md
stepBackDesign-round-1: local — seven independent-or-clustered r1 findings, no mechanical triggers; two clusters (C1 sampling bookkeeping, C2 claims derivable from the law) fixed by one rule each
graph-pass: depth 5→5 · width 1.2→1.2 · applied 0 · parked 1
stepBackDesign-round-2: local — round 2 converged ([converged], 0 Blocking); punch list of 3 sub-Blocking findings applied inline to spec and beads, not re-roasted

approvals:
- top-split · auto · mosh-hex.1 LEAF, mosh-hex.2 LEAF, mosh-hex.3 LEAF, mosh-hex.4 LEAF, mosh-hex.5 LEAF
- coverage-round-1 · canonical R-list: R1 "Ctrl-C under flood at 300 ms RTT shows the result within 1.5 s", R2 "adaptive delay-based in-flight window, 2 MiB only as cap", R3 "no LAN throughput loss (window >= 1 MiB)", R4 "session protocol and wire unchanged", R5 "delay-and-rate link emulation without root/tc", R6 "baseline and after-change measured; D3 decided without building pacing", R7 "fork files only, hook budget and deps unchanged" · requirements: 7 · mapped: 5 · unmapped: 2 (R4, R7) · auto GAP R4 → mosh-hex.4 amended; auto GAP R7 → mosh-hex.5 amended; auto GAP R5 → mosh-hex.1 retitled; auto GAP R8 (leg resume) → mosh-hex.5 amended; auto GAP R11 (source back-pressure) → mosh-hex.4 amended; auto GAP R1-miss → mosh-hex.5 amended; auto GAP R10 (RTT source) → mosh-hex.4 amended; auto UNOWNED-SEAM backlog metric + measurement helper → mosh-hex.3 owns; auto UNOWNED-SEAM after-change evidence + D3 → mosh-hex.5 owns; NEEDS-SPEC mosh-hex.4 declined (leaf) · r-new appended as R8–R11
- coverage-round-2 · requirements: 11 · mapped: 11 · unmapped: 0 · divergence: findings 9 → 9 · novel 8/9 (89%) · widening: yes · auto GAP baseline ordering → edge mosh-hex.4<-mosh-hex.3; auto GAP server slow-client → mosh-hex.3 amended; auto GAP R4 verify → mosh-hex.5 amended; auto UNOWNED-SEAM leg-kill → mosh-hex.5 consumes signal_leg; auto UNOWNED-SEAM INITIAL/trace leg → mosh-hex.4/.5 amended; auto GAP R1 conditional → mosh-hex.3 budget check; auto GAP metric cap → mosh-hex.3/.5 amended; auto GAP harness location → rejected (evidence does not hold); auto UNOWNED-SEAM trace capture → mosh-hex.4 owns

parked:
- 2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-2.md · escalation · "material dissent: does the 1.5 s budget close at R = 256 KiB/s (2-1 confirm, ~4% over on 'about' figures); decided: spec states the budget arithmetic, claims the goal only at the R the acceptance test runs, and mosh-hex.3's predicted budget moves the supported bottom up if 256 KiB/s does not close"
- coverage-ledger.md#cov-21 · graph-change · "drop mosh-hex.4 <- mosh-hex.3 (safe no): saves one round if the baseline is pinned to a pre-window commit carrying mosh-hex.1; not applied"
