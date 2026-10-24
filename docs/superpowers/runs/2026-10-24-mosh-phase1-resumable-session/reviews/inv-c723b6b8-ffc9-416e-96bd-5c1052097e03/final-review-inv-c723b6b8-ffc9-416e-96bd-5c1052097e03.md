# Final seam review: epic mosh-dqw (Mosh Phase 1: resumable remote session)

Invocation: inv-c723b6b8-ffc9-416e-96bd-5c1052097e03
Branch: super-auto/mosh-phase1-resumable-session
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931. I verified both with `git rev-parse --verify <sha>^{commit}`.
Review source binding: `dispatch-text.mjs bind-review-source` exited 0 before this file was written. The artifact did not exist beforehand.
Caller checkout observed: HEAD 2bd5303c and a clean `git status --short`, both at the start and at the end. I did not switch or edit it.
Reproductions ran in a detached worktree that this review owned, under the scratchpad (`seam-review/wt`), with its own CARGO_TARGET_DIR and TMPDIR. I removed it afterwards.

## Inputs

- The eight cluster reviews (cluster-1 through cluster-8).
- `assembled/.../clusters.json`.
- `bd show mosh-dqw --json`: the global constraints and the key-abstractions digest.
- The spec at head.
- The cross-cluster hunks below. I did not re-review any cluster's own files.

## Cross-cluster interfaces checked

- **fork-hooks to mosh-core, plain-vs-resume swap.** `src/remote/attach.rs` and `src/remote/saved.rs` call `mosh::start_ui_bridge` with the same arguments they used to pass to `SshStdioBridge::start`. The plain fallback inside `start_ui_bridge` rebuilds upstream's command exactly, including `--idle-timeout-v1` for noninteractive saved machines. The resume path does not (see Finding 1).
- **fork-hooks to mosh-remote, flag routing.** In `src/remote/host.rs`, the arm `[o, ..] if o.starts_with("--x-mosh-")` and the dispatch hook after `ensure_remote_server_running` and the ssh-agent registration agree with `mosh::remote::dispatch`. Dispatch accepts exactly `[SESSION_FLAG]` and `[KEEPER_FLAG]` and rejects everything else. `SESSION_FLAG` and `KEEPER_FLAG` are defined once, in `src/mosh/mod.rs`. Consistent.
- **fork-hooks to mosh-core, status capability.** `src/cli/status.rs` hardcodes `"x-mosh-resume-bridge-v1"`, which matches `STATUS_CAPABILITY` in `src/mosh/mod.rs:27`. The test `status_hook_advertises_capability` pins the two together. Consistent.
- **fork-hooks to mosh-core, outage banner.** The `client/mod.rs` hook calls `mosh_outage::observe(&endpoint_id, &kind, &data)`. `link_frames.rs` imports `LinkStatus` and `LINK_KIND` from that same module (`crate::client::mosh_outage`), so there is one definition of the payload, as the digest requires. The `composition.rs` hook passes the 3-arg `paint(frame, active_endpoint_id, endpoint_label)`, which matches `outage.rs`. Consistent.
- **mosh-core to mosh-remote, shared `SessionEndpoint`.**
  - The keeper (`keeper.rs:508`, `:973`) and the local bridge (`local_bridge.rs:580`, `:1478`) both drive `SessionEndpoint::on_leg_bytes`. The cluster-6 exactly-once defect is therefore a two-sided seam: on the keeper side it loses upstream bytes headed for the herdr server, and on the bridge side it loses downstream bytes. One fix in `session.rs` covers both.
  - The keeper re-queues `END{server-eof}` on resume (`keeper.rs:445-449`). The local bridge has no equivalent for `END{client-gone}` or `END{ring-overflow}` (cluster 6, Finding 2). The two halves of the same protocol rule were built asymmetrically.
  - The keeper removes an eof session only after `fully_acked()` (`keeper.rs:267`). The bridge does send a best-effort final ACK (`local_bridge.rs:1056`, `final_ack`), so cluster 7's "the bridge exits before that ACK" is only partly true. The ACK is sent *after* `close_client(true)`. In `--remote` mode the bridge thread lives in the client process, and that process can exit as soon as its stream closes. So the ordering is racy by construction, even though cluster 8's 10-round probe saw `session_end{server-eof}` within about 60 ms every time. Sending `final_ack` before `close_client` would close the race.
- **Ring-cap policy across the two ends.** The local side always feeds the client reader while down, and overflows and ends the session at 8 MiB. The keeper's `may_read_source()` gate back-pressures the server at exactly 16 MiB instead of overflowing. Both are bounded by grace, but they contradict spec §Ring caps on the keeper side. Cluster 2 already requires this to be recorded.
- **mosh-remote test hygiene to mosh-core config.** `keeper.rs:1337` sets `HERDR_MOSH_GRACE=3600` process-wide, and `mosh::get()` caches its config in a `OnceLock`. Today the only caller of `get()` is `start_ui_bridge`, and no unit test calls it, so no test is affected yet. This is a latent coupling, not a live failure.
- **fork-hooks (update.rs) to remote install.** The fork's manifest override does not reach the remote-install URLs (`attach.rs:47-48`). A fork-built `--remote` install therefore puts an upstream binary on the remote, which then silently takes the plain bridge (cluster 5). This is a requirement that falls between the update hook and the resume feature. It is out of the spec's touch list, so it is deferred to a follow-up.

## Findings

### 1. The saved-machine resume path drops upstream's `--idle-timeout-v1`, which breaks an upstream test (Must fix, reproduced)

This is the seam behind cluster 3's finding. Three clusters each saw a correct piece:

- `src/remote/saved.rs:35` (fork-hooks) now routes saved-machine discovery through `start_ui_bridge`.
- When the remote advertises the capability, `start_ui_bridge` dials `remote-client-bridge --x-mosh-session-v1` through `SshLegDialer` (mosh-core). It applies the idle-timeout opt-in only to `plain_command` (`src/mosh/mod.rs:153-158`).
- `ssh_leg.rs:649` asserts that the session command carries no idle-timeout flag.
- `host.rs` accepts `--idle-timeout-v1` only as a lone argument, so the two flags cannot simply be combined.

Reproduced in the review worktree:

| Commit | `cargo test --locked --test client_mode federated_client_starts_without_local_and_survives_its_restart` |
|---|---|
| head 2bd5303c | FAILED at `tests/client_mode.rs:1083`: "saved machine discovery must opt into the advertised bridge idle timeout" |
| base 5608de06 | ok (1 passed) |

Why this cannot land as is:

- The spec requires the full test suite to pass.
- The epic forbids editing upstream test modules.
- A spec exception alone would leave the suite red.

Task 11's "15 failures, all pre-existing" claim was measured against 992f6a45, which already contains the fork. Against the run-start baseline (dec51b74, whose product tree equals the base), this failure is new.

The resume path has to keep upstream's saved-machine idle-timeout semantics in a way that is visible in the ssh command line. Alternatively, saved machines must stay on the plain bridge, but that narrows the feature's scope and needs a design decision.

### 2. Combined suite reliability (carried and compounded)

Each cluster tested its own flakes in isolation. Once merged, the run's own branch suite has:

- the keeper flake, 5 of 20 runs (cluster 7, must fix);
- the local-bridge `outage_overflow_ends_session` flake, 1 of 4 runs under load (cluster 6, untested scope);
- the slow 90-100 s ring-overflow integration test (cluster 8).

The "full test suite passes" acceptance criterion depends on all of them together.

## Verdict
not ready (saved-machine resume path drops --idle-timeout-v1 and fails upstream client_mode test reproduced at head, plus cluster must-fixes: exactly-once byte loss in SessionEndpoint, local bridge never re-sends lost final END, flaky keeper test, undocumented spec divergences)

## Must fix before landing
- Restore upstream saved-machine `--idle-timeout-v1` semantics on the resume path (src/mosh/mod.rs start_ui_bridge plus ssh_leg session command, with src/remote/host.rs arg routing), or keep saved machines on the plain bridge by explicit design decision, so tests/client_mode.rs federated_client_starts_without_local_and_survives_its_restart passes again; reproduced failing at head 2bd5303c and passing at base 5608de06 (seam of clusters fork-hooks, mosh-core and run-docs-evidence).
- Redo the end-of-run regression comparison against the run-start baseline dec51b74 (product tree equal to base), not 992f6a45 which already contains the fork; task-11-report.md:49 "all 15 pre-existing" is false for the client_mode test (cluster 3).
- src/mosh/session.rs:494-574 on_leg_bytes advances recv_offset for DATA decoded earlier in a buffer and then drops those Deliver events when a later message in the same buffer is a protocol error; bytes are never delivered or replayed, breaking exactly-once in both directions because keeper.rs and local_bridge.rs share this path (cluster 6).
- src/mosh/local_bridge.rs:970-988 leg_dead never re-queues END{client-gone} or END{ring-overflow} once poll_send consumed it, so after a resume the run stays Linked with the client closed and the keeper session lives on, unlike keeper.rs:445-449 which re-queues server-eof (cluster 6).
- src/mosh/remote/keeper.rs:1195 server_eof_forwards_remaining_bytes_then_end flakes 5 of 20 runs because the test local stops pumping at END and never flushes the final sub-64 KiB ACK; keep pumping or force the final ACK before asserting session_end (cluster 7).
- Append dated Post-Implementation Notes to the design spec for keeper exact-16-MiB back-pressure versus the §Ring caps overflow rule, resume.replayed_down emitted as null, reconnect-leg ssh spawn failure classified Fatal, and the graceful --remote quit versus server hang-up final-ACK race, or change the code so the spec holds (cluster 2).

## Untested scope
- The client_mode idle-timeout regression was reproduced only for that one test; no full-suite run at head against the dec51b74 baseline was made in this review, so other fork-caused upstream test regressions are not excluded.
- No end-to-end test sends valid DATA followed by an invalid message in one read on either the keeper or the local side (cluster 6).
- No test covers a session-final END emitted to a leg that dies before flush followed by resume (cluster 6).
- local_bridge::tests::outage_overflow_ends_session failed 1 of 4 full mosh-suite runs under load (cluster 6).
- Graceful `--remote` quit (prefix+q) has no 2 s keeper session-end assertion; final_ack runs after close_client, so the race is unguarded although a 10-round probe did not reproduce it (clusters 7 and 8).
- AC15 is evidenced by 16 x 1 MiB pastes checked by size with a 256 KiB tolerance; the 2 MiB in-flight bound is unit-only and no single 16 MiB clipboard image is sent (clusters 2 and 8).
- "No ClientShellSnapshot or full PaneSurface after resume" is evidenced only by the server_connect-count proxy (cluster 2).
- Leg-0 ssh stderr diagnostics printing over the TUI during an outage still needs the human ssh check parked in run.md (cluster 2).
- Non-Unix local and Windows-remote gates (status.rs cfg element, dispatch non-Unix arm, start_ui_bridge non-Unix branch, peer_uid on other unixes) are inspection-only (clusters 5, 6, 7).
- Keeper per-HELLO table cells resume:false on parked or eof sessions, resume:true on eof with live leg, the keeper silence verdict, attacher END{keeper-unavailable} exit path, spawn and lock-open errors, out-of-ring resume offset and accept-error handling have no tests (cluster 7).
- Leg stealing on resume, eviction via a second `--remote` connection and END before WELCOME on a reconnect leg are unit-only, not end to end (cluster 8).
- src/update.rs manifest override and scripts/mosh-diff-budget.sh have no automated tests (cluster 5).
- Leg teardown joining the stderr thread with a stderr-holding ProxyCommand grandchild, and graceful close_client with a never-reading client, are untested (cluster 6).
- Banner paint under graphics placements or a visible cursor on row 0 is untested (cluster 5).
- No automated check of docs/superpowers/specs/INDEX.md links or vision path:line citations (cluster 1).

## Deferred OK
- Move local_bridge final_ack ahead of close_client(true) so the keeper's eof-session removal does not depend on the client process outliving its closed stream.
- keeper.rs:1337 hello_grace_wins_over_env sets HERDR_MOSH_GRACE process-wide while mosh::get() caches config in a OnceLock; no current test calls get(), but any future one would silently read 3600 s.
- src/remote/attach.rs:47-48 remote-install manifest URLs ignore the fork override, so fork installs put upstream binaries on remotes that silently take the plain bridge; needs a follow-up bead (cluster 5).
- Keeper keeps an eof session until grace or eviction if the final ACK is lost (cluster 7).
- Keeper accept errors other than WouldBlock or Interrupted end the process and every session (cluster 7).
- Keeper resume with an out-of-ring peer offset drops the leg unanswered (cluster 7).
- Keeper end_sent set before END bytes are written, narrow lost-END window recovered via resume unknown (cluster 7).
- Attacher bring-up failure prints a debug-formatted Error line on remote stderr before the plain fallback (cluster 7).
- current_build_id falls back to version only when HERDR_BUILD_ID is unset (cluster 7).
- Keeper child never reaped by the attacher, is_end by type byte only, large keeper test module (cluster 7).
- Unknown --x-mosh-* flag rejected only after ensure_remote_server_running (clusters 5 and 7).
- ssh_leg dial_command maps any ssh spawn failure to Fatal on reconnect legs (cluster 6; recorded in spec per must-fix).
- local_bridge::start re-implements upstream start_command's bind and accept loop (drift risk), ReplayRing and Decoder efficiency, ssh_leg.rs rustfmt drift, local_bridge.rs size, OutageState stale clear on next repaint (clusters 5 and 6).
- scripts/mosh-diff-budget.sh: insertions-only budget, `++ ` line misparse, untracked files not audited, unrestricted git grep output (cluster 5).
- composition.rs banner row not added to occlusion and cursor not cleared while resuming; `#[rustfmt::skip]` on hook lines (cluster 5).
- Spec stale text: Components path src/client/mosh_outage.rs, duplicated Hook 2 paint signature, touch-list total 11; findings-ledger-design.md at r1; acceptance checklist unticked; absolute local paths in run.md and roast-design-2.md (cluster 2).
- Run-start baseline `[exited with code 0]` wrapper trailer (cluster 3).
- inputs/: task-tree-amend1.json equals r1, snapshots predate round-2 punch list, touch map lacks superseded marker, absolute developer paths (cluster 4).
- HERDR_MOSH_VISION.md drift: footprint 12 lines, outage.rs mount and test files, HELLO fields, env var naming rule, Phase 5 narrowing, review-surface command exclusion; INDEX.md status draft (cluster 1).
- Slow ring-overflow integration test not ignored, harness Linux-only prerequisites, mosh_status caps.len() == 4 brittleness, weak paste integrity check, cosmetic harness doc drift (cluster 8).
