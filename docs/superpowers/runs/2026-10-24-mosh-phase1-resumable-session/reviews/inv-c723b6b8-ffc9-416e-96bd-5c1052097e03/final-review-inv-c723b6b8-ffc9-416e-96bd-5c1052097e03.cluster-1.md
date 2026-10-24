# Final review: cluster 1 (docs)

Invocation: inv-c723b6b8-ffc9-416e-96bd-5c1052097e03
Epic: mosh-dqw (Mosh Phase 1: resumable remote session)
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`). Review-source binding via dispatch-text.mjs bind-review-source succeeded (exit 0).
Caller checkout observed at the start: branch super-auto/mosh-phase1-resumable-session, HEAD 2bd5303c, status clean. No caller files were edited.

## Scope

Cluster files (slices 000001, 000030):

- `HERDR_MOSH_VISION.md` (new, 252 lines): the fork-wide plan. The epic names it the "Fork plan", and the spec cites its §4.1, §5 and §6.
- `docs/superpowers/specs/INDEX.md` (new, 5 lines): the specs index, with one entry pointing at the Phase 1 design.

The reference is the epic description (`bd show mosh-dqw --json`) and the spec `docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/2026-10-24-mosh-phase1-resumable-session-design.md` at head. I opened other files only to check specific claims the vision makes: the upstream hook diff in `src/{cli,client,remote}` and `src/update.rs`, the `Hello` struct in `src/mosh/session.rs`, `keeper_hash` in `src/mosh/remote/mod.rs`, the exclusions in `scripts/mosh-diff-budget.sh`, and `.gitignore`.

## Spec conformance

- The spec's diff-scope criterion excludes `HERDR_MOSH_VISION.md` and `docs/superpowers/` from the upstream budget. `scripts/mosh-diff-budget.sh` excludes both paths (lines 36-37), so neither cluster file counts against the 20-line budget. Conforms.
- `.gitignore` ignores `/docs/*` upstream. `docs/superpowers/specs/INDEX.md` is tracked at head (`git ls-tree`), so it was force-added, as the epic's "git add -f for run artifacts" constraint expects. Conforms.
- The INDEX link `../runs/2026-10-24-mosh-phase1-resumable-session/2026-10-24-mosh-phase1-resumable-session-design.md` resolves from `docs/superpowers/specs/` to a file at head. Its summary (local bridge plus keeper, exactly-once input, outage banner) matches the spec's goal statement.
- On the core Phase 1 architecture, the vision agrees with the spec and the code:
  - link-level design B;
  - the attacher/keeper split;
  - `x-mosh-resume-bridge-v1`, `--x-mosh-session-v1` and `x-mosh.link.v1`;
  - 60 s client-set grace;
  - 2 s heartbeat and 8 s silence;
  - back-off of 1, 2, 4 and 8 s;
  - BatchMode reconnect legs that bypass the ControlMaster;
  - an 8 MiB local cap;
  - the `HERDR_MOSH_RESUME=0` kill switch;
  - the fallback to the plain `SshStdioBridge`;
  - the `#[path]` mount line in `attach.rs` and the `// mosh-fork hook` tagging;
  - no Cargo changes.

## Findings

No defect in this cluster blocks landing. Both files are documentation, and nothing in the product reads or enforces their content. The findings below are places where the vision doc no longer matches the shipped Phase 1 and the spec. That matters because later phases will use the vision's budgets and rules for planning.

1. **The vision's upstream footprint figures are stale (minor, doc drift).**
   - Shipped: `git diff --stat` over the seven upstream files shows 12 insertions and 4 deletions. The spec (line 462) records 12 lines.
   - The vision says three different, lower things:
     - §4.1 says "about 8 lines in existing files";
     - the §6 conflict budget says "Phase 1 about 10" total, with "1 (`client/mod.rs`, banner)" in the hottest files;
     - the §6 touch list gives `src/client/mod.rs` 1 line and `src/cli/status.rs` 1 line.
   - Shipped per file: `client/mod.rs` has 2 lines (the `#[path = "../mosh/outage.rs"] pub(crate) mod mosh_outage` mount plus the EndpointControl hook). `status.rs` has 2 lines (rustfmt keeps `#[cfg(unix)]` on its own line).
   - The touch-list text for `host.rs` shows the arm as `[o] if o.starts_with(...)`. The shipped arm is `[o, ..] if ...`, as the spec has it.
   - Phase 3a and Phase 5 budgets are extrapolated from these baselines, so they understate the footprint by the same amount.

2. **The vision's new-files table and mount rule omit the client-side fork module (minor, doc drift).**
   - §6 rule 2 says "Phase 1 uses one line in `src/remote/attach.rs`". It adds that "later phases" will mount fork files under other owners.
   - Phase 1 already mounts `src/mosh/outage.rs` from `client/mod.rs`. The spec and the epic constraints mandate this, because the `mosh` tree is `pub(in crate::remote)` and is not visible to `client`.
   - The "Phase 1 new files" table omits `src/mosh/outage.rs`. It also omits the test files that shipped: `tests/mosh_harness.rs`, `tests/mosh_fallback.rs`, `tests/mosh_integration.rs` and `tests/mosh_status.rs`.

3. **The vision's session-protocol description contradicts the spec (minor, doc drift).**
   - §4.1 says hello and welcome carry `(token, recv_offset, grace, build_id, previous_token)`, with messages "chunk, ack, heartbeat, end".
   - The spec (rule 8) says "HELLO carries no build id". The shipped `Hello` struct (`src/mosh/session.rs:152-160`) is `{v, token, resume, recv_offset, grace_ms, previous_token}`. The build id lives in the keeper rendezvous hash instead (`keeper_hash`, `src/mosh/remote/mod.rs:111`).
   - The vision also leaves out `v` and `resume`, and calls DATA "chunk".
   - A Phase 3a implementer who reads only the vision would assume the HELLO is gated on build id.

4. **The naming rule in §6 rule 7 is contradicted by the vision itself and by the shipped code (minor, doc inconsistency).**
   - Rule 7 says "Prefix every fork-introduced name with `x-mosh`: kinds, capabilities, flags and env vars".
   - The vision's own §4.1 uses `HERDR_MOSH_RESUME` and `HERDR_MOSH_GRACE`. The shipped code adds `HERDR_MOSH_TRACE`, plus `HERDR_STABLE_UPDATE_MANIFEST_URL` and `HERDR_PREVIEW_UPDATE_MANIFEST_URL` in `src/update.rs`. The last two carry no mosh marker at all, deliberately, since §7 item 4 calls the seam upstreamable.
   - The code is reasonable: env var names cannot contain hyphens. The rule should say `HERDR_MOSH_*` for env vars and exempt the upstreamable update-manifest seam.

5. **The phase plan still lists the outage banner as Phase 5, but D5 moved it into Phase 1 (minor, doc inconsistency).**
   - The §5 row "5 | Outage banner | Outage visible within the heartbeat interval" and the §4.2 Phase 5 bullet ("A 'last contact N s' banner, carried as a status payload on `x-mosh.link.v1`") describe what Phase 1 shipped: `LinkStatus::Resuming{last_contact_ms}` and `OutageState::paint`.
   - D5 in §8 says "Decided: in Phase 1".
   - What actually remains for Phase 5 is only the blind-typeahead visibility. Phase 5 should be narrowed to that.

6. **The review-surface command in §6 rule 1 differs from the budget script and the spec (minor).**
   - The command excludes `src/mosh`, `tests/mosh_*`, `scripts/mosh-*` and `HERDR_MOSH_VISION.md`, but not `docs/superpowers`.
   - The spec's acceptance criterion and `scripts/mosh-diff-budget.sh` also exclude `docs/superpowers/`. Run by hand, the documented command therefore lists every force-added run artifact as "upstream" surface.

7. **The INDEX entry's status is `draft` (nit).** The design it links is fully implemented on this branch and is being landed. When the branch lands, the status should be one that reflects that, such as "implemented".

## Verdict
ready

## Must fix before landing
- none

## Untested scope
- No automated link or path check covers `docs/superpowers/specs/INDEX.md` or the vision's `path:line` citations. The citations are pinned to upstream 5608de06 and will drift by design. I resolved the one INDEX link by hand.

## Deferred OK
- HERDR_MOSH_VISION.md §4.1 and §6: update the Phase 1 footprint to the shipped 12 upstream lines (2 in client/mod.rs, 2 in status.rs) and rebase the Phase 3a and Phase 5 conflict budgets on it.
- HERDR_MOSH_VISION.md §6 rule 2 and the new-files table: record the Phase 1 `#[path]` mount of src/mosh/outage.rs from client/mod.rs, and list outage.rs and tests/mosh_{harness,fallback,integration,status}.rs.
- HERDR_MOSH_VISION.md §4.1: correct HELLO to `{v, token, resume, recv_offset, grace_ms, previous_token}` (no build_id; build id is in the keeper rendezvous hash) and rename chunk to DATA, matching spec rule 8.
- HERDR_MOSH_VISION.md §6 rule 7: change env var naming to `HERDR_MOSH_*` and exempt the upstreamable HERDR_*_UPDATE_MANIFEST_URL seam.
- HERDR_MOSH_VISION.md §4.2 and §5: narrow Phase 5 to blind-typeahead visibility, since the last-contact banner shipped in Phase 1 per D5.
- HERDR_MOSH_VISION.md §6 rule 1: add `':!docs/superpowers'` to the review-surface command to match the spec and scripts/mosh-diff-budget.sh.
- docs/superpowers/specs/INDEX.md: change the Phase 1 entry status from `draft` to an implemented or landed status when the branch lands.
