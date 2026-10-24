# Final review: cluster 7 (mosh-remote)

Invocation: inv-c723b6b8-ffc9-416e-96bd-5c1052097e03
Epic: mosh-dqw (Mosh Phase 1: resumable remote session)
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`).
Review source binding: `dispatch-text.mjs bind-review-source` returned exit 0 with the invocation, base and head above.
Cluster files: src/mosh/remote/attacher.rs, src/mosh/remote/keeper.rs, src/mosh/remote/mod.rs (slices 000039-000041; all three are new files).
Spec: docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/2026-10-24-mosh-phase1-resumable-session-design.md at head. Relevant sections: Architecture, Keeper (Rendezvous, Attacher failure reporting, Spawn, Per-HELLO, Grace, Server EOF, Idle exit), Ring caps point 4, Testing (Keeper and Rendezvous unit tests).

## Method

- Read the three files at head with `git show 2bd5303c:<path>`, plus the digest and the spec sections listed above.
- To check named risks, opened these upstream helpers: `run_remote_client_bridge` (src/remote/host.rs), `forward_remote_bridge_stdio` and `create_remote_private_dir` (src/platform/unix_common.rs), the SIGPIPE disposition in main.rs and unix_common.rs, `ssh_agent::Registration::start`, `build_info`, and the `SessionEndpoint` helpers (`welcome`, `leg_down`, `may_read_source`, `queue_final_end`, `fully_acked`).
- Ran the tests in a review-owned detached worktree at head (/tmp/claude-1001/-var-home-alepar-AleCode-herdr/f90071ef-0086-4b3f-982c-c58ab7dd70ba/scratchpad/rev-c7/wt), with its own CARGO_TARGET_DIR:
  - `cargo test --bin herdr mosh::remote`: the first run gave 46 passed and 1 failed (`keeper::tests::server_eof_forwards_remaining_bytes_then_end`). Three more runs passed (47/47).
  - The failing test run on its own: 3 of 6 runs failed.
  - `cargo test --bin herdr mosh::remote::keeper`, 10 runs: 1 failed and 9 passed.
  - In total the test failed in 5 of 20 runs. Each failure took 5.1 s and ended with the same panic: `no trace session_end` (the only trace lines were server_connect, welcome new and leg_up).
- Caller checkout observed before and after: HEAD 2bd5303c2b3ab118792a7b9e10672b487158c931, and `git status --porcelain` was empty both times.

## Spec conformance (what holds)

- **Rendezvous.**
  - The directory is `<temp_dir>/herdr-mosh-<euid>`, with a `/tmp` fallback when the socket path would not fit `sun_path` (<= 103 bytes).
  - It is created 0700 and `AlreadyExists` is accepted. It is checked with `symlink_metadata` for being a directory, owned by euid, with mode exactly 0700.
  - The socket name is `k-<24 hex>.sock`. The hash is SHA-256 over length-prefixed fields (euid, session, protocol version, build id, client socket path). The `.live` and `.spawn` locks are siblings of the socket.
  - The socket is bound and then chmod 0600. Since the directory is 0700, the bind-then-chmod window is harmless.
- **Attacher bring-up** follows the spec's order:
  1. Validate the directory, then connect.
  2. On ENOENT or ECONNREFUSED, take the blocking spawn lock and connect again.
  3. Try the live lock without blocking. If it is free: unlink the stale socket, drop the live lock, spawn, then poll for 5 s. If it is held: poll for 5 s, then retry the live lock once.
  4. Release the spawn lock on return.
  - Every connect is followed by a peer-uid check (`SO_PEERCRED` on Linux, `getpeereid` on macOS) before any byte is forwarded.
  - Any bring-up error writes exactly one `END{"keeper-unavailable"}` to stdout and returns Err, so the exit is non-zero.
  - After bring-up the attacher pumps through upstream `forward_remote_bridge_stdio`.
- **Keeper spawn.** The command is `current_exe [--session S] remote-client-bridge --x-mosh-keeper-v1`, with null stdin, stdout and stderr, `SSH_AUTH_SOCK` removed, and `detach_server_daemon_command` (setsid). Lock files open with O_CLOEXEC (Rust std), so the keeper inherits no lock fds.
- **Per-HELLO table.**
  - `previous_token` eviction runs first.
  - Then, via `session::answer_hello`: a version mismatch gets `END{protocol-version}`; resume:false on an existing token gets `END{token-in-use}`; resume:true on an absent token gets `WELCOME unknown`, with no server connect.
  - A new session connects to the server once; if that fails it gets `END{server-connect-failed}` and nothing is created.
  - A resume steals any live leg, cancels the grace, replays from the peer's offset, and re-queues `END{server-eof}` for an eof session.
  - Every refusal is followed by `answer_and_close`.
- **Grace and timers.** The grace comes only from `HELLO.grace_ms`; `keeper_reads_only_trace_env` pins that the keeper reads no other environment. The grace starts on leg EOF, read error, protocol error, a failed WELCOME write or the 8 s silence verdict, and is cancelled on resume. When it expires, the server stream is shut down and the session is removed, with no tombstone.
- **Drain policy.** `server_reader` reads only while `may_read_source()` allows it: the window while a leg is up, the 16 MiB cap while none is. On overflow the session is removed and the server stream closed. Bytes to the server go through an unbounded mpsc channel (bounded in practice by the peer's window) to a separate server-writer thread, which acks only after `write_all`.
- **Idle exit.** After 30 s with an empty table and no pending HELLOs, the keeper tries `.spawn` without blocking (and re-arms if it is held). It then does one accept from the backlog (serving it and re-arming if there is one) and re-checks for busy. Only then does it drop the listener, unlink the socket and release `.live` before `.spawn`. The `pending` counter also defers the exit while a HELLO is in flight. That HELLO read is bounded by `PRE_WELCOME_BOUND`, 20 s.
- **Routing.** `dispatch` routes exactly `[SESSION_FLAG]` and `[KEEPER_FLAG]`. Any other `--x-mosh-*` form gets upstream's InvalidInput message.
- **Observability.** `keeper.jsonl` gets these events: welcome{status}, server_connect, leg_up, leg_down{reason}, resume{replayed_up, replayed_down}, session_end{reason}.
- **Lower-risk checks.**
  - SIGPIPE stays ignored on the `remote-client-bridge` path (`begin_cli_output` is not called there), so writes to closed sockets in the keeper or attacher return EPIPE instead of killing the process.
  - Sockets accepted from the non-blocking listener are explicitly set blocking in `admit`, which covers BSD-style flag inheritance.

## Findings

1. **Must fix: flaky unit test (correctness of the suite).** `keeper::tests::server_eof_forwards_remaining_bytes_then_end` (src/mosh/remote/keeper.rs:1195) failed in 5 of 20 runs.
   - The keeper removes an eof session only when `eof && end_sent && endpoint.fully_acked()` (keeper.rs run_timers).
   - The test's `TestLocal::wait_end` stops pumping as soon as END arrives.
   - The final DATA chunk (300 000 - 4 x 64 KiB = 37 856 bytes) is under `ACK_EVERY_BYTES`. Its ACK is held for the 200 ms cadence and is never flushed.
   - So the session stays parked for the full 60 s grace, and `wait_trace("session_end")` times out after 5 s. The test passes only when read coalescing happens to push the last delivery over the 64 KiB ack threshold.
   - Fix: keep pumping or flushing the local until the session_end trace appears, or set `auto_ack` so that a final ACK is forced before asserting. The epic acceptance criterion "the full test suite passes" is not reliably met while this flakes.
2. **Should fix soon (product, same root as finding 1; composition and the bridge are another reviewer's scope).**
   - The removal predicate depends on a final ACK after `END{server-eof}`, and the task-10 report says the real local bridge exits before that ACK is sent.
   - The result: after every live handoff or server restart, the keeper holds the eof session, with up to 16 MiB of ring and an fd triple, until grace expiry or ghost eviction. That session also blocks idle exit for that long.
   - This is bounded, so it is not a blocker here. The spec's "removed once the local end has acked the final downstream offset" is satisfied in letter only. The fix belongs on the bridge (send the final ACK before closing).
3. **Minor: one accept error kills every session.** In `serve`, any accept error other than WouldBlock or Interrupted (for example ECONNABORTED, EMFILE or ENFILE) returns Err from the main loop. The keeper process then exits and drops every parked and live session. Transient accept errors should be logged and retried.
4. **Minor: undefined-state resume.** When `endpoint.welcome(Resumed, peer_offset)` fails because the peer's offset is outside the ring, `admit` drops the leg without any answer and leaves the session parked. The local end classifies that as EOF before WELCOME (Retryable) and retries until give-up. An `END` or `WELCOME unknown` there would end it deterministically. The state needs a buggy peer, so this is low priority.
5. **Minor: narrow race on END delivery.** `leg_writer` sets `end_sent = true` under the lock but writes the END bytes after releasing it. If the local already acked everything, `run_timers` (50 ms tick) can remove the session and shut down the leg in between, and the END is then never written. The recovery is benign: the next resume is answered `unknown`, which also closes the client socket. It costs one extra leg and a misleading trace.
6. **Minor: stray error line on the user's terminal.** On bring-up failure, `attacher::run` returns Err from `main`, which prints a debug-formatted `Error: Custom { .. "mosh keeper unavailable: .." }` to stderr. On a `--remote` leg 0 with inherited stderr, that line reaches the user's terminal just before the plain fallback. It is consistent with upstream's own bridge errors, but it is noise on a path the spec calls transparent.
7. **Minor: weak build-id pairing.**
   - `current_build_id()` is `version + "+" + HERDR_BUILD_ID`, and `HERDR_BUILD_ID` is a build-time option that is often unset for fork or dev builds.
   - For those builds, two binaries with the same version share a keeper across a rebuild. The old keeper keeps running the replaced binary.
   - The protocol version still guards wire compatibility, so this is only a weaker form of the spec's "a remote upgrade starts a fresh keeper".
8. **Minor: test hygiene.** `hello_grace_wins_over_env` calls `std::env::set_var("HERDR_MOSH_GRACE", "3600")` in the shared unit-test process and never restores it. `mosh::get()` caches its config in a OnceLock, so any later test in that process that reads the bridge config would silently get a 3600 s grace.

## Verdict
not ready (keeper unit test server_eof_forwards_remaining_bytes_then_end is flaky, failing 5 of 20 runs because the test local never flushes its final ACK)

## Must fix before landing
- src/mosh/remote/keeper.rs:1195 server_eof_forwards_remaining_bytes_then_end flakes (5 of 20 runs): the test local stops pumping at END, so the final sub-64 KiB ACK is never sent and the eof session is not removed within the 5 s wait; keep pumping or force the final ACK before asserting session_end

## Untested scope
- Per-HELLO table cells resume:false on a parked session and resume:false on an eof session (token-in-use) have no keeper test; only the live cell is covered
- resume:true on an eof session whose leg is still live (steal plus re-queued END{server-eof}) is untested; only eof-while-parked is covered
- Keeper silence-verdict path in run_timers (8 s, then leg_down, then grace start) has no in-process test
- attacher::run writing END{keeper-unavailable} to stdout and exiting non-zero is never exercised; the rendezvous tests assert bring_up errors and separately call report_unavailable, which is tautological
- Attacher spawn-error path (spawn() returning Err) and lock-open errors are not unit-tested
- Keeper resume with a peer offset outside the ring (welcome Err) is untested
- Keeper accept-loop error handling (non-WouldBlock accept errors) is untested
- Non-Linux and non-macOS unix peer_uid (does not compile there) and the non-unix dispatch path are inspection-only

## Deferred OK
- Keeper keeps an eof session until grace or eviction when the local bridge exits before its final ACK after END{server-eof} (bridge-side fix; bounded by grace)
- Any accept error other than WouldBlock or Interrupted ends the keeper process and all sessions; retry transient errors such as ECONNABORTED or EMFILE as a follow-up
- Resume with an out-of-ring peer offset drops the leg unanswered instead of END or unknown, so the local end retries until give-up (needs a buggy peer)
- end_sent is set before the END bytes are written, so run_timers can remove the session and drop an undelivered END in a narrow window (recovers through resume unknown)
- Attacher bring-up failure prints a debug-formatted Error line on remote stderr, visible on a --remote leg 0 before the plain fallback
- current_build_id falls back to version only when HERDR_BUILD_ID is unset, so same-version rebuilds share a keeper
- hello_grace_wins_over_env sets HERDR_MOSH_GRACE process-wide without restoring it
- Ring filling to exactly 16 MiB while down back-pressures the server instead of overflowing (logged in task 7)
- Keeper child never reaped by the attacher (logged in task 10); the zombie window is short because the keeper outlives the leg
- is_end identifies END by type byte only (logged in task 7)
- Keeper test module is about 800 lines in one file (logged in task 7)
- Unknown --x-mosh-* flag is rejected only after ensure_remote_server_running (logged in task 10; the upstream hook ordering is outside this cluster)
