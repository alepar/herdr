# Bounded final review: epic mosh-dqw (Mosh Phase 1: resumable remote session)

Invocation: inv-9350a065-92b6-4559-8456-5c7f474804bb
Branch: super-auto/mosh-phase1-resumable-session
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head a1232c13a4bfbec0528dc81848a70da4b0645525. I verified both with `git rev-parse --verify <sha>^{commit}`.
Review source binding: `dispatch-text.mjs bind-review-source` exited 0 before any artifact was written.
Caller checkout observed: HEAD a1232c13 with a clean `git status --short`, at the start and before writing. I did not switch or edit it. All reads came from captured Git (`git show a1232c13:<path>`, `git diff 5608de06..a1232c13`). I created no worktree and ran no build or test.

## Why this review is bounded

`review-inputs.json` reports status unavailable, sourceAvailable true, wholeRangeReady false, 0 omitted, 150 changed paths, and one gap: `a1232c13:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03: incomplete claimed collection`. I preserved the failed assembly and the original sidecar. I did not build a whole-range package.

The range has 150 changed paths. 124 of them are run documents under `docs/superpowers/runs/` (roasts, the review archive of the previous final review, slices, inputs). The other 26 are product, test, script and spec-index paths, 12,401 inserted lines in all. I inspected 16 of those 26 paths in full, plus parts of four large files (see Partial inspection). Every other path and hunk is unreviewed and listed under Untested scope.

## Inputs

- The sidecar and `diffstat.txt` under `assembled/final-review-inv-9350a065-...-final/`.
- The archived previous final review at head 2bd5303c (`reviews/inv-c723b6b8-.../final-review-inv-c723b6b8-....md`, read from captured Git at a1232c13). I used it as the list of earlier must-fixes to re-check.
- The design spec at head, partly: §Upstream touch list, §Acceptance criteria and every Post-Implementation Note.
- `git log 2bd5303c..a1232c13`: 22 non-merge commits since that review (the fix round 1 and code redesign cycle 1 beads mosh-dqw.19 to .30).
- The ledger `progress.md`, read only after I wrote my own findings below.

## Fully inspected

- All upstream hook hunks: `src/cli/status.rs`, `src/client/mod.rs` (2 hunks), `src/client/shell/composition.rs`, `src/remote/attach.rs` (2), `src/remote/host.rs` (2), `src/remote/saved.rs`, `src/update.rs`.
- New fork files: `src/mosh/mod.rs`, `src/mosh/remote/mod.rs`, `src/mosh/remote/attacher.rs`, `src/mosh/ring.rs`, `src/mosh/link_frames.rs`, `src/mosh/outage.rs`, `tests/mosh_status.rs`, `scripts/mosh-diff-budget.sh`, `docs/superpowers/specs/INDEX.md`.

## Partial inspection (not counted as covered)

- `src/mosh/session.rs`: lines 1-773, the whole non-test part. The test module (774-1899) was not read.
- `src/mosh/remote/keeper.rs`: lines 30-737, all of the non-test part except the imports. The test module was not read.
- `src/mosh/local_bridge.rs`: lines 30-52 and 420-1369, which cover the connection state machine, Closing and teardown. Lines 53-419 (start, the plain fallback, trace, serve_connection and type definitions) and the test module were not read.
- `src/mosh/ssh_leg.rs`: only `dial_command`, `classify_spawn_error` and the idle-flag command construction (about lines 330-470).

## Step 1: my own view (written before reading the ledger)

### Status of the previous final review's must-fixes, checked by inspection only

- **Saved-machine `--idle-timeout-v1` on the resume path: fixed in code.** `mod.rs` `idle_opt_in` gives the plain command and the resume leg the same rule. `SshLegDialer::new` builds `remote-client-bridge --idle-timeout-v1 --x-mosh-session-v1`. `remote/mod.rs` `route` accepts that order only. The `host.rs` arm `[o, ..] | [_, o]` admits it, and a Rust or-pattern guard is re-tested for each alternative, as the routing test `["junk", SESSION_FLAG] => Reject` relies on. The attacher passes `idle` on to upstream's `forward_remote_bridge_stdio`, whose watchdog resets on any byte in either direction, so 2 s session heartbeats keep a live leg alive. `select_with` copies the re-selected candidate's `remote_bridge_idle_timeout`, so the opt-in follows the binary that actually runs. The `client_mode.rs:1083` assertion is a substring check for `remote-client-bridge --idle-timeout-v1`, which the new command satisfies. Not re-run here. The same test then kills the bridge pid and reconnects three times, and that part now goes through the resume path; whether it passes is unmeasured.
- **Exactly-once loss in `on_leg_bytes`: fixed.** `LegBytesError.delivered` returns the `Deliver` events decoded before the error. Both callers hand them on before dropping the leg: keeper `run_leg` sends them to `to_server`, and the bridge's `on_leg_bytes` delivers them to the splitter.
- **Lost session-final END after a leg loss: fixed in the shared endpoint.** `final_end` is kept on the endpoint, and `final_end_sent` is reset in both `leg_down` and `set_up`, so either side re-sends the END on the next leg. The bridge's EndSent Closing resumes when the leg reaches EOF before the END write is flushed (`tick_closing`, "eof-before-end-flushed").
- **Final ACK versus client close (graceful `--remote` quit race): addressed by design.** For ServerEof, Phase C sends `ClientOut::Close` only after the owed ACK is written and the leg is done. No timing test was inspected.
- **Spec divergences: recorded.** Post-Implementation Notes exist for the keeper's exact-16-MiB back-pressure, the null `replayed_down`, the final-ACK race and the single Closing phase. The 12-line footprint is recorded too. The reconnect spawn-error note was superseded once `classify_spawn_error` made non-permanent errnos Retryable on reconnect legs, which matches the last bullet of the Closing note.
- **Run-start regression baseline (dec51b74) comparison: still outstanding.** No full-suite measurement of this head exists. The caller runs that sweep after this invocation.

### New observations

1. **The bridge's EndSent Closing loses the END when the leg writer fails but the reader stays open.** In `tick_closing` Phase B (local_bridge.rs, about 1239-1258), only `reader_done` is checked. If the writer thread's `write_all` fails (ssh stdin closed, stdout still open), `flushed` never reaches `leg_target`. The bridge then waits for `deadline` (500 ms) and completes Closing as if the END had been sent. It never takes the `leg_dead` resume path that the reader-EOF case uses. Trailing client DATA in the same channel batch can be lost the same way. The keeper session then stays parked until grace (60 s by default) instead of ending within about 2 s, against the acceptance criterion "Client exit ends its keeper session within 2 s". The client's final bytes (for example Detach) are also never delivered, on a path whose whole purpose is to deliver them. The fix is small: have the writer thread publish its failure, and treat writer-failed-and-unflushed like reader-EOF-and-unflushed. The task-22 review recorded this as a minor (see Step 2). I regrade it below.
2. **The keeper drops an out-of-ring resume without answering, after it has already stolen the live leg.** In `admit` Resumed, `detach_leg(.., "stolen")` runs before `endpoint.welcome(..)`. If the peer's `recv_offset` is outside the ring, the function returns `Ok(None)` and drops the new stream with no WELCOME or END. The old leg has already been shut, so a misbehaving or stale resume tears down a working leg. The bridge then sees EOF before WELCOME, and `classify_leg` for a reconnect leg decides what happens next. This is the same item as the previous review's deferred "out-of-ring resume offset drops the leg unanswered". The new detail is that a live leg is lost too. The local side never acks past what it received, so this cannot happen in a correct peer. I rate it Deferred OK.
3. **The spec's touch-list table is stale.** It still says status.rs is 1 line and the total is 11. The Post-Implementation Note says 12, and the captured diff outside fork paths is exactly 12 inserted lines. Line 444 in §Unchanged behavior also still says "the 11 hook lines". This is documentation only.
4. **The hook seams are consistent.** `STATUS_CAPABILITY`, `SESSION_FLAG` and `KEEPER_FLAG` are each defined once. `link_frames.rs` imports `LinkStatus`/`LINK_KIND` from `client::mosh_outage`. The `composition.rs` paint call matches the 3-argument `paint`. The keeper runs with `SSH_AUTH_SOCK` removed, so the `host.rs` `ssh_agent::Registration::start()` that now runs before `dispatch` is a no-op for the keeper and correct for the attacher. Nothing in the repository targets a BSD, so the Linux/macOS-only `peer_uid` is not a live build break.

## Step 2: ledger triage

The ledger has 1 parked completion, about 60 `minor (deferred)` lines, 4 `Recurring minor` lines (all one process-class cluster), and no `BLOCKED-AUTH` or `Recurring blocker` lines.

- **Recurring minor, process-class `missing-evidence`** (×6 across mosh-dqw.1, .4, .11, .21, .20, .27; recurrence-id v1-157-900283191-1386768612). Task reports give test commands and pass counts in prose, with no structured evidence records. As a group, this is why no per-task test claim can stand in for the deferred full-suite sweep. Each finding keeps its own identity. None is a product defect. Deferred OK, with the consequence noted under Untested scope.
- **Parked: mosh-dqw.10, status.rs hook is 2 lines (12 upstream lines in total) against the bead's "exactly 3".** This is plan-mandated, so it needs a human decision. The spec now records the 12-line count in a Post-Implementation Note, but the bead text was not amended and the touch-list table still says 11 (observation 3). It must be settled by the human before landing.
- **Misgraded minor, data-loss path: Task 22 (mosh-dqw.29) `[missing-edge-case-test]` "EndSent: a leg writer failure while the reader stays open falls to the 500 ms deadline and loses the END".** This matches observation 1. It names a lost END and undelivered client bytes, so it is must-fix.
- **Minors already resolved by later beads (verified by inspection):** Task 4 `[final-end-loss]`, Task 6 `[error-classification]`, Task 8 `[blocking-stop-path]`, which the Closing redesign replaced, and Task 10 `[product-observation]`, the final-ACK race. Task 4 `[caller-contract]` is honoured: keeper `run_leg` never calls `on_leg_bytes` before `welcome`, because `read_hello` consumes exactly one message and the leg reader starts after WELCOME.
- **Task 22 `[message-overwrite]`** (on_stop and Phase A replace `io.pending`). On stop, unsent ring bytes are deliberately not flushed, so replacing a pending DATA chunk with END{client-gone} adds no new class of loss. In Phase A the server is already gone. Deferred OK.
- **Task 23 `[flaky-test-unverified]`** (mosh_fallback stale_keeper_socket failed once and then passed alone). This is a possible flake in the suite that the acceptance criteria require to pass. It is not a skipped test, but it must be measured with repeated runs in the deferred sweep. Untested scope.
- **Task 19 `[timing-flake-risk]`** (a wall-clock window of 3800-5500 ms) and **Task 15 `[loosened-bound]`**: timing risks for the sweep. Untested scope.
- **Task 21 `[pid-reuse-kill]`** (the test harness SIGKILLs keeper pids it saw earlier). This is a hazard to the developer's machine, not a product defect. The window is one test's lifetime. Deferred OK, but it should be fixed soon.
- **Task 9 `[child-not-reaped]`**: the keeper is setsid'd and outlives the short-lived attacher, and init reaps it. Deferred OK. **Task 9 `[platform-coverage-gap]`**: no BSD target exists. Deferred OK.
- **Task 7 `[ring-cap-edge]`**: recorded in the spec and accepted there. Deferred OK.
- The remaining minors are about formatting, oversized modules, doc drift, missing red runs, test brittleness and environment prerequisites. They are listed under Deferred OK or Untested scope.

## Verdict
not ready (bounded review only: whole-range review outstanding, no full-suite measurement of this head; bridge EndSent Closing loses END and trailing client bytes on leg-writer failure; parked 12-line footprint needs human decision)

## Must fix before landing
- Complete a whole-range final review: this bounded review covered 16 of 26 product/test/script paths in full and none of the 124 run-document paths; provenance gap run.md:43 inv-c723b6b8 incomplete claimed collection remains open.
- Run the full-suite sweep (cargo nextest run --locked, clippy -D warnings, fmt --check) at head a1232c13 and compare against the run-start baseline dec51b74, not 992f6a45; no full-suite measurement of this branch exists, so the branch is not tested as a whole.
- src/mosh/local_bridge.rs tick_closing Phase B (about 1239-1258): EndSent Closing checks only reader_done, so a leg writer failure while the reader stays open waits out the 500 ms deadline and completes without END or trailing DATA; the keeper session parks until grace (60 s) instead of ending within 2 s; publish writer failure and resume like the eof-before-end-flushed case (ledger Task 22 minor, misgraded data-loss path).
- Human decision on the parked mosh-dqw.10 finding: accept the 12-line upstream footprint (status.rs hook is 2 lines) and amend the bead's "exactly 3" plus the spec touch-list table total of 11 and the §Unchanged behavior "11 hook lines" text, or require a different hook shape.

## Untested scope
- Unreviewed product hunks: src/mosh/local_bridge.rs @@ -0,0 +1,2738 @@ (lines 53-419 and the test module unread), src/mosh/remote/keeper.rs @@ -0,0 +1,1736 @@ (test module unread), src/mosh/session.rs @@ -0,0 +1,1899 @@ (test module unread), src/mosh/ssh_leg.rs @@ -0,0 +1,1168 @@ (mostly unread).
- Unreviewed test hunks: tests/mosh_fallback.rs @@ -0,0 +1,447 @@, tests/mosh_harness.rs @@ -0,0 +1,961 @@, tests/mosh_integration.rs @@ -0,0 +1,217 @@, tests/mosh_multi_client.rs @@ -0,0 +1,485 @@, tests/mosh_resume.rs @@ -0,0 +1,482 @@.
- Unreviewed docs: HERDR_MOSH_VISION.md @@ -0,0 +1,252 @@, the design spec outside the touch list, acceptance criteria and post-implementation notes, and all 124 docs/superpowers/runs paths (roasts, amend records, inputs, slices, the inv-c723b6b8 archive and run.md).
- Provenance gap: a1232c13:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection; the review collection for that earlier invocation is unsettled.
- tests/client_mode.rs federated_client_starts_without_local_and_survives_its_restart was not re-run; its idle-timeout assertion is satisfied by inspection, but its three bridge-kill reconnect cycles now run through the resume path and are unmeasured.
- tests/mosh_fallback.rs stale_keeper_socket failed once in task 23's run and passed alone; needs repeated runs in the sweep (ledger Task 23 minor).
- local_bridge transient_spawn_failures_back_off_until_give_up wall-clock window 3800-5500 ms and the loosened tests/mosh_resume.rs:126 bound are flake risks under load (ledger Tasks 19 and 15).
- Per-task test claims are prose only across six tasks (recurring missing-evidence cluster), so no task report substitutes for the sweep.
- No test covers EndSent Closing with a leg writer failure while the reader stays open (ledger Task 22).
- Keeper out-of-ring resume after stealing a live leg, keeper accept-error back-off under real EMFILE and the keeper silence verdict in run_timers have no end-to-end tests.
- Non-Unix local and Windows-remote gates (status.rs cfg element, dispatch non-Unix arm, start_ui_bridge cfg(not(unix)) branch) are inspection only; no Windows build or lint was run.
- AC15 flow control is evidenced by 16 x 1 MiB pastes; the 2 MiB in-flight bound is unit-only and no single 16 MiB payload is sent (ledger Task 10).
- ClientShellSnapshot absence after resume is evidenced only by the server_connect-count proxy (ledger Task 11).
- src/update.rs manifest override and scripts/mosh-diff-budget.sh have no automated tests.

## Deferred OK
- Keeper admit Resumed calls detach_leg before welcome, so an out-of-ring peer offset tears down the live leg and drops the new one unanswered; unreachable with a correct peer (observation 2, previous review's deferred item).
- Spec touch-list table and §Unchanged behavior still say 11 lines; the Post-Implementation Note says 12 and the captured diff is 12 (documentation, tracked under the parked decision).
- Spec final-ACK race note still says mosh-dqw.19 had not landed (ledger Task 16 stale-cross-reference).
- local_bridge on_stop and tick_closing Phase A overwrite io.pending; stop already drops unsent ring bytes by design and Phase A runs after server-eof (ledger Task 22 message-overwrite).
- tests/mosh_harness.rs Remote::drop SIGKILLs earlier keeper pids, which a recycled pid could hit; fix soon (ledger Task 21 pid-reuse-kill).
- Keeper child never reaped by the attacher; it is setsid'd and init reaps it (ledger Task 9).
- peer_uid has Linux and macOS implementations only; no other Unix target exists in the repository (ledger Task 9).
- Keeper exact-16-MiB back-pressure instead of overflow, recorded and accepted in the spec (ledger Task 7).
- Keeper is_end matches by type byte only; oversized keeper and local_bridge test modules; duplicate trace helpers (ledger Tasks 7, 8, 11).
- Keeper run_timers silence branch ordering and grace evaluated one 50 ms pass later after a silence verdict; close_session spawns one 500 ms backstop thread per closed session (ledger Tasks 13, 17).
- Unknown --x-mosh-* flag rejected only after ensure_remote_server_running (ledger Task 9).
- noninteractive leg 0 captures stderr; spawn_leg teardown joins the stderr thread (bounded by the deadline per the Closing note) (ledger Task 6).
- rustfmt drift in fork files (ssh_leg.rs, local_bridge.rs connect_with indentation) and #[rustfmt::skip] on hook lines (ledger Tasks 3, 6, 11, 22).
- scripts/mosh-diff-budget.sh counts insertions only (ledger Task 3).
- mod.rs idle_opt_in doc comment sits between platform_gate's doc lines (ledger Task 14).
- tests/mosh_status.rs caps.len() == 4 breaks when upstream adds a capability (ledger Task 9).
- Outage banner stale clear takes effect only on the next repaint; the hook's decode skip is enforced by continue only (ledger Task 2).
- link_frames encode_link_frame takes &LinkStatus rather than data_json (ledger Task 5).
- Slow ring-overflow integration test (about 90 s) not ignored; harness needs perl and /proc and a non-root user for the spawn-fail knob; tests/mosh_multi_client.rs ui_child still reads /proc (ledger Tasks 10, 21).
- Weak or relaxed assertions in the new tests/mosh_integration.rs closing-order test (2000 ms bound from the previous keeper event; accepts server-eof or client-gone) (ledger Task 23).
- Missing red runs and TDD-red evidence on several fix tasks (ledger Tasks 13, 15, 19, 22, 23).
- Stray evidence/mosh-dqw.9/test-summary.log committed; resume trace replayed_down null on the local side (recorded in the spec) (ledger Tasks 8, 10).
- src/remote/attach.rs remote-install manifest URLs ignore the fork override, so fork installs put upstream binaries on remotes, which then take the plain bridge (previous review deferred, needs a follow-up bead).
