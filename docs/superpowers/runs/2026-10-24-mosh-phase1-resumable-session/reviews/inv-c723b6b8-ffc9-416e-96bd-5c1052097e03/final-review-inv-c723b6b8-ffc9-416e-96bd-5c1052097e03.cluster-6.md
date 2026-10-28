# Final review: cluster 6 (mosh-core)

Epic: mosh-dqw (Mosh Phase 1: resumable remote session). Branch: super-auto/mosh-phase1-resumable-session.
Captured source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931. Both verified with `git rev-parse --verify <sha>^{commit}`. The review source binding (`bind-review-source`) succeeded before this file was written.

Files in scope: src/mosh/{mod.rs, session.rs, ring.rs, link_frames.rs, outage.rs, ssh_leg.rs, local_bridge.rs} (slices 000035-000038, 000042-000044).
Spec: docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/2026-10-24-mosh-phase1-resumable-session-design.md at head.

## Method

- Read all seven files at head, plus the spec sections they implement: the session wire protocol, ring caps and drain policy, local bridge behaviour, legs, discovery and fallback, and the outage banner.
- Opened src/remote/attach.rs (`SshStdioBridge::start`/`start_command`/`bridge_connection`), src/remote/saved.rs and src/mosh/remote/keeper.rs only to check specific risks (named below).
- Reproductions ran in a review-owned detached worktree at head, under the scratchpad (`review-c6-wt`, target dir `review-c6-target`). The caller checkout was not touched. Its HEAD is still 2bd5303c and `git status` is clean.
- Ran `cargo test --bin herdr -- mosh:: mosh_outage` four times in the review worktree. Three runs passed: 157 existing tests plus my 2 repro tests. One run failed in `local_bridge::tests::outage_overflow_ends_session` ("timed out waiting for ..."). It passed 3 of 3 times when run alone. A keeper test (other cluster) also failed once in a later full run.

## Findings

### 1. A leg protocol error drops in-order bytes already accepted in the same read (exactly-once violation)

`SessionEndpoint::on_leg_bytes` (src/mosh/session.rs:494-574) decodes every message in the input buffer in a loop:
- For each in-order `DATA` it advances `self.recv_offset` and pushes a `SessionEvent::Deliver` onto a local `events` vec.
- If a later message in the same buffer fails (gap, malformed header or JSON, misplaced message, ACK beyond end, bad WELCOME), it returns `Err(LegProtocolError)` and the `events` vec is dropped.
- Neither caller can see those Deliver events: local_bridge.rs:580-581 goes straight to `leg_ended`, and the keeper does the same. But `recv_offset` has already advanced, so the next HELLO/WELCOME reports the higher offset and the peer never replays those bytes.

The spec says "Protocol errors on a leg end the leg, not the session" and requires exactly-once delivery in both directions. This path instead leaves a silent hole in the stream while the session carries on:
- Downstream: torn herdr frames at the client.
- Upstream: a corrupted client stream at the server.

Reproduced at head with a unit test in the review worktree (`review_repro::review_batch_error_after_valid_data_loses_bytes`):
- One buffer holds `DATA(0,"ab")` followed by `DATA(9,..)` (a gap). The call returns Err and the keeper's `recv_offset` is 2.
- After `leg_down` and a `Resumed` handshake, the local end replays nothing. "ab" is never delivered.

The existing `gap_is_leg_error_and_state_is_untouched` test only covers a lone gap message, so it misses this.

Fix options, both small:
- Return the events gathered before the error alongside it.
- Or commit `recv_offset` only for events the caller actually receives.

### 2. A session-final END handed to a leg that then dies is never re-sent by the local bridge

`poll_send` consumes `final_end` with `take()` when it emits the END (session.rs:674-676). The keeper handles this: it re-queues `END{server-eof}` on resume (keeper.rs:445-449, "The END may have been lost with the previous leg"). The local bridge has no equivalent:
- `leg_dead` (local_bridge.rs:970-988) kills the leg, which drops the leg writer queue and `pending`, and it clears `end_flush`.
- Nothing calls `queue_final_end` again (grep: only local_bridge.rs:784 and :799).

Scenario: the client exits during a stalled but not yet dead leg.
1. `END{client-gone}` sits behind a blocked leg writer.
2. The 8 s silence verdict kills the leg.
3. The reconnect resumes and the ring replays, but no END is ever sent.
4. After resume the connection run stays `Linked` with `client_closed = true` and `end_flush = None`. `tick_linked` is the only path that closes, and it never fires. Heartbeats keep the keeper session live indefinitely, still counted as an active client by the server.
5. The bridge serves connections serially, so it also accepts no further client connection until `should_stop`.

This breaks the spec's ordering rule and the acceptance criterion "Client exit ends its keeper session within 2 s when the leg is alive" for that path. `ring-overflow` has the same gap.

Reproduced at the session level (`review_repro::review_final_end_emitted_then_leg_lost_is_not_resent`): after END is emitted, `leg_down` and a `Resumed` handshake, `poll_send` yields no END. The local-bridge consequence follows from inspection of `leg_dead`/`tick_linked`.

Mitigation: `--remote` quit and saved-machine reconnects usually also set `should_stop`, and `on_stop` sends a fresh END. So the practical exposure is the window between client EOF and bridge drop.

Fix options:
- In the bridge, re-queue the final END in `leg_dead` when `client_closed`, mirroring the keeper.
- Or make `SessionEndpoint` keep `final_end` until `leg_down` proves it was not flushed.

### 3. Flaky unit test

`local_bridge::tests::outage_overflow_ends_session` failed 1 of 4 full mosh-suite runs, under parallel load, with a fixed 10 s/20 s wait. It passed 3 of 3 times in isolation. It writes 9 MiB through a socket and depends on scheduling. This is a CI reliability issue, not a product defect.

### Checked and found consistent with the spec

- **Codec:** `[u8 type][u32 LE len]` layout; the header is validated before any payload is buffered; DATA is capped at 64 KiB plus the 8-byte offset; control messages are capped at 4 KiB.
- **Token:** 128 bits from /dev/urandom, hex-encoded.
- **answer_hello:** `resume:true` never yields `new`; `v` mismatch gives `protocol-version`.
- **Dedup and gaps:** overlap is discarded; a lone gap is a leg error.
- **Acks:** sent only for delivered bytes, every 64 KiB, after 200 ms, or with a heartbeat. Acks are re-issued after a resume because `last_ack_sent` is reset.
- **Window:** while a leg is up, in-flight is computed from `send_next - ring.start` and never exceeds 2 MiB. While down, the read limit is the ring cap.
- **Ring:** overflow is all-or-nothing and reported.
- **Send ordering:** a control message never waits behind more than one chunk; the session-final END waits until the ring is sent.
- **Timers:** silence clock starts at WELCOME and resets on any byte, including partial messages; 20 s pre-WELCOME bound; reconnect setup deadline is min(remaining grace, 15 s).
- **FrameSplitter:** whole-frame hold-back; frames over `MAX_GRAPHICS_FRAME_SIZE` are rejected; the trailing partial frame is dropped on server EOF.
- **Injected link frame:** built with upstream `write_message` and decodes as `Ignored` upstream.
- **OutageState:** observe/paint, the 2 s stale clear and width-safe truncation.
- **Legs:**
  - Leg 0 builds the same command as upstream `bridge_connection`.
  - Reconnect legs are always BatchMode with `ControlPath=none`/`ControlMaster=no` and `control_path: None`.
  - Classification precedence is 255, then END, then remote exit, then EOF/bound, with the per-kind outcomes the spec table requires.
  - Back-off is 1, 2, 4, 8, 8 s.
  - Candidate re-selection needs both the capability and the endpoint requirement.
- **Local bridge:**
  - Pre-WELCOME gate holds, and the untouched stream goes to the plain fallback.
  - First injection at the dead verdict, then every 1 s, only after the first frame, never past give-up; `Linked` is injected after resume.
  - Give-up equals grace from link loss.
  - Ghost-eviction map is process-global and keyed by local socket path.
  - Stop path sends END within 500 ms.
  - Server EOF delivers whole frames, drops the partial one, then sends a final ack.
  - A fatal reconnect reaches `failure_tx`.
  - The plain-path `remote_command` matches `SshStdioBridge::start` exactly.

## Verdict
not ready (leg protocol error after valid DATA in one read silently drops bytes, and the local bridge never re-sends a session-final END lost with a dying leg)

## Must fix before landing
- src/mosh/session.rs:494-574 `on_leg_bytes` advances `recv_offset` for DATA decoded earlier in a buffer, then returns Err on a later bad message and drops those Deliver events; the bytes are never delivered or replayed (repro `review_batch_error_after_valid_data_loses_bytes`), which breaks exactly-once on the "protocol error ends the leg, not the session" path.
- src/mosh/local_bridge.rs:970-988 `leg_dead` does not re-queue `END{client-gone}`/`END{ring-overflow}` once `poll_send` has consumed it (session.rs:674); after a resume the connection run stays Linked with the client closed, the keeper session lives on, and the serial bridge accepts no new connection until `should_stop`. The keeper already re-queues server-eof (keeper.rs:445-449).

## Untested scope
- No test feeds valid DATA followed by an invalid message in one `on_leg_bytes` call (only a lone gap is tested), on either the local or the keeper side.
- No test covers a final END emitted to a leg that dies before flush, followed by a resume (session-level `final_end_survives_leg_down_and_goes_after_replay` only covers the not-yet-emitted case).
- local_bridge::tests::outage_overflow_ends_session is timing-sensitive: it failed 1 of 4 full mosh-suite runs under parallel load in the review worktree and passed 3 of 3 alone.
- The non-Unix `start_ui_bridge` branch (src/mosh/mod.rs:94-106) and the `cfg_attr(not(unix), allow(dead_code))` build are inspection-only; no Windows build or clippy was run.
- Leg teardown joining the stderr capture thread after kill (ssh_leg.rs:204-213), with a stderr-holding ProxyCommand grandchild, is untested; it can block the leg I/O thread.
- Graceful `close_client` with a client that never reads (local_bridge.rs:1077-1093) blocks the bridge thread until `should_stop`; not exercised.

## Deferred OK
- local_bridge::start re-implements upstream `start_command`'s bind/permission/accept loop instead of calling it; the struct-literal return is design-mandated, but upstream changes to that block will not propagate (drift risk).
- `resume` trace emits `replayed_down: null` on the local side (spec lists the field; the local end cannot observe it).
- ssh_leg.rs dial_command maps any ssh spawn failure to Fatal, even on a reconnect leg where a transient EAGAIN would end the session.
- ReplayRing stores bytes in a VecDeque<u8> with byte-wise extend/collect and never shrinks; fine at 8/16 MiB caps but is a later efficiency candidate.
- Decoder drains from the front of a Vec per message (quadratic in buffered bytes for large reads); bounded by read size and acceptable now.
- rustfmt --check drift in src/mosh/ssh_leg.rs (carried from mosh-dqw.6).
- local_bridge.rs is about 2,180 lines (code plus tests) in one file.
- OutageState's 2 s stale clear takes effect only on the next repaint (by design).
