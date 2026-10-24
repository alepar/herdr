# friction log — 2026-10-24-mosh-phase1-resumable-session
- [pre-flight] repo had no beads tracker; initialized with `bd init --stealth -p mosh` (tracker excluded via .git/info/exclude) instead of stopping, since the user requested the run explicitly at launch.
- [pre-flight] `.worktrees/` ignored via .git/info/exclude instead of committing to upstream-owned .gitignore (fork minimal-diff rule).
- [pre-flight] docs/* is upstream-gitignored; run-directory files are force-added (git add -f) on every commit.
- [phase 2] design roast rung: Workflow
- [phase 2] round 1 roast ran without --review-readiness (caller omission) → [low coverage] qualifier; supplied from round 2
- [phase 2] parallelism pass: graph-pass judge not dispatched because graph-shape reported no candidate that reduces depth (all 5→5)
- [phase 2] design roast converged at round 2 with 8 sub-Blocking findings; all 8 applied inline as spec+bead amendments (punch list "file or decline": chosen to fix at design time, not re-roasted)
- [phase 3 prep] zig 0.16.0 was missing (vendored libghostty-vt build dep); installed official release to ~/.local/opt (sha256 verified) and symlinked ~/.local/bin/zig. bun missing → just check's maintenance-test unavailable; sweep = cargo nextest + clippy -D warnings + fmt --check
- [phase 3 prep] .superpowers/ (super-code workspace) excluded via .git/info/exclude, not .gitignore (fork minimal-diff)
