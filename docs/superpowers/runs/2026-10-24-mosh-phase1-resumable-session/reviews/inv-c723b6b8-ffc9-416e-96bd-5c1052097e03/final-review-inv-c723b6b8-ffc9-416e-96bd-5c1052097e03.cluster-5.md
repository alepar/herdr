# Final review: cluster 5 (fork-hooks), epic mosh-dqw

Invocation: inv-c723b6b8-ffc9-416e-96bd-5c1052097e03
Source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`; review-source binding recorded in the ledger, exit 0).
Caller checkout: HEAD 2bd5303c, `git status --short` clean before and after the review. The caller checkout was not switched or edited. A review-owned detached worktree was used for reproductions and was removed afterwards.

Scope: scripts/mosh-diff-budget.sh, src/cli/status.rs, src/client/mod.rs, src/client/shell/composition.rs, src/remote/attach.rs, src/remote/host.rs, src/remote/saved.rs, src/update.rs (slices 000031-000034, 000045-000048), checked against the spec's "Upstream touch list", the hook sections of §Outage banner, §Versioning, §Discovery, the acceptance criteria, and the epic's Global Constraints.

## Evidence (observed at head, review worktree)

- Upstream footprint: `git diff 5608de06..2bd5303c` outside the fork paths touches exactly the 7 files in the touch list: 12 insertions, 4 deletions. Every inserted line carries `// mosh-fork hook`. The spec's appendix note (line 462) accounts for status.rs being 2 lines (12 lines total, not 11). That settles the conflict parked in mosh-dqw.10 against its bead's "exactly 3".
- No hot-file edits: headless.rs, client_transport.rs, handshake.rs, wire.rs, app/mod.rs and main.rs are untouched, and Cargo.toml and Cargo.lock are unchanged.
- `bash scripts/mosh-diff-budget.sh`: prints `12 inserted upstream lines (budget 20), 0 untagged hunks` and exits 0.
- Synthetic failure: appending `// probe` to src/update.rs gives `untagged hunk: src/update.rs @@ -4314,0 +4315 @@` and exit 1. Reverted.
- `cargo fmt --check`: exit 0. `cargo clippy --locked --all-targets -D warnings`: clean.
- `cargo test --locked --test mosh_status`: 1 passed. `cargo test --bin herdr -- mosh_outage remote::attach::mosh::remote:: update:: cli::status`: 140 passed. `cargo test --test remote_attach`: 1 passed (the suite has one test and is unchanged).
- Manifest override: a build with `HERDR_STABLE_UPDATE_MANIFEST_URL=https://example.invalid/latest.json` embeds that URL once. Rebuilding without the variable removes it, so cargo tracks the `option_env!` dependency.
- Capability drift guard: `remote::tests::status_hook_advertises_capability` checks that status.rs contains `"<STATUS_CAPABILITY>", // mosh-fork hook`. That ties the hardcoded literal in status.rs to `src/mosh/mod.rs::STATUS_CAPABILITY`.

## Hook-by-hook

- attach.rs mount and `start_ui_bridge(&remote_ssh, ...)`, and saved.rs `start_ui_bridge(&ssh, ...)`: both match the touch list. `#[rustfmt::skip]` on the multi-line `let` freezes upstream's following argument lines, so the merge surface stays at one line. Visibility is correct: `pub(super) mod mosh` in attach is visible to `crate::remote::saved`.
- host.rs match arm: routes any `--x-mosh-*` flag to `idle_timeout=false`. Dispatch comes after `ensure_remote_server_running` and the ssh-agent registration, as the spec says ("after l.24"). `dispatch` has a catch-all arm that returns an `InvalidInput` error for any other `--x-mosh-*` flag, and on non-Unix for every such flag. So an unknown fork flag can never fall through to a plain pump. The keeper is spawned with `env_remove("SSH_AUTH_SOCK")`, so `Registration::start()` returns None for it.
- status.rs: `#[cfg(unix)]` on a `vec!` element is valid. The capability is Unix-only, as the spec requires.
- client/mod.rs hook 1: sits at the top of the `EndpointControl` arm, after the loop's `write_stream.accepts`/`received` gating, so a link frame still counts as liveness and stale generations are dropped. The repaint uses the same `shell.compose(reported_size) -> present_frame` pattern as the arm's other UI branches. `continue` keeps `LINK_KIND` out of `decode_endpoint_control`. This matches the arm's existing early-`continue` style, so nothing after the match is skipped that the arm doesn't already skip.
- composition.rs hook 2: placed after the endpoint-status block and before `compose_graphics`, passing `endpoint_label(..)`, as amended in the spec.
- update.rs: a `match option_env!(..)` with a `!url.is_empty()` guard, which is const-evaluable. An empty value falls back to today's URLs. Two lines, tagged.
- mosh-diff-budget.sh: it diffs from the merge base rather than `origin/master` directly. That is the better reading of the spec once upstream is merged in, because commits that exist only upstream are not counted. Excludes, the stat/grep output, the 20-line budget, per-hunk tags and the exit codes match the bead.

## Observations (none blocking)

- Remote install still uses upstream's manifest. src/remote/attach.rs:47-48 hardcode `https://herdr.dev/{latest,preview}.json` for `remote_release_asset` (the `herdr --remote` remote-install path). A fork build with the override variables set still installs an upstream binary on a remote that lacks herdr or has a mismatched one. That remote then lacks `x-mosh-resume-bridge-v1`, so the connection silently takes the plain bridge (with the hint line). The spec scopes the release blocker to self-update in update.rs, so this is not a spec miss. Task 3's report flags it as a follow-up, but no follow-up bead exists.
- Budget script parser misreads one rare line shape. In `-U0` output, an added line whose content starts with `++ ` appears as `+++ ...`, which the awk reads as a file header. Reproduced: `++ x // mosh-fork hook` appended to update.rs is reported as `untagged hunk:  @@ ...` with an empty file name, and the script exits 1. It errs toward failing (a false positive, never a false pass) and is rare in Rust.
- Budget script skips untracked files. Untracked upstream-path files are not audited. Reproduced: a new untracked `src/probe_untracked.rs` leaves the result at exit 0. The header says uncommitted edits are audited, which holds only for tracked files. At upstream-merge time everything is committed, so the practical risk is low.
- Budget script `git grep` output is noisy. It is not limited to upstream paths: 39 of its 62 output lines come from docs/superpowers, HERDR_MOSH_VISION.md, src/mosh and tests/mosh_*. That buries the 12-line touch surface the spec says the grep should list.
- Banner can be covered in two cosmetic cases. The banner row is not added to `occlusion`, so `compose_graphics` can still place a graphics placement on the top row over the banner. Also, the endpoint stays Online during an outage, so the cursor is not cleared and can land on the banner row.

## Verdict
ready

## Must fix before landing
- none

## Untested scope
- src/update.rs manifest override has no automated test (the bead forbids an upstream test module); checked only by manual `strings` inspection (task 3 and this review).
- scripts/mosh-diff-budget.sh has no automated test; the untagged-hunk and over-budget failures are manual synthetic checks only, and the deletion-only-hunk rejection is not exercised at all.
- src/remote/host.rs `--x-mosh-*` routing of an unknown flag through the real binary (`herdr remote-client-bridge --x-mosh-bogus-v9`) is covered only by the in-process `dispatch` unit test; the session-flag path is exercised end to end by the tests/mosh_* suites.
- The non-Unix side of the hooks (status.rs `#[cfg(unix)]` element, dispatch's non-Unix error arm) is not compiled in CI here (only x86_64-unknown-linux-gnu is installed).
- composition.rs banner paint over graphics placements and under a visible cursor on row 0 is not tested.

## Deferred OK
- src/remote/attach.rs:47-48 remote-install manifest URLs ignore the fork override, so remotes get upstream binaries and silently fall back to the plain bridge; outside the spec's touch list, needs a follow-up bead (2 more tagged upstream lines).
- scripts/mosh-diff-budget.sh counts inserted lines only; deletions are not budgeted (deferred minor from mosh-dqw.3).
- scripts/mosh-diff-budget.sh misparses an added line starting with `++ ` as a file header (fail-safe false positive).
- scripts/mosh-diff-budget.sh does not audit untracked files despite its "uncommitted edits are audited" header.
- scripts/mosh-diff-budget.sh `git grep -n mosh-fork` is not restricted to upstream paths and lists 39 fork-file lines beside the 12 hook lines.
- src/client/shell/composition.rs banner row is not added to `occlusion`, and the cursor is not cleared while resuming (cosmetic).
- src/remote/host.rs unknown `--x-mosh-*` flag is rejected only after `ensure_remote_server_running()` (deferred minor from mosh-dqw.10).
- src/update.rs and the hook lines use `#[rustfmt::skip]` to keep one statement per line (deferred minor from mosh-dqw.3; intentional for merge surface).
- src/mosh/outage.rs 2 s stale clear only takes effect on the next repaint (deferred minor from mosh-dqw.2).
