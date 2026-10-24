# Final review (bounded) — epic mosh-dqw, super-auto/mosh-phase1-resumable-session

Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 89c026dc1e6e72c15cbc84bbd5315b27a25a130a (both verified with `git rev-parse --verify`). Caller checkout observed clean at head 89c026dc at review start.

## Scope and route

Bounded captured-source review. review-inputs.json reported status unavailable, sourceAvailable true, wholeRangeReady false, 166 selected, 0 omitted, one retained gap: 89c026dc:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection. The failed assembly and the original sidecar were left untouched. No whole-range package was built.

Read the full changed-path inventory (166 paths, 44139 insertions, 4 deletions). Focus: the delta since the previous final review (inv-9350a065 at a1232c13), which is commits 7c7ea894 (mosh-dqw.34) and 62f00b74 (mosh-dqw.33), plus all upstream hook hunks and the two small mosh test files listed in the coverage JSON.

Inspected in full: src/cli/status.rs, src/client/mod.rs, src/client/shell/composition.rs, src/remote/attach.rs, src/remote/host.rs, src/remote/saved.rs, src/update.rs hook hunks; tests/mosh_status.rs; tests/mosh_integration.rs.

Partially read, not claimed as inspected: src/mosh/local_bridge.rs (single hunk @@ -0,0 +1,2841 @@): the Closing state machine and its inputs (lines about 330-680, 900-940, 1040-1345) and the two tests added by 62f00b74; the rest of that file is unreviewed in this invocation.

## Findings (own view, STEP 1)

### Prior Must-fix resolved: EndSent Closing with a leg writer failure
62f00b74 adds `LegIo::writer_done`, set by the leg writer thread on exit, and `LegIo::worker_exited()` (reader or writer). tick_closing Phase B now treats writer death like reader death: for EndSent with the END unflushed it clears Closing and calls `leg_dead("write-failed")`, so the retained END is re-sent on the next leg; otherwise the leg is done. EndSent Closing is only entered by pump_sends after the END try_send succeeded, so leg_target is always Some there and the Disconnected hand-off branch cannot silently skip the END. writer_done can only be set while the leg is held by a write error (tx is dropped only by reap_leg/kill_leg, which take the leg). New unit test end_write_failure_with_open_leg_resends_end_on_resume covers it. Resolved by inspection; not run here.

### Prior should-fix resolved: client gone during server-eof drain
Phase A now completes the drain when the client writer thread has finished (EPIPE) or a ClientEof arrived while Closing (`client_eof_seen`). update_delivered still acks only `client.written` on this path because on_client_eof returns before `client_gone()` sets client_closed while Closing; the new test asserts the keeper is not fully acked. Covered by client_gone_mid_server_eof_drain_closes_within_stop_budget, which exercises only the finished-writer branch.

### Observation (minor): client_eof_seen treats a half-close as drained
A ClientEof in Phase A ends the server-eof drain early even if the client still reads. Phase C then sends ClientOut::Close and gives the client writer CLOSE_BUDGET to finish behind its queued frames, so the client may still receive them, but the final ACK reflects only what was written at Phase A exit. The session is ending with server-eof, so the under-ack has no consequence; herdr clients do not half-close in normal use. No direct test of this branch (also ledger Task 24 minor).

### Observation (minor): closing-order integration test now passes without proving delivery when stop wins
tests/mosh_integration.rs client_gone_with_queued_bytes_ends_the_session_promptly asserts the server received MKfinal only when the local session_end reason is not "stopped". This matches the spec (design line 144: on stop, ring bytes not yet sent are abandoned), and it removes the flake the round-3 roast named. Consequence: the end-to-end claim "END follows ring bytes and the keeper drains to_server" is only checked on runs that happen to take the client-EOF path, and the 200-run check did not record which branch ran (ledger Task 25 minor). A `--remote` user who types and then quits on a slow link loses the unsent keystrokes; spec-sanctioned.

### Upstream hook footprint
Captured upstream diff is 12 insertion lines over 7 files, every line tagged `// mosh-fork hook`, one #[path] mount in src/remote/attach.rs. status.rs uses two lines (#[cfg(unix)] plus the entry), which is the parked mosh-dqw.10 plan-mandated finding. host.rs `[o, ..] | [_, o] if o.starts_with("--x-mosh-") => false` still accepts any --x-mosh-* flag before ensure_remote_server_running and defers rejection to dispatch (previously deferred). update.rs manifest override uses option_env! at compile time; no test (previously noted).

## Ledger triage (STEP 2)

- Parked mosh-dqw.10 (Important, plan-mandated): status.rs hook is 2 lines vs the bead's "exactly 3" upstream-line total; still needs the human's decision. Must fix before landing (decision, not code).
- Parked run-failure: tests/client_mode.rs federated idle-timeout regression vs baseline dec51b74; addressed by 4561acb6 (mosh-dqw.21) per the log, but no measurement at head exists. Folded into the sweep must-fix.
- Task 24 (mosh-dqw.33) minor: client_eof_seen branch untested. Deferred OK; listed under untested scope.
- Task 25 (mosh-dqw.34) minors: unrecorded branch coverage and brief deviation (keeper may end with server-eof when stop wins). Deferred OK; the brief deviation is spec-consistent.
- Task 10 (mosh-dqw.9) product-observation: graceful --remote quit racing server hang-up parks the keeper until grace. The current closing-order test asserts keeper session_end within 2000 ms of the previous keeper event under that race; this addresses it on the tested path. Deferred OK pending sweep evidence.
- Recurring minor missing-evidence (6 tasks): per-task test claims are prose only; no task report substitutes for the sweep. Process issue, not product.
- All other deferred items carried from inv-9350a065 retained unchanged (see Deferred OK). No BLOCKED-AUTH lines observed.

## Verdict
not ready (bounded review only; whole-range review, full-suite sweep and the parked mosh-dqw.10 decision are outstanding)

## Must fix before landing
- Complete a whole-range final review: this bounded review inspected 9 product/test paths (upstream hooks, tests/mosh_status.rs, tests/mosh_integration.rs) and none of the run-document paths; provenance gap 89c026dc:docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection remains open.
- Run the full-suite sweep (cargo nextest run --locked, clippy -D warnings, fmt --check) at head 89c026dc and compare with the run-start baseline dec51b74; no full-suite measurement of this branch exists, so the branch is not tested as a whole, including the parked tests/client_mode.rs federated idle-timeout regression.
- Human decision on the parked mosh-dqw.10 finding: accept the 12-line upstream footprint (status.rs hook is 2 lines) and amend the bead's "exactly 3" and the spec's 11-line touch-list total, or require a different hook shape.

## Untested scope
- whole-epic review incomplete; bounded inspected hunks only; all remaining hunks and provenance gaps remain unreviewed
- Unreviewed product hunks: src/mosh/local_bridge.rs @@ -0,0 +1,2841 @@ (only the Closing machinery and the two new tests were read), src/mosh/session.rs @@ -0,0 +1,1899 @@, src/mosh/remote/keeper.rs @@ -0,0 +1,1736 @@, src/mosh/ssh_leg.rs @@ -0,0 +1,1168 @@, src/mosh/mod.rs @@ -0,0 +1,237 @@, src/mosh/remote/mod.rs @@ -0,0 +1,420 @@, src/mosh/remote/attacher.rs @@ -0,0 +1,526 @@, src/mosh/ring.rs @@ -0,0 +1,166 @@, src/mosh/link_frames.rs @@ -0,0 +1,234 @@, src/mosh/outage.rs @@ -0,0 +1,314 @@.
- Unreviewed test and script hunks: tests/mosh_fallback.rs @@ -0,0 +1,447 @@, tests/mosh_harness.rs @@ -0,0 +1,961 @@, tests/mosh_multi_client.rs @@ -0,0 +1,485 @@, tests/mosh_resume.rs @@ -0,0 +1,482 @@, scripts/mosh-diff-budget.sh @@ -0,0 +1,71 @@.
- Unreviewed docs: HERDR_MOSH_VISION.md, docs/superpowers/specs/INDEX.md, the design spec (only §Local bridge behaviour stop rule and the Closing Post-Implementation Note were consulted), and all other docs/superpowers/runs paths including roasts, inputs, profiles, archived reviews and run.md.
- Provenance gap: run.md:43 inv-c723b6b8-ffc9-416e-96bd-5c1052097e03 incomplete claimed collection; that earlier invocation's review collection is unsettled.
- No test exercises the Phase A client_eof_seen branch (ClientEof while draining a server-eof); only the finished-writer branch is tested (ledger Task 24).
- tests/mosh_integration.rs closing-order test checks queued-byte delivery only on runs that take the client-EOF path, and no run records which branch it took (ledger Task 25).
- None of the tests added by 62f00b74 and 7c7ea894 were run in this review; the 200/200 parallel-load claim for the integration test is task-report prose.
- tests/client_mode.rs federated_client_starts_without_local_and_survives_its_restart is unmeasured at head; its bridge-kill reconnect cycles now run through the resume path.
- tests/mosh_fallback.rs stale_keeper_socket failed once in task 23's run; needs repeated runs in the sweep.
- Wall-clock bounds in local_bridge transient_spawn_failures_back_off_until_give_up and tests/mosh_resume.rs are flake risks under load.
- Non-Unix local and Windows-remote gates are inspection only; no Windows build or lint was run.
- AC15 flow control: the 2 MiB in-flight bound is unit-only; no single 16 MiB payload is sent end to end.
- src/update.rs manifest override and scripts/mosh-diff-budget.sh have no automated tests.

## Deferred OK
- Phase A ClientEof counts as drained, so a half-closing client gets a final ACK covering only bytes written at drain exit; harmless because the session is ending with server-eof.
- On stop, unsent ring bytes (including keystrokes typed just before a `--remote` quit on a slow link) are abandoned, as the spec states; the closing-order test now accepts that outcome.
- Keeper admit Resumed calls detach_leg before welcome, so an out-of-ring peer offset tears down the live leg; unreachable with a correct peer (carried).
- Spec touch-list table and §Unchanged behavior still say 11 lines while the captured diff is 12 (documentation, tracked under the parked decision).
- Spurious leg_down{stolen} keeper trace line on resume (round-3 punch list, trace fidelity only).
- tests/mosh_harness.rs Remote::drop SIGKILLs earlier keeper pids that a recycled pid could hit (carried, fix soon).
- Keeper child never reaped by the attacher (setsid, init reaps); peer_uid Linux and macOS only (carried).
- Keeper exact-16-MiB back-pressure instead of overflow, recorded in the spec (carried).
- Unknown --x-mosh-* flag rejected only after ensure_remote_server_running (carried).
- rustfmt drift in fork files and #[rustfmt::skip] on hook lines (carried).
- tests/mosh_status.rs caps.len() == 4 breaks when upstream adds a capability (carried).
- Outage banner stale clear takes effect only on the next repaint (carried).
- Slow ring-overflow integration test (about 90 s) not ignored; harness needs perl, /proc and a non-root user (carried).
- Missing TDD-red evidence on several fix tasks and the recurring missing-evidence cluster (process).
- src/remote/attach.rs remote-install manifest URLs ignore the fork override, so fork installs put upstream binaries on remotes, which then take the plain bridge (carried; needs a follow-up bead).
