# Final review (bounded): epic mosh-dqw, super-auto/mosh-phase1-resumable-session

Invocation: inv-29e12973-5528-4a80-9636-b7c81b87f820
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head e3cac50e5deeb278e755676f4410f9e320d96a6b (both verified with `git rev-parse --verify <sha>^{commit}`; caller checkout HEAD observed at e3cac50e, status clean, not switched).

## Scope and boundedness

This is a bounded captured-source review, not a whole-range review. The selection sidecar `review-inputs.json` reports status unavailable, sourceAvailable true, wholeRangeReady false, 134 changed paths, 0 omitted, and one retained gap: `e3cac50e:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:40 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03: incomplete claimed collection`. No whole-range package was built or loaded. The failed assembly and the original sidecar were left untouched.

Read from captured Git:
- Every upstream hook hunk: `src/cli/status.rs`, `src/client/mod.rs` (2 hunks), `src/client/shell/composition.rs`, `src/remote/attach.rs` (2 hunks), `src/remote/host.rs` (2 hunks), `src/remote/saved.rs`, `src/update.rs`.
- `scripts/mosh-diff-budget.sh`, `docs/superpowers/specs/INDEX.md`, `tests/mosh_status.rs`, `src/mosh/mod.rs` in full.
- The production (non-test) portions of `src/mosh/session.rs` (1-773), `src/mosh/local_bridge.rs` (1-1175), `src/mosh/remote/keeper.rs` (1-656), `src/mosh/ssh_leg.rs` (1-473), `src/mosh/remote/attacher.rs` (1-207), `src/mosh/remote/mod.rs` (1-67 and the rendezvous module 100-285), `src/mosh/outage.rs` (1-200), `src/mosh/link_frames.rs` (1-120), `src/mosh/ring.rs` (1-90).
- The design spec, lines 1-346 plus its Post-Implementation Notes, and run.md lines 36-44 (the gap context).
- The prior final review `final-review-inv-c723b6b8-...md` (Verdict through Deferred OK), checked against head.

Not read: the `#[cfg(test)]` modules of the files above, `HERDR_MOSH_VISION.md`, the e2e test files `tests/mosh_fallback.rs`, `tests/mosh_harness.rs`, `tests/mosh_integration.rs`, `tests/mosh_multi_client.rs`, `tests/mosh_resume.rs`, and 107 of the 108 `docs/superpowers/runs/...` paths (reviews, inputs, coverage and ledger documents, evidence, profiles).

No build, test, clippy or full-suite run was made in this review. The full-suite sweep is deferred to the caller, so no full-suite measurement of this branch exists yet. This branch is untested at head as a whole.

## Step 1: own findings (cross-task seams against the spec)

1. **Important (PLAUSIBLE): the keeper can drop the client's last bytes, including `Detach`, when `END{client-gone}` follows them.** In `src/mosh/remote/keeper.rs` `run_leg`, a `Deliver` is queued on `session.to_server` (an mpsc channel drained by the `server_writer` thread). A `PeerEnd` decoded in the same or the next read calls `remove_session`, and that runs `session.server.shutdown(Shutdown::Both)` straight away. `server_writer` then fails `write_all` on the shut socket and discards everything still queued. On the local side, `on_client_eof` queues the END and the very next `tick()` pumps it right after the final DATA (`local_bridge.rs` `on_client_eof`, `tick_linked`, `pump_sends`), so DATA and END reach the keeper microseconds apart. `EndpointRegistry::drop` (`src/client/endpoint/registry.rs:365-375`) sends `ClientMessage::Detach` and then disconnects. The server handles `Detach` differently from EOF: `headless.rs:2640-2650` calls `send_terminal_stream_detach_shutdown` only for `ClientDetach`. So a resume-bridge client that quits can be seen by the server as "disconnected" rather than "detached", and any other final input is lost. That contradicts spec rule 6 "Ordering" (a session-final END never overtakes stream data). The plain bridge does not lose these bytes. Fix: on `client-gone`, close the `to_server` sender and let `server_writer` drain and then shut the server stream, bounded by a short deadline, instead of shutting it down inline.
2. **Minor: pre-WELCOME leg EOF blocks the leg I/O thread on `child.wait()`.** `local_bridge.rs` `leg_ended("eof")` calls `pre_welcome_failed(false)`, which calls `kill_leg(false)` and then `Leg::finish(false)`. That waits on the ssh child without killing it and joins the stderr thread (`ssh_leg.rs` `spawn_leg` teardown). If ssh stays alive after its stdout closes, the bridge thread stops polling `should_stop` and timers, which the spec forbids ("never blocks on the network"). The same wait-without-kill sits in `dial_command`'s `Ok((Err(_), _))` arm, which delays `join_dial` at teardown. It is related to the deferred task-6 `[blocking-teardown]` minor.
3. **Minor: `ReplayRing` stores bytes in a `VecDeque<u8>` and copies them one byte at a time** (`ring.rs` `push`, `read_at`). With 8/16 MiB caps this works but costs CPU. It is consistent with the slow, CPU-heavy ring-overflow test noted by task 10. It was deferred earlier as an efficiency item.
4. **Verified fixed at head**, from the prior final review's must-fix list:
   - `on_leg_bytes` now returns decoded `Deliver` events together with the error (`LegBytesError.delivered`), and both owners deliver them (dqw.20).
   - The session-final END is retained in `final_end` and re-sent on every leg (`leg_down` and `set_up` reset `final_end_sent`), and `final_ack` now runs before `close_client` (dqw.19).
   - Saved-machine `--idle-timeout-v1` is restored on the resume leg command (`mod.rs` `idle_opt_in`, `ssh_leg.rs` `SshLegDialer::new`), and `host.rs`/`remote::route` admit `["--idle-timeout-v1", SESSION_FLAG]` (dqw.21).
   - The spec Post-Implementation Notes for the four divergences are present (dqw.23).

   The flaky-keeper-test fix and the client_mode regression were not re-run here.

## Step 2: ledger triage

- **Parked, plan-mandated (task 9, mosh-dqw.10):** the `status.rs` hook is 2 lines (4 upstream lines for the task, 12 in total), not the bead's "exactly 3". The spec now has a Post-Implementation Note recording 12 lines, and the budget script allows 20. The code is fine. The bead acceptance text still needs a human decision to amend it.
- **Recurring minor `[missing-evidence]` ×4 across mosh-dqw.1, .4, .11, .21 (process class):** test claims across tasks rest on prose, not structured evidence. This is a process issue, not a product defect, but it means no task's pass claim can stand in for the deferred full-suite sweep.
- **Must-fix by the misgrading rule (known product defects):**
  - Task 6 `[error-classification]`: a reconnect-leg ssh spawn failure is Fatal. The spec note calls it "a known gap ... not a decision to keep the behaviour". A transient EAGAIN ends a resumable session.
  - Task 13 `[edge-case-ordering]`: in keeper `run_timers`, the silence branch resets `end_sent` before the removal check, so a fully acked eof session waits for grace. The fix is a reorder of a few lines.
  - Task 6 `[blocking-teardown]`: the stderr-thread join can hang teardown when a ProxyCommand grandchild holds stderr. That breaks the spec's "never a hang", and it shares a root with own finding 2.
- **Resolved at head:** task 4 `[final-end-loss]` (dqw.19). Task 10 `[product-observation]`, the graceful quit versus server hang-up race, is resolved by the code order (dqw.19 moved `final_ack` ahead of `close_client`), but no test asserts it.
- **Deferred OK:** the rest. Doc drift, fmt drift, module size, brittle or slow tests, keeper back-pressure at 16 MiB (accepted and documented), the unreaped keeper child, late rejection of an unknown flag, the `is_end` type-byte helper, and the bounded `blocking-stop-path` (`close_client` is bounded by 500 ms, and `shutdown_client` unblocks the writer).
- **No `BLOCKED-AUTH` lines in the ledger.**
- **Metrics:** ledger-check ok, landed equals completed (16 merges, 15 clean plus 1 after a fix pass), no METRICS INVALID line.

## Verdict
not ready (bounded review only, whole-range review and full-suite sweep outstanding; keeper may drop client Detach before END client-gone; three known product-defect minors and one plan-mandated parked item need disposition)

## Must fix before landing
- src/mosh/remote/keeper.rs run_leg: on PeerEnd the call to remove_session shuts the server stream while bytes delivered just before END{client-gone} can still be queued on to_server, so the client's final Detach (registry.rs:365-375) can be lost and the server sees a disconnect instead of a detach (headless.rs:2640-2650); drain server_writer before shutdown (PLAUSIBLE, from code reading, not reproduced).
- src/mosh/ssh_leg.rs dial_command: map a spawn_leg error on a reconnect leg to Retryable instead of Fatal (task 6 [error-classification]; the spec note calls it a known gap, not a kept decision).
- src/mosh/remote/keeper.rs run_timers: check eof && end_sent && fully_acked before the silence branch resets end_sent, so a fully acked eof session is removed rather than parked for grace (task 13 [edge-case-ordering]).
- src/mosh/ssh_leg.rs spawn_leg teardown and src/mosh/local_bridge.rs pre_welcome_failed(false): bound the child wait and the stderr-thread join (kill after a short deadline) so a lingering ssh or a ProxyCommand grandchild holding stderr cannot hang the bridge thread or teardown (task 6 [blocking-teardown] plus own finding 2).
- Human decision: amend the mosh-dqw.10 bead acceptance from "exactly 3 upstream lines" to the actual 4 (the spec Post-Implementation Note already records the 12-line total); plan-mandated parked finding.
- Run the deferred full-suite sweep (cargo nextest run plus clippy -D warnings) at e3cac50e and compare it against the run-start baseline dec51b74; no full-suite measurement of this branch exists.
- Complete a whole-range final review once the review-input selection gap (run.md:40, inv-c723b6b8 incomplete claimed collection) is resolved; this bounded review does not replace it.

## Untested scope
- Selection gap (retained): e3cac50e:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:40 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03: incomplete claimed collection; review-input provenance unavailable.
- Unreviewed paths: HERDR_MOSH_VISION.md, tests/mosh_fallback.rs, tests/mosh_harness.rs, tests/mosh_integration.rs, tests/mosh_multi_client.rs, tests/mosh_resume.rs, and 107 of the 108 docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/** paths (reviews/, inputs/, evidence/, coverage, ledger and profile documents; run.md read only at lines 36-44).
- Partly unreviewed hunks: the #[cfg(test)] modules of src/mosh/session.rs (774-1899), src/mosh/local_bridge.rs (1176-2292), src/mosh/remote/keeper.rs (657-1520), src/mosh/ssh_leg.rs (474-1042), src/mosh/remote/attacher.rs (208-526), src/mosh/remote/mod.rs (68-99 and 286-420), src/mosh/outage.rs (201-314), src/mosh/link_frames.rs (121-234), src/mosh/ring.rs (91-166); the design spec lines 347-479 were read only for the Post-Implementation Notes.
- No build, test, clippy or full-suite run in this review; the caller's deferred sweep is the only planned measurement.
- The client_mode federated idle-timeout regression (prior must-fix, addressed by dqw.21) was not re-run at head.
- The keeper test server_eof_forwards_remaining_bytes_then_end flake (prior must-fix) was not re-run at head.
- Task 11: 15 pre-existing agent_kind_mismatch failures have no recorded run owner (attribution gap).
- The keeper Detach/END race (Must fix item 1) has no test that sends DATA immediately followed by END{client-gone} and asserts the server received the bytes.
- Graceful --remote quit 2 s keeper session-end assertion is absent (task 10, prior review).
- AC15 flow control is evidenced by 16 x 1 MiB pastes, not a single 16 MiB image or an in-flight bound e2e check (task 10).
- The should_stop-first path for --remote is not separately exercised (task 10).
- server-connect-failed and protocol-version are injected by the fake ssh, not produced by a live keeper (task 10).
- A foreign-owned rendezvous dir is covered only by the mode-0755 case (task 10).
- The client/mod.rs hook's skip of decode_endpoint_control is enforced only by `continue`, with no test (task 2).
- ClientShellSnapshot absence after resume is evidenced only by the server_connect-count proxy (task 11).
- The non-Unix start_ui_bridge gate, the cfg_attr dead_code relaxation and peer_uid on non-Linux/macOS unixes are inspection-only (tasks 1, 8, 9, 11).
- The keeper silence-verdict path in run_timers has no in-process test (task 7).
- The attacher run() END{keeper-unavailable} plus non-zero exit is not tested end to end (task 9).
- The src/update.rs manifest override and scripts/mosh-diff-budget.sh have no automated tests.

## Deferred OK
- Task 3 [budget-insertions-only]: the diff budget counts insertions only; acceptable for a hand-run audit.
- Task 3 [rustfmt-skip-attribute] and the #[rustfmt::skip] on every hook line: needed to keep hooks one line each.
- Task 1 [scaffold-allow] / Task 11 [non-unix-unverified]: dead_code is relaxed only off Unix.
- Task 2 [stale-clear-needs-repaint]: the banner stale clear takes effect on the next repaint; accepted by design.
- Task 5 [signature-deviation]: encode_link_frame takes &LinkStatus; this keeps one payload definition.
- Task 6 and Task 11 [fmt-drift]: rustfmt diffs in src/mosh/ssh_leg.rs; cosmetic.
- Task 6 [deferred-to-later-task] items: the pre-WELCOME bound, END capture and the HERDR_REMOTE_BINARY hint now live in local_bridge.rs and mod.rs (verified in code).
- Task 6 [stderr-on-leg0]: noninteractive leg 0 captures stderr only for today's error message.
- Task 4 [brief-deviation] and [caller-contract]: the keeper drains after welcome as documented.
- Task 4 [final-end-loss]: resolved at head by dqw.19 (final_end retained, re-sent per leg).
- Task 7 [ring-cap-edge]: the keeper back-pressures at 16 MiB instead of overflowing; documented in a Post-Implementation Note and loses no data.
- Task 7 [oversized-test-module] / Task 8 [oversized-module]: file size only.
- Task 7 [fragile-helper]: keeper is_end matches the END type byte; correct for this codec.
- Task 8 [doc-drift] replayed_down null: documented; the keeper trace has the figure.
- Task 8 [blocking-stop-path]: close_client and on_stop are bounded by 500 ms, and shutdown_client unblocks a stuck writer.
- Task 9 [late-rejection-side-effect]: an unknown --x-mosh-* flag is rejected after ensure_remote_server_running; harmless.
- Task 9 [brittle-assertion]: tests/mosh_status.rs caps.len()==4 breaks on the next upstream capability; change it to containment at the next merge.
- Task 9 [child-not-reaped]: the keeper child is reparented when the attacher exits; at worst a short-lived zombie.
- Task 10 [product-observation] graceful quit race: addressed by dqw.19's final_ack ordering; the assertion gap is listed under Untested scope.
- Task 10 [slow-test], [environment-dependency], [stray-artifact]: test and repo hygiene.
- Task 11 [duplicate-helper]: two trace helpers; permitted by the brief.
- Task 14 [doc-drift]: platform_gate's doc comment now sits above idle_opt_in in src/mosh/mod.rs; cosmetic.
- Task 15 [missing-red-check] and [loosened-bound]: justified in the brief.
- Task 16 [stale-cross-reference]: the spec note on the final-ACK race should cite dqw.19 as landed.
- Recurring minor [missing-evidence] x4 (mosh-dqw.1, .4, .11, .21): a process-class cluster; it does not block landing but leaves the full-suite sweep as the only measurement.
- src/mosh/ring.rs byte-wise VecDeque copying: an efficiency follow-up.
- The prior review's Deferred OK list (keeper accept-error exit, out-of-ring resume offset, remote-install manifest URLs ignoring the fork override, OnceLock grace env in tests, banner occlusion and cursor) is still unaddressed and remains deferrable.
