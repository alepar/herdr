# Final review: cluster 4 (run-docs-inputs)

Epic: mosh-dqw. Branch: super-auto/mosh-phase1-resumable-session.
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`). Caller checkout was at HEAD 2bd5303c with a clean status when the review started. All reads used `git show <sha>:<path>`, and no worktree was created.

## Scope

These six files are under `docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/inputs/` (slices 000023-000028):

- `herdr-mosh-touch-map.md` (498 lines): the research touch map and minimal-diff strategy that the spec was built from.
- `herdr-remote-report.md` (354 lines): background report on herdr's remote transport.
- `mosh-report.md` (506 lines): background report on mosh, ET and related tools.
- `task-tree-r1.json`, `task-tree-r2.json`, `task-tree-amend1.json` (538/538/539 lines): snapshots of the bd task tree for the epic.

All six files are archived inputs to the run. None of them is product code or the authoritative spec. Per the epic, the spec is `2026-10-24-mosh-phase1-resumable-session-design.md`, which belongs to cluster run-docs-root. I judged this cluster on four things: whether the files are well-formed, whether their citations are accurate, whether they expose anything sensitive, and whether the snapshots are consistent with each other.

## Checks performed

- JSON validity: all three task-tree files parse as JSON arrays of 12 bd issues (the epic plus mosh-dqw.1..11) with the standard bd fields.
- Provenance by commit: the three reports were added in 65e5fbf9 ("start super-auto run"). `task-tree-r1.json` was added in 0a065435 (design roast round 1), `task-tree-amend1.json` in 8199968f (coverage-amend round) and `task-tree-r2.json` in d5bc2621 (start design roast round 2). None of these files changed after it was added.
- Snapshot consistency: `task-tree-amend1.json` has the same content as `task-tree-r1.json`. A field-by-field comparison finds no differences, and the raw diff is one trailing newline. So the snapshot added with the coverage-amend commit is the tree as it was *before* the amendment, and the amended tree appears in `task-tree-r2.json`. That file adds the R14/R15/R18 combined-branch checks to mosh-dqw.11, has updated_at 06:29:48Z (the time of the amend commit) and carries the extra dependency edges on .7 and .10. This fits "inputs" meaning "the tree fed into that round", but the file name is easy to misread.
- Snapshot against live beads: the descriptions in `task-tree-r2.json` match live bd for mosh-dqw.1, .10 and .11, but not for .4 and .7. That fits run.md's note that the round-2 step-back punch list was "applied inline to spec and beads" after the snapshot. The snapshots are historical and are not maintained.
- Citation spot-check of the touch map against base 5608de06: these citations all match the code at base. `SshStdioBridge::start(` at `attach.rs:90`; the `SshStdioBridge` field list at `attach.rs:2996-3002`; `SshStdioBridge::start(` at `saved.rs:35`; the manifest consts at `update.rs:25-26`; the `endpoint_capabilities` vec at `cli/status.rs:298-302`; the `run_remote_client_bridge` match at `host.rs:7-21`, with `ensure_remote_server_running` before `_ssh_agent`. The hooks on the branch sit exactly at those anchors.
- Sensitive content: none found. I found no tokens, keys or credentials. The only email in the task-tree JSON is the owner/creator email, and it is the same address already in the commit author metadata. The reports contain absolute developer paths (`/var/home/alepar/AleCode/herdr` as the citation root), which is a cosmetic issue and not a leak of anything secret.
- Divergence from the final spec: the touch map predates the decisions that the spec records. Examples: "about 8" upstream lines against the spec's 11-line touch list; "No outage indicator" listed as an accepted Phase 1 regression, while D5 pulled the banner into Phase 1; and a `[o]` guard arm for host.rs, while the branch uses `[o, ..]`. These are expected for a pre-design input. The files carry no "superseded by spec" marker, though.

## Findings

No defect in this cluster blocks landing. The files are archived run inputs, they are well-formed, and the citations I checked are accurate. Nothing in them is read by code or tests.

## Verdict
ready

## Must fix before landing
- none

## Untested scope
- External claims about mosh, Eternal Terminal and related tools in mosh-report.md (protocol details, buffer sizes, behaviour) were not independently verified; they are background only and no product behaviour depends on them.
- Only a sample of the touch-map and herdr-remote-report file:line citations (attach.rs:90, attach.rs:2996-3002, saved.rs:35, update.rs:25-26, status.rs:298-302, host.rs:7-21) was checked against base 5608de06; the remainder are unverified.

## Deferred OK
- task-tree-amend1.json has the same content as task-tree-r1.json (the tree before the amendment), and the amended tree lives in task-tree-r2.json; a one-line README or rename in inputs/ would stop readers from taking amend1 as the post-amendment tree.
- Task-tree snapshots predate the round-2 step-back punch list (live mosh-dqw.4 and mosh-dqw.7 descriptions differ from task-tree-r2.json); live bd is authoritative.
- herdr-mosh-touch-map.md has no superseded-by-spec marker and differs from the final spec in places (about 8 against 11 upstream lines, Phase 1 "no outage indicator" against the D5 banner, `[o]` against `[o, ..]` host.rs guard); a header pointing to the design doc would prevent misuse.
- herdr-mosh-touch-map.md and herdr-remote-report.md cite an absolute developer path (/var/home/alepar/AleCode/herdr) as their citation root; this is cosmetic and could be made repo-relative.
