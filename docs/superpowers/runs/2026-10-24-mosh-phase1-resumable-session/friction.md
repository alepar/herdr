# friction log — 2026-10-24-mosh-phase1-resumable-session
- [pre-flight] repo had no beads tracker; initialized with `bd init --stealth -p mosh` (tracker excluded via .git/info/exclude) instead of stopping, since the user requested the run explicitly at launch.
- [pre-flight] `.worktrees/` ignored via .git/info/exclude instead of committing to upstream-owned .gitignore (fork minimal-diff rule).
