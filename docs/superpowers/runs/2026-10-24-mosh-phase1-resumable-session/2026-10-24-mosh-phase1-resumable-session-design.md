# Mosh Phase 1 — link-level resumable remote session (design)

Fork plan: `HERDR_MOSH_VISION.md` §4.1, §5 (Phase 1), §6. Detailed verified touch map: `inputs/herdr-mosh-touch-map.md` §2.1 (this spec's source of file:line facts). Background: `inputs/herdr-remote-report.md`, `inputs/mosh-report.md`.

## Goal

`herdr --remote <target>` and saved-machine connections keep the same TUI session through an ssh drop or a stalled link. Within a 60 s grace period they resume with no lost or duplicated input, and with no `ClientShellSnapshot` or full `PaneSurface` after resume. While the link is down, an outage banner shows how long ago the remote was last heard from. All of this lands in fork-owned code under `src/mosh/`, with about 11 hook lines in upstream files, and a fork build can point self-update at its own manifest so that upstream auto-update never replaces it.

## Problem description

Every remote herdr connection is one `ssh -T` stdio pipe. A Wi-Fi switch, a NAT rebind, a laptop sleep or a dead ControlMaster kills that pipe, and then:
- `herdr --remote` exits with "lost connection" (`src/client/mod.rs:2278-2283`);
- a saved machine tears down its endpoint and re-runs the full bootstrap, handshake, snapshot and full surface;
- input typed during the undetected-dead window is silently lost (`src/client/endpoint/writer.rs:186-256`).

The herdr server already keeps panes alive. What is missing is a link that outlives a single ssh process.

## Main challenges

1. **Make the outage invisible to two unmodified peers.** The server's client connection must stay registered. The TUI's reader must never see EOF, a torn frame or a 10 s health expiry.
2. **Stay inside about 11 upstream lines.** Upstream is very active, so none of `headless.rs`, `client_transport.rs`, `handshake.rs`, `wire.rs`, `app/mod.rs` or `main.rs` may be touched.
3. **Deliver exactly once in both directions** across any number of leg reconnects, with bounded memory.
4. **Reconnect without the ControlMaster.** A TCP-dead master hangs mux clients indefinitely.
5. **Fail safe.** Any failure the session cannot absorb falls back to today's behaviour.

## Key decisions made

Resume is a byte-stream session between a fork **local bridge**, which replaces `SshStdioBridge`'s pump at the two UI swap points, and a fork **keeper** daemon on the remote. The keeper holds the server socket open while ssh is down. Each ssh process is only a **leg**, and legs come and go.
- Both ends keep byte-offset replay rings and retransmit from the peer's received offset.
- The local bridge forwards only whole herdr frames to the client.
- During an outage it injects `EndpointControl{kind:"x-mosh.link.v1"}` keepalive and status frames. These keep saved-machine health alive and drive the outage banner.

Discovery uses the existing `status client --json` capability list. Anything unsupported falls back to the unchanged `SshStdioBridge`.

The protocol-level alternative (surface acks, sequenced input, server-side parking) was rejected for Phase 1 in decision D1: it needs about 220 upstream lines, about 55 of them in hot files.

## Architecture

```
herdr client (TUI, unmodified except banner hooks)
   │  user-facing local socket (same path/0600 contract as today)
   ▼
local_bridge (src/mosh/local_bridge.rs)  ── one Session per accepted client connection
   │  session protocol (src/mosh/session.rs) over the current leg
   ▼
leg: ssh child (src/mosh/ssh_leg.rs) ──► sshd ──► `herdr [--session S] remote-client-bridge --x-mosh-session-v1`
                                                     = attacher (src/mosh/remote/attacher.rs): stdio ⇄ keeper socket pump
                                                        │ unix socket in a per-uid 0700 dir; peer uid checked;
                                                        │ name hashes protocol version, build id, server socket path
                                                        ▼
                                                    keeper (src/mosh/remote/keeper.rs), detached daemon:
                                                    token → { server LocalStream, SessionEndpoint (one outbound
                                                              ring: server→client bytes), state, grace }
                                                        │ unix socket (client_socket_path(), unchanged)
                                                        ▼
                                                    herdr server (unmodified)
```

### Components and responsibilities

| Unit (new file) | Owns | Does not own |
|---|---|---|
| `src/mosh/mod.rs` | Public fork API: `start_ui_bridge`, `pub(in crate::remote)`. Constants: `STATUS_CAPABILITY = "x-mosh-resume-bridge-v1"`, the flags `--x-mosh-session-v1` / `--x-mosh-keeper-v1` (`LINK_KIND` lives in `src/mosh/outage.rs`; fork code refers to `crate::client::mosh_outage::LINK_KIND`). Env knobs: `HERDR_MOSH_RESUME=0` kill switch; `HERDR_MOSH_GRACE` (seconds, default 60, clamped 5..=3600) | Any I/O |
| `src/mosh/session.rs` | The session wire protocol (below), the per-direction offset and ack state machine, the flow-control rule (§Ring caps and drain policy), the link timers, and hello/welcome negotiation. Pure: it operates on buffers, no sockets | Leg lifecycle, rings' storage policy |
| `src/mosh/ring.rs` | A bounded replay ring keyed by absolute byte offset: `push`, `ack(offset)`, `replay_from(offset)`. Overflow is reported, never silently dropped | What to do on overflow (the caller decides) |
| `src/mosh/link_frames.rs` | Splitting the herdr frame stream into whole frames (u32-LE length prefix, `wire.rs:1647-1715` framing; frames over `MAX_GRAPHICS_FRAME_SIZE` are a protocol error that ends the session); encoding injected `x-mosh.link.v1` frames with the existing `crate::protocol` frame writer | Status semantics |
| `src/mosh/ssh_leg.rs` | Capability probe and candidate re-selection, per-leg remote command and `Command` construction, leg spawn/kill (the leg owns its ssh child, its exit status and its piped stderr), leg failure classification (§Legs), reconnect back-off and per-leg setup deadline | Session bytes |
| `src/mosh/local_bridge.rs` | The user-facing socket (accept loop), the client↔session pump (leg I/O thread plus a separate client-writer thread), whole-frame hold-back, injection rules, the give-up deadline, session-end triggers, the process-global ghost-eviction token map, returning a working `SshStdioBridge` value | ssh argument details |
| `src/mosh/remote/mod.rs` | `dispatch(args) -> Option<io::Result<()>>` for the `--x-mosh-*` flags; the rendezvous contract (private dir, keeper socket and lock paths, owner checks) shared by attacher and keeper | — |
| `src/mosh/remote/attacher.rs` | Locating or spawning the keeper, verifying the keeper's peer uid, pumping stdio ⇄ keeper socket, and reporting its own bring-up failure as a session `END` | Session semantics (beyond that one `END`, it is a dumb pump) |
| `src/mosh/remote/keeper.rs` | The token table and HELLO state table, server connections, rings, drain policy, grace timers, leg stealing, ghost eviction, server EOF, lock-ordered idle exit | The herdr protocol (it never parses frames) |
| `src/client/mosh_outage.rs`, mounted from `client/mod.rs` | `LinkStatus` (the JSON payload of `x-mosh.link.v1`); per-endpoint outage state; `observe(endpoint_id, kind, data) -> bool`; `paint(frame, active_endpoint)` for the banner | Detecting outages (the bridge does) |

### Session wire protocol (`session.rs`)

The protocol starts on the leg after the existing ready marker `herdr-remote-output-ready:1`, so upstream's preamble stripping (`discard_remote_output_preamble`, used by the leg itself, §Legs) still applies. Every message has the form `[u8 type][u32 LE len][payload]`.

| Type | Direction | Payload |
|---|---|---|
| `HELLO` (1) | local→keeper | JSON `{v:1, token:hex32, resume:bool, recv_offset:u64, grace_ms:u32, previous_token:hex32?}`. `resume` is `false` on the first HELLO of a session and `true` on every later one |
| `WELCOME` (2) | keeper→local | JSON `{v:1, status:"new"\|"resumed"\|"unknown", recv_offset:u64}` |
| `DATA` (3) | both | `[u64 LE offset][bytes]`: the absolute offset of the first byte in that direction's stream. At most 64 KiB of bytes per message (`MAX_DATA_CHUNK`); a larger one is a leg protocol error |
| `ACK` (4) | both | `[u64 LE offset]`: all bytes below the offset have been delivered (rule 3) |
| `HEARTBEAT` (5) | both | empty |
| `END` (6) | both | JSON `{reason:str}`: the session is over and both sides discard it. The keeper also sends it before any `WELCOME` to refuse a `HELLO` (§Keeper "Per HELLO"), and the attacher sends it when it cannot reach a keeper (§Keeper "Rendezvous") |

**Rules**

1. **Offsets.** They start at 0 per direction when the session is created.
2. **Receive and dedup.** A receiver delivers `DATA` bytes at or above its `recv_offset` and discards the overlap below it. A gap, where the offset is above `recv_offset`, is a protocol error that ends the leg (not the session).
3. **Acks mean delivered.** `ACK(n)` means every byte below `n` has been written to the local consumer (the client socket at the local bridge, the server socket at the keeper) or, at the local bridge only, sits in the partial-frame hold-back (§Local bridge behaviour). Bytes that are received but still queued for the consumer are not acked. An `ACK` is sent after at least 64 KiB newly delivered or 200 ms since the last ack, whichever comes first, and on every heartbeat. Flow control built on this is in §Ring caps and drain policy.
4. **Resume.** On `WELCOME resumed`, each side replays from its ring starting at the peer's reported `recv_offset`, within the in-flight window (§Ring caps and drain policy).
5. **New and unknown.**
   - The keeper answers `new` only to `resume:false`, and never answers `new` to `resume:true`. The local bridge treats `new` on a reconnect leg as session-gone (the same as `unknown`).
   - `unknown` for a resume attempt means the session is gone. The keeper keeps no tombstones: an ended session (grace expiry, ring overflow, `END`, eviction) is removed, so a later resume of it is answered `unknown`. The local bridge closes the client socket, and today's disconnect path runs (exit with the reattach hint for `--remote`, supervisor reconnect for saved machines).
6. **Liveness and link timers.** These are all the link timers, with their start events. Each is a compile-time constant in `session.rs`.
   - *Heartbeat:* `HEARTBEAT` is sent every 2 s while a leg is up.
   - *Silence:* once `WELCOME` has been exchanged, a leg is dead after 8 s with no inbound byte. Any inbound byte resets it, partial messages included. The clock starts when the local end reads `WELCOME` and when the keeper writes it.
   - *Pre-`WELCOME`:* before `WELCOME`, on any leg, the local end allows 20 s from writing `HELLO`; the 8 s silence verdict does not apply yet. `HELLO` is written as soon as the ready marker is seen, and the shell prints that marker before `exec herdr` (`posix_remote_output_command`), so this bound must cover everything the remote does after it: `ensure_remote_server_running`'s 5 s server-spawn wait on a cold host (`host.rs:40-61`), herdr startup, and the attacher's 5 s keeper wait. Expiry is classified like EOF before `WELCOME` (§Legs "Failure classification").
   - *Ordering:* only a pending `ACK`, `HEARTBEAT` or a pre-`WELCOME` refusal `END` may go ahead of the next `DATA` chunk, so these control messages never wait behind more than one 64 KiB chunk. A session-final `END` (`server-eof`, `ring-overflow`, the client-EOF `END{"client-gone"}`) is queued after every ring byte has been sent, so it never overtakes stream data. The one exception is stop (§Local bridge behaviour "Session-end triggers").
   - *Leg setup:* a reconnect leg must reach `HELLO` within min(remaining grace, 15 s) of being spawned, or it is killed as `Retryable`. Every reconnect leg also carries `ConnectTimeout=10` (§Legs). Leg 0 has no fork setup deadline before `HELLO`, because interactive prompts may take arbitrarily long, as today. After `HELLO`, its remote bring-up is bounded by the 20 s pre-`WELCOME` bound above.
   - *Back-off and give-up:* see §Legs and §Local bridge behaviour "Give-up deadline". The first status frame is injected at the dead verdict (§Local bridge behaviour "Injection").
7. **Tokens.** 128 bits read from `/dev/urandom`, hex-encoded. They are not a security boundary. The boundary is the ssh channel, the per-uid 0700 rendezvous directory and the attacher's peer-uid check on the keeper socket (§Keeper "Socket").
8. **Versioning.** The capability string `x-mosh-resume-bridge-v1`, the flag `--x-mosh-session-v1` and HELLO/WELCOME `v:1` carry one session-protocol version. Changing any constant both ends must agree on (message layout, `MAX_DATA_CHUNK`, the window, the heartbeat or silence timers) bumps all three together. The keeper answers a `HELLO` whose `v` differs from its own with `END{"protocol-version"}`. HELLO carries no build id: the local and remote builds need not be equal.

### Ring caps and drain policy

There is one flow-control rule, implemented once in `SessionEndpoint` (`session.rs`) and applied identically by both ends. Each end is a **sender** for one direction: the local bridge sends client→server bytes and reads its **source**, the client socket; the keeper sends server→client bytes and reads its source, the server socket. Each end has exactly one outbound replay ring, which holds that direction's unacked bytes (sent or not yet sent). The receive side has no ring.

1. **Chunking.** `DATA` carries at most 64 KiB (`MAX_DATA_CHUNK`). `ACK`, `HEARTBEAT` and a pre-`WELCOME` refusal `END` go ahead of the next chunk; a session-final `END` waits for the ring to be sent (rule 6 "Ordering").
2. **Ack after delivery.** An end acks only what it has delivered (rule 3). Each end writes to its consumer on its own **consumer-writer thread**, separate from the leg I/O thread, so a slow consumer never blocks leg reads, heartbeats, acks, injection or `should_stop` polling. The consumer-writer queue is bounded by the peer's window (point 3), because the peer cannot have more than a window of unacked bytes in flight. The local partial-frame hold-back is bounded by one maximum herdr frame (`MAX_GRAPHICS_FRAME_SIZE`) plus one chunk. So receive-side memory is at most window + max frame + chunk per session end.
3. **Leg up: fixed in-flight window.** While a leg is up, a sender reads its source only while its ring holds less than `WINDOW` = 2 MiB (unsent plus unacked). When the window is full it stops reading the source until acks free space. 2 MiB is parity with OpenSSH's default session channel window (`CHAN_SES_WINDOW_DEFAULT` = 64 × 32 KiB = 2 MiB, `channels.h`), so leg-up throughput (about `WINDOW`/RTT) and backpressure match today's plain bridge. The server's socket write blocks, so its single render slot (`client_transport.rs:276`, `320-329`) coalesces as it does today. The client writer blocks the same way it blocks on a slow ssh pipe today. After a resume, replay obeys the same window: ring bytes beyond the window wait for acks. `WINDOW` is a fixed compile-time constant, not adaptive pacing.
4. **Leg down: buffer up to the ring cap.** While no leg is up, a sender keeps reading its source into its ring up to the ring cap: 8 MiB local (client→server), 16 MiB keeper (server→client). This keeps the client writer's 5 s write deadline (`writer.rs:15`) from firing, and keeps draining the server's uncapped control lane (`client_transport.rs:310-318`). On overflow the session ends, with no backpressure fallback:
   - local: the bridge closes the client socket at a frame boundary and sends `END{"ring-overflow"}` on the next leg if one comes up (best effort);
   - keeper: it closes its server stream and removes the session, so the next resume is answered `unknown`.
   A single client message larger than the 8 MiB local cap that is written during an outage (for example a 16 MiB `ClipboardImage`) therefore ends the session (outcome (b)). That is an explicit, documented end: while a leg is up, the same message streams through the window.
5. **Ring caps are outage buffers, not flow control.** `ReplayRing` reports overflow. Only point 4 can overflow it, because under point 3 a sender never reads its source while its ring holds `WINDOW` or more (a post-resume backlog drains before reading restarts).

### Local bridge behaviour

**Whole frames.** Downstream bytes are passed to the client only as complete herdr frames. A partial frame is held back until it completes. The hold-back is bounded (§Ring caps and drain policy point 2), and its bytes count as delivered for acks (rule 3), so a frame larger than the window still completes.

**Threads.** Each connection run has a leg I/O thread (session messages, timers, `should_stop` polling at ≤100 ms) and a client-writer thread that owns every blocking write to the client socket: whole frames and injected frames, in order. The leg I/O thread never writes to the client socket itself.

**Pre-WELCOME gate.** The bridge reads nothing from the client socket until leg 0's `WELCOME new`. The client's handshake bytes stay in the socket, so a leg-0 fallback (§Discovery and fallback) can hand the untouched stream to `bridge_connection`.

**Injection.** While a resume is in progress, the bridge writes injected frames into the client socket. Each one is a `ServerMessage::EndpointControl { kind: "x-mosh.link.v1", data: <LinkStatus JSON> }` encoded with the existing frame writer. Rules:
- only after the first complete downstream frame (the handshake `Welcome`) has been delivered;
- only between whole frames. Because only whole frames reach the client-writer queue, the client stream is always at a frame boundary between queued items;
- the first one at the dead verdict (8 s silence or leg EOF), then every 1 s while resuming;
- never after the give-up deadline.

**Health margin.** The client's health check sends a ping every 5 s and expires 10 s after an unanswered ping (`client/endpoint/health.rs`). Session `HEARTBEAT`s are not forwarded to the client. Worst case: a ping is sent just before the leg stalls at time S. The last inbound byte is at or before S, so the dead verdict comes by S+8 and the first injected frame is written then. The client would expire at S+10, which leaves a margin of about 2 s minus the client-writer queue latency. The queue holds at most one window (2 MiB) ahead of the injected frame. A test stalls the leg immediately after a client ping and asserts that no health expiry follows.

**Status payloads.**
- During an outage: `{"state":"resuming","last_contact_ms":N}`.
- After a successful resume: one `{"state":"linked"}`.
- The client counts every injected frame as liveness (`client/mod.rs:1457-1458`). It decodes unknown kinds as `Ignored` (`client/endpoint/control.rs:61-66`), unless the banner hook consumes them first.

**Give-up deadline.** It equals the grace period from the moment of link loss. At the deadline the bridge closes the client socket.

**Session-end triggers.**
- Client EOF: the bridge sends `END{"client-gone"}` over the live leg, after the remaining ring bytes (rule 6 "Ordering"), or over the next leg when none is live; this is best effort.
- `SshStdioBridge` drop or stop: the leg I/O thread polls `should_stop` every ≤100 ms and never blocks on the network or on the client socket. When `should_stop` is set and a leg is live, the leg I/O thread stops sending `DATA`, writes and flushes `END{"client-gone"}` after the chunk in progress, bounded to 500 ms, and then tears the leg down. Ring bytes not yet sent are abandoned, as today's plain bridge abandons what is in its pipe when it stops. So whichever of client EOF and stop the bridge sees first, a live leg ends the keeper session promptly instead of parking it for the grace.
- Grace expiry.
- Ring overflow (§Ring caps and drain policy point 4).

**Ghost eviction.** The last session token is remembered in a process-global map in `local_bridge.rs`, keyed by the bridge's local socket path (the `local_socket` argument of `start_ui_bridge`), not per bridge instance. That key is stable across a saved machine's reconnects within one process: `saved_bridge_path(profile_id)`, even though `connect_saved_ssh` builds a new bridge on each reconnect (`saved.rs:20-55`, `supervisor.rs:477`). It is unique per `--remote` process. A new client connection's `HELLO` carries the stored token as `previous_token`, and the keeper ends that session immediately. A local process restart loses the map. The ghost session it leaves then lives until its grace expires, which is the parked-client cost D2 already accepts.

**Concurrency.** Connections are served one at a time, as upstream does (`attach.rs:3056-3068`).

### Legs (`ssh_leg.rs`)

**Leg 0** (the first connection) uses exactly the ssh options today's path uses: for `--remote`, interactive with an inherited stderr, so password and host-key prompts still work; for saved machines, noninteractive.

**Leg process ownership.** The leg spawns and owns its ssh child itself, from a `Command` built with upstream builders (`apply_managed_ssh_options`, `apply_noninteractive_ssh_options`, `attach.rs:1403-1440`). It does not use `bridge_child`, because `bridge_child` returns only a formatted `io::Error` and suppresses the exit error on client close or stop (`attach.rs:3287-3445`). The child's stdin/stdout are the leg's duplex byte stream; no socketpair is involved. The leg strips the ready marker with upstream's `discard_remote_output_preamble` (`attach.rs:3459`), reachable through the `#[path]` mount. So the leg has the child's `ExitStatus` and, on reconnect legs, its piped stderr (drained by a reader thread, capped like `capture_ssh_stderr`) directly. All of this is fork code: no upstream hook.

**Reconnect legs**
- Always noninteractive: `apply_noninteractive_ssh_options` (BatchMode, `ConnectTimeout=10`, `ConnectionAttempts=1`, ServerAlive), piped stderr, so prompts never land on the raw-mode TUI. This applies with and without managed config.
- Always bypass the ControlMaster: every reconnect leg passes `-o ControlPath=none -o ControlMaster=no` on the command line, whether or not ssh config is managed. Command-line options win over every config file, including the user `~/.ssh/config` that the managed config `Include`s (first value wins, `ssh_config(5)`). With managed config the leg also uses a clone of `ManagedSshOptions` with `control_path: None`, so no `-S` is added (`attach.rs:725-731`, `1421-1440`).
- One leg at a time. Back-off 1, 2, 4, 8, 8… seconds until the give-up deadline. Each leg's setup is capped by rule 6 "Leg setup".

**Remote command per leg.** `posix_remote_output_command("exec " + exe.command(session_args(.., ["remote-client-bridge", "--x-mosh-session-v1"])))`, reusing upstream builders (`attach.rs:367-394`, `417-436`).

**Failure classification.** A leg that ended after `WELCOME` (EOF or silence verdict) is always `Retryable`, because the link died. A leg torn down for stop or session end is not classified. A reconnect leg killed by its setup deadline is `Retryable`. A reconnect leg whose ssh child cannot be spawned at all (the spawn itself returns an error, for example `EAGAIN` from `fork`) is `Retryable` within the give-up deadline: it has no exit status and says nothing about the remote. A leg that ended before `WELCOME` is classified from structured signals only, checked in this order (the first that applies wins):
1. **ssh-level failure:** exit status 255, which ssh reserves for its own errors (`ssh(1)`).
   - On a reconnect leg, it is `Fatal` when the piped stderr contains `Permission denied` (authentication) or `Host key verification failed` (host key), and `Retryable` otherwise (unreachable, timeout, reset).
   - On leg 0 it is reported as today (`failure_tx`, client socket closed), with no classification needed. The plain bridge would fail the same way, and for `--remote` it would re-prompt.
2. **Explicit refusal:** an `END` from the attacher or keeper before `WELCOME` (`keeper-unavailable`, `server-connect-failed`, `protocol-version`, `token-in-use`; on a reconnect leg only `keeper-unavailable` and `protocol-version` can occur, because a `resume:true` `HELLO` never opens a server connection).
   - On leg 0 it is `Unsupported` and takes the plain fallback (§Discovery and fallback).
   - On a reconnect leg it is `Fatal`: the remote has positively said it can no longer host this session.
3. **Remote error exit:** a non-zero exit status other than 255 that the bridge did not cause by killing the leg: the "unsupported remote client bridge option" rejection, the generation mismatch from `ensure_remote_server_running` (`host.rs:40-61`), any other remote error. A leg the bridge killed (pre-`WELCOME` bound, setup deadline) never reaches this class.
   - On leg 0 it is `Unsupported` (plain fallback).
   - On a reconnect leg it is `Fatal`.
4. **Otherwise:** EOF before `WELCOME` with no `END` and no remote error exit, or no `WELCOME` within the 20 s pre-`WELCOME` bound (rule 6).
   - On leg 0 it is `Unsupported` (plain fallback).
   - On a reconnect leg it is `Retryable`, within the give-up deadline: a link that stalls or drops between `HELLO` and `WELCOME` looks exactly like this, and a still-resumable session must not end for it.

`Fatal` stops retrying at once and closes the client socket at a frame boundary (outcome (b)). No stderr is read on leg 0, so the inherited stderr of `--remote` costs nothing. The ssh `Command` does not set a locale: OpenSSH client messages are not localized, so the two match strings are stable as they are, and setting `LC_ALL` could be forwarded by a `SendEnv LANG LC_*` config and change the locale of the remote server that a leg starts.

### Discovery and fallback

`start_ui_bridge(ssh, remote_herdr, local_socket, session, options, noninteractive, stop) -> io::Result<SshStdioBridge>`:
1. With `HERDR_MOSH_RESUME=0`, a Windows remote, a non-Unix local build (`#[cfg(not(unix))]`: the resume path is not compiled there), or no capable candidate: call `SshStdioBridge::start` unchanged.
2. Otherwise probe `remote_client_status` once for `STATUS_CAPABILITY`.
3. If the selected binary lacks it, walk `remote_binary_candidates` for one that has both the capability and `supports_endpoint_requirement` (`attach.rs:1519-1540`, `1588-1593`). If none qualifies, use the plain bridge. For `--remote`, print one hint line naming `HERDR_REMOTE_BINARY`.
4. Return an `SshStdioBridge` struct literal that owns the local-bridge thread, its `should_stop` and a `failure_tx`, so `reported_failure()` and `Drop` keep their semantics (`attach.rs:2996-3002`, `3101-3120`).

**The leg-0 rule** (stated once here; §Legs, `LegDialer`, `LocalBridge` and outcome (c) refer to it). The bridge reads nothing from the client until leg 0's `WELCOME new` (§Local bridge behaviour "Pre-WELCOME gate"). Leg 0 then ends in one of three ways:
- **`WELCOME new`:** the session exists. From here on, retry and the give-up deadline apply.
- **ssh-level failure** (exit 255, §Legs "Failure classification"): today's semantics. No retry; the failure goes to `failure_tx` and the client socket is closed, as when today's single ssh child fails.
- **Any remote-side failure before `WELCOME`** (`LegError::Unsupported`): that one connection is served by upstream's plain `bridge_connection` with today's leg-0 options, given the untouched client stream. This covers a downgraded binary rejecting `--x-mosh-session-v1`, an attacher `END{"keeper-unavailable"}` (keeper spawn, lock, private-dir or socket error, the 5 s wait, a peer-uid mismatch), a keeper `END` refusal (for example `server-connect-failed`), a non-255 exit, EOF before `WELCOME`, or no `WELCOME` within the 20 s pre-`WELCOME` bound. No stderr string is matched, so this works for `--remote` with inherited stderr. A generation mismatch fails the plain bridge the same way it fails today. The next client connection tries resume again.
  - *Accepted limitation:* the fallback opens a second, independent ssh connection, after the failed leg has spent up to the 20 s pre-`WELCOME` bound. Without a ControlMaster, an interactive `--remote` user sees a second password or OTP prompt. Because no failure is remembered, this cost repeats on every new connection while the host-side cause persists (for example an unwritable rendezvous dir). The capability probe keeps incapable remotes off this path, so it fires only on keeper bring-up faults.

**On a reconnect leg**, the client has already handshaken through the keeper, and splicing in a fresh server connection would desync it. A positive remote refusal there (an `END` before `WELCOME`, or a remote error exit, §Legs "Failure classification" classes 2 and 3) is `Fatal`, and the session ends (outcome (b)). Silence or EOF before `WELCOME` is `Retryable` within the give-up deadline. There is never a plain fallback mid-session.

### Keeper (`remote/keeper.rs`)

**Rendezvous (one contract for attacher and keeper).** `mosh::remote` defines the private directory, the keeper socket path and two sibling lock files. The attacher and the keeper both use that single definition, owned by the scaffold.
- *Private directory:* `<std::env::temp_dir()>/herdr-mosh-<euid>`, or `/tmp/herdr-mosh-<euid>` when the socket path below would not fit `sun_path` (`fits_unix_socket_path`, `attach.rs:3645`). It is created with `crate::platform::create_remote_private_dir` (mode 0700), and `AlreadyExists` is accepted. Before every use it is validated with `symlink_metadata`: it must be a directory, not a symlink, owned by the current euid, with mode exactly 0700. This is the same check upstream applies to its `hssh-<uid>` ControlMaster dir (`unix_common.rs:766-778`), re-implemented in fork code because that helper is private to `platform`. Because only the owner can create entries in it, the socket and lock files inside need no per-file owner check.
- *Socket:* `<dir>/k-<h>.sock`, where `h` is the first 24 hex digits of SHA-256 (`sha2`, already a dependency) over (euid, session name, session-protocol version, build id, the resolved `client_socket_path()`). The attacher resolves `client_socket_path()` in its own environment. The keeper it spawns inherits that environment, so it resolves the same path. Attachers whose `HERDR_SOCKET_PATH` / `HERDR_CLIENT_SOCKET_PATH` differ (parallel e2e tests among them) therefore get different keepers. The build id keeps an attacher paired with a keeper of the same binary, so a remote upgrade starts a fresh keeper (its sessions answer `unknown`). The socket is bound with mode 0600.
- *Locks:* `<dir>/k-<h>.live`: the keeper holds an exclusive `flock` on it for its whole lifetime. A second keeper that cannot take it non-blocking exits at once. `<dir>/k-<h>.spawn`: held by an attacher while it checks for and spawns a keeper, and by the keeper during idle exit.
- *Peer check:* after every successful connect, the attacher reads the peer's uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS, via `libc`) and requires it to equal its own euid before it forwards a single byte.

The attacher first validates the directory and tries to connect. On `ENOENT` or `ECONNREFUSED` it takes the spawn lock and retries the connect. If that fails, it tries a non-blocking `flock` on the live lock:
- If it acquires the live lock, no keeper is alive. It unlinks any stale socket file, releases the live lock, spawns the keeper, waits up to 5 s for the socket and connects.
- If it cannot acquire the live lock, a keeper is alive but not yet listening. It retries the connect for up to 5 s, then re-tries the live lock once, which covers a keeper that exited meanwhile. If that acquires the lock, it continues as in the branch above.

The attacher then releases the spawn lock. A crashed keeper's stale socket is therefore always recovered, and two attachers never spawn two keepers.

**Attacher failure reporting.** If any bring-up step fails, the attacher writes one session `END{"keeper-unavailable"}` on its stdout and exits non-zero. The steps are directory validation, a lock or spawn error, the 5 s wait, and a peer-uid mismatch. Its stdout is the leg, after the ready marker. This is the only message it ever originates. Leg 0 maps it to the plain fallback, and a reconnect leg maps it to `Fatal` (§Discovery and fallback). Once connected, the attacher is a pure byte pipe, and the session protocol (`HELLO` first) is spoken end to end by the local bridge and the keeper.

**Configuration across hosts.** The grace period is chosen once, on the client (`HERDR_MOSH_GRACE`), and travels in `HELLO.grace_ms`. The keeper uses the received value and never reads its own environment for it. The heartbeat interval, the silence timeout, `MAX_DATA_CHUNK` and `WINDOW` are compile-time constants in `session.rs`. The two ends agree on them through the session-protocol version (rule 8), not through the build id: the local bridge and the remote keeper are separate builds. The ring caps are each end's own and need no agreement. End-to-end tests shorten the grace with `HERDR_MOSH_GRACE=5` (the clamp minimum).

**Spawn.** The attacher spawns the keeper when no live keeper answers: `current_exe [--session S] remote-client-bridge --x-mosh-keeper-v1`, via `detach_server_daemon_command` (setsid) plus a plain `spawn` (not the launchd path; `launchd_spawn.rs:31-36`). The spawn has `env_remove("SSH_AUTH_SOCK")` and `Stdio::null()` on stdin, stdout and stderr, as upstream's `build_server_daemon_command` does (`server/autodetect.rs:225-233`). So the keeper holds none of the attacher's sshd channel fds: leg 0's sshd session closes when its attacher exits, and keeper output can never reach the leg byte stream or the terminal. The keeper writes diagnostics only to `HERDR_MOSH_TRACE`. Concurrent attachers are serialised by the spawn lock.

**Per `HELLO`.** `previous_token` is applied first. If it is present, differs from `token` and names a session, that session is ended (server stream closed, removed). A `v` that differs from the keeper's is answered `END{"protocol-version"}`. Then the token's state decides the answer. The states are: *absent* (never existed, or ended and removed: no tombstones), *live* (a leg is attached), *parked* (no leg, server stream open, grace running), and *eof* (server EOF reached, remaining bytes and `END` queued, live or parked).

| HELLO | absent | live | parked | eof |
|---|---|---|---|---|
| `resume:false` | Connect to `client_socket_path()` once. On success: create the session, answer `WELCOME new`. On failure: answer `END{"server-connect-failed"}` and create nothing | `END{"token-in-use"}`; the session is untouched | as live | as live |
| `resume:true` | `WELCOME unknown`; never opens a server connection | Steal the leg (close the previous one), `WELCOME resumed{recv_offset}`, replay | Cancel grace, `WELCOME resumed{recv_offset}`, replay | Cancel grace, `WELCOME resumed{recv_offset}`, replay the remaining bytes, then `END{"server-eof"}` |

The keeper closes the leg after any `END` answer. `token-in-use` needs a 128-bit token collision or a buggy peer; it exists only so that every cell is defined.

**Grace.** A per-session timer starts when its leg drops (leg EOF or the keeper's 8 s silence verdict). It is cancelled on resume. On expiry it closes the server stream and removes the session.

**Server EOF** (live handoff, server restart). On server EOF the session enters *eof*. The keeper forwards the remaining ring bytes, then `END{"server-eof"}`, in stream order, over the live leg. If no leg is up, they stay queued until a resume (table above) or grace expiry. The session is removed once the local end has acked the final downstream offset, or at grace expiry. The keeper does not parse frames. The local bridge delivers every complete frame (including the handoff `ServerShutdown`, `headless.rs:1716-1735`), drops any trailing partial frame, and then closes the client socket, so today's reconnect path runs. If that reconnect reaches the keeper before the new server listens, it gets `END{"server-connect-failed"}` and leg 0 takes the plain fallback. The fallback's upstream `remote-client-bridge` runs `ensure_remote_server_running` as today.

**Idle exit.** It is lock-ordered against the rendezvous. Once the table has been empty for 30 s, the keeper takes `.spawn` non-blocking; if an attacher holds it, the keeper re-arms the 30 s timer. Holding `.spawn`, it accepts any connection already in the listen backlog (non-blocking). If there is one, or the table is no longer empty, it releases `.spawn`, serves the connection and re-arms. Otherwise it closes the listener, unlinks the socket and exits, which releases `.live` and then `.spawn`. The lock closes the spawn-side window, but not every race: an attacher's first connect takes no lock, so a connect can still land while the keeper closes. That resolves through existing paths. A connect that arrives after the close fails with `ENOENT` or `ECONNREFUSED`, and the attacher's branch takes the spawn lock, re-checks the live lock and spawns a new keeper. A connect that lands in the backlog after the drain is accepted by the kernel and then meets EOF before `WELCOME`: on leg 0 that takes the plain fallback (§Discovery and fallback), and on a reconnect leg it is `Retryable` (§Legs "Failure classification" class 4).

### Outage banner (D5)

**Mount.** `client/mod.rs` gains `#[path = "../mosh/outage.rs"] pub(crate) mod mosh_outage; // mosh-fork hook`, so `src/mosh/outage.rs` is the one source of `LinkStatus` and of `LINK_KIND = "x-mosh.link.v1"`. The client-side module must define the kind, because the remote-side `mosh` tree is `pub(in crate::remote)` and not visible to `client`. The bridge side encodes with `crate::client::mosh_outage::LinkStatus`; the crate-root `client` module is private, so its `pub(crate)` items are reachable crate-wide.

**Hook 1, `client/mod.rs` `EndpointControl` arm** (top of the arm, l.~2093). A single `if crate::client::mosh_outage::observe(&endpoint_id, &kind, &data) { <request repaint as the arm's other UI-affecting branches do>; continue; } // mosh-fork hook`. It returns `true` only for `LINK_KIND`, which keeps it out of `decode_endpoint_control`.

**Hook 2, `client/shell/composition.rs`** (in `compose`, after the endpoint-status block and before `compose_graphics`, l.~719). `crate::client::mosh_outage::paint(&mut frame, &self.active_endpoint_id); // mosh-fork hook`.
- The hook passes the display name: `crate::client::mosh_outage::paint(&mut frame, &self.active_endpoint_id, self.endpoint_label(&self.active_endpoint_id))`, which is still one statement, using the existing `endpoint_label` (`client/shell/endpoints.rs:462`). It paints a single-row banner `⚠ <label> unreachable — last contact <N>s ago, resuming…` on the top row of the frame, inverse video, truncated to width.
- Painting happens only while the state for the active endpoint is `resuming`.
- `linked` clears the state. A 2 s gap with no status while `resuming` also clears it. That covers both a lost `linked` frame and endpoint removal or bridge teardown (injections stop), so no removal hook is needed.

### Upstream touch list (the complete diff outside fork files)

| File | Hook | Lines |
|---|---|---|
| `src/remote/attach.rs` | `#[path = "../mosh/mod.rs"] pub(super) mod mosh; // mosh-fork hook` after the `use` block | 1 |
| `src/remote/attach.rs` l.90 | `SshStdioBridge::start(` → `mosh::start_ui_bridge(&remote_ssh, ` (same remaining arguments) | 1 |
| `src/remote/saved.rs` l.35 | `SshStdioBridge::start(` → `super::attach::mosh::start_ui_bridge(&ssh, ` | 1 |
| `src/remote/host.rs` l.7-21 | match arm `[o, ..] \| [_, o] if o.starts_with("--x-mosh-") => false, // mosh-fork hook` (also admits `--idle-timeout-v1 --x-mosh-session-v1`) | 1 |
| `src/remote/host.rs` after l.24 | `if let Some(result) = super::attach::mosh::remote::dispatch(args) { return result; } // mosh-fork hook` | 1 |
| `src/cli/status.rs` l.298-302 | push `"x-mosh-resume-bridge-v1"` into `endpoint_capabilities` on Unix | 1 |
| `src/update.rs` l.25-26 | `STABLE_/PREVIEW_UPDATE_MANIFEST_URL` take `option_env!("HERDR_STABLE_UPDATE_MANIFEST_URL")` / `option_env!("HERDR_PREVIEW_UPDATE_MANIFEST_URL")` overrides, defaulting to today's URLs (release blocker: `auto_update` would replace a fork build) | 2 |
| `src/client/mod.rs` | `mosh_outage` mount line; `EndpointControl` arm hook | 2 |
| `src/client/shell/composition.rs` | banner paint hook | 1 |
| **Total** | | **11** |

Plus `scripts/mosh-diff-budget.sh` (new). It prints `git diff origin/master --stat` excluding fork paths, plus `git grep -n 'mosh-fork'`, and exits 1 if more than 20 changed lines are outside fork paths. It is run by hand at upstream merges.

## Decision points, by section

**Resume layer (D1).** *Chosen:* a link-level byte session through a keeper, because it meets every acceptance criterion with about 11 upstream lines and no hot-file edits. *Considered:* protocol-level acks, input log and server parking (rejected: about 220 lines, about 55 in hot files, and the ack ring only pays off once QUIC drops frames, so it moves to Phase 3b); patching `SshStdioBridge::start` in place (rejected: it lacks the `RemoteSsh`, and its internals change upstream about 9 times a month).

**Where the keeper lives.** *Chosen:* a separate detached process per (user, session, build), spawned by the first attacher. It must outlive each ssh-spawned attacher, and running it inside the server would need `headless.rs` hooks. *Considered:* the attacher holding the server socket (rejected: it dies with sshd's session); a server-integrated park record (rejected for Phase 1; optional Phase 1.5).

**Offsets vs sequence numbers.** *Chosen:* absolute byte offsets per direction, because the stream is opaque bytes and offsets make dedup trivial, with no framing dependency. *Considered:* per-herdr-frame sequence numbers (rejected: they need frame parsing on both sides of the upstream path for no benefit).

**Whole-frame hold-back and injection.** *Chosen:* the local bridge parses only the u32 length prefix. That is the minimum needed to inject safely and to never tear a frame at give-up. *Considered:* no injection (rejected: a saved machine would hit its 10 s health timeout during every resume); injecting health pongs (rejected: pongs only answer pings, and the bridge cannot see client ping timing without decoding).

**Grace (D2).** *Chosen:* 60 s by default, set on the client and carried in `HELLO`. A parked session still counts as an active client to the server (geometry claim, foreground, held keys; touch map §4.4), so long graces are opt-in. *Considered:* 5 min (rejected for that reason); a server-side env var (rejected: cannot be set from the client without `SendEnv`).

**Reconnect legs.** *Chosen:* fresh, BatchMode, ControlMaster-bypassing legs, one at a time with back-off, because a mux client through a dead master can hang indefinitely. *Considered:* reusing the master (rejected: same hang); racing mux and fresh legs (rejected as Phase 1 complexity).

**Dead-link timing.** *Chosen:* 2 s heartbeat and an 8 s silence timeout counted from the last inbound byte after `WELCOME`, a 20 s pre-`WELCOME` bound, fixed, with every timer's start event stated in rule 6. *Considered:* SRTT-adaptive detection (deferred to Phase 2/3 with real RTT data from Phase 0).

**Flow control.** *Chosen:* one symmetric rule in `session.rs` (§Ring caps and drain policy): 64 KiB `DATA` chunks, acks after delivery, a fixed 2 MiB in-flight window while a leg is up, and ring caps used only as outage buffers. 2 MiB matches OpenSSH's default session channel window (`CHAN_SES_WINDOW_DEFAULT`), so leg-up throughput and backpressure match today's plain bridge, the server's render coalescing behaves as it does on a healthy link today, and memory stays bounded at both ends. *Considered:* using the ring caps as backpressure (rejected: it queues up to 16 MiB of stale renders ahead of echo, and an 8 MiB local overflow kills healthy sessions); adaptive pacing (Phases 2–4).

**Outage banner (D5).** *Chosen:* bridge-injected status frames plus 3 client hook lines. This removes the "online but frozen" regression without client timers: frames arrive every 1 s and drive the repaint. *Considered:* deferring to Phase 5 (rejected by decision D5); a client-side timer (rejected: it needs a Timer-arm hook in a hot region).

**Late execution (D6).** *Chosen:* accept and document. Input and requests buffered during an outage are delivered exactly once on resume, even if the client already reported the request failed after `ENDPOINT_COMMAND_TIMEOUT` = 60 s (`client/endpoint_commands.rs:12`); a test pins this behaviour. *Considered:* the bridge decoding and dropping stale `EndpointRequest`s (rejected: it couples the bridge to the request frame layout).

**Discovery.** *Chosen:* a capability string in `status client --json` plus candidate re-selection inside fork code, with zero edits to upstream discovery. *Considered:* editing `prepare_discovered_remote_herdr` (rejected: a hot latch region; listed as an upstream candidate instead).

**Release blocker: self-update.** *Chosen:* build-time `option_env!` manifest URL overrides in `update.rs` (2 lines, upstreamable). *Considered:* config-only (rejected: the `[update]` knobs only disable checks; `config/model.rs:33-37`).

**Out of scope (stated):**
- Resumability of the API bridge (`remote-api-bridge`).
- Observer and direct-attach clients.
- Windows remotes (the capability is Unix-only). Windows local clients also take the plain bridge: the resume path is Unix-only on the local side too (§Discovery and fallback step 1).
- QUIC, adaptive pacing and prediction (Phases 2–4).
- Server-side parking (Phase 1.5).

## Error handling summary

Every failure resolves to one of three outcomes, never a hang:
- **(a) resume:** a transient leg failure within the grace period, including silence or EOF before `WELCOME` on a reconnect leg;
- **(b) today's disconnect path**, by closing the client socket at a frame boundary: grace expiry, ring overflow during an outage (including one client message larger than the 8 MiB local cap), `unknown` (or `new`) on a resume, a `Fatal` leg (auth, host key, or a positive remote refusal on a reconnect leg: an `END` before `WELCOME` or a remote error exit), server EOF after delivery;
- **(c) the plain bridge for that connection:** no capability, kill switch, Windows remote, non-Unix local build, or any remote-side leg-0 failure before `WELCOME` (§Discovery and fallback "The leg-0 rule": downgraded binary, `END{"keeper-unavailable"}`, `END{"server-connect-failed"}`, non-255 exit, EOF before `WELCOME` or the 20 s pre-`WELCOME` bound). An ssh-level leg-0 failure (exit 255) is reported exactly as today.

Protocol errors on a leg end the leg, not the session. A torn or oversized herdr frame ends the session.

## Testing

**Unit tests** (`#[cfg(test)]` in `src/mosh/*`)
- Session: loss at every offset, duplicate and overlapping `DATA`, gaps, double reconnect, and `END` in both directions; `DATA` over 64 KiB rejected; acks advance only on delivery; the sender stops reading at `WINDOW` while a leg is up and keeps reading to the ring cap while it is down; replay after resume respects the window; `ACK`, `HEARTBEAT` and a pre-`WELCOME` refusal `END` are never queued behind more than one chunk, while a session-final `END` is sent only after every ring byte; the silence clock starts at `WELCOME` and resets on any byte; before `WELCOME` only the 20 s bound from `HELLO` applies.
- Ring: cap overflow and replay boundaries.
- Keeper: every cell of the Per-`HELLO` table, including `resume:true` for an absent token answering `unknown` without a server connection, `server-connect-failed`, and *eof* while parked replaying the remaining bytes then `END`; idle exit against a concurrent attacher: a held spawn lock defers the exit, a connection already in the backlog at the drain is served, and an attacher connecting after the exit gets `ENOENT`/`ECONNREFUSED`, re-checks the live lock and spawns a fresh keeper (the final-instant backlog race is covered by the leg-classification test for EOF before `WELCOME`, not asserted here).
- Rendezvous: a foreign-owned, symlinked or wrong-mode private dir and a peer-uid mismatch each yield `END{"keeper-unavailable"}`; keeper names differ when `client_socket_path()` differs.
- Leg classification: exit 255 with each fatal stderr string, exit 255 otherwise, non-255 exit, a bridge-killed leg, attacher `END` before `WELCOME`, EOF before `WELCOME`, and the 20 s pre-`WELCOME` bound, on leg 0 and on a reconnect leg (EOF and the bound are `Retryable` on a reconnect leg); the precedence exit 255, then `END`, then remote error exit.
- Session end on stop: with a live leg, setting `should_stop` writes and flushes `END{"client-gone"}` within 500 ms before teardown, whether or not client EOF was seen first.
- Link frames: partial-frame hold-back, injection only between frames, and a round-trip decode of the injected frame through `crate::client::endpoint::decode_endpoint_control` yielding `Ignored`.
- Outage: `observe`/`paint` state transitions, including the 2 s stale clear.
- Leg: command construction, with `-o ControlPath=none -o ControlMaster=no` and the noninteractive options on every reconnect leg, with and without managed config; the keeper spawn command has null stdio.

These are in-process tests that can reach private `attach` items through the `#[path]` mount.

**End-to-end tests** (`tests/mosh_resume.rs`, `tests/mosh_multi_client.rs`). They drive the `herdr` binary with a fake `ssh` on `PATH` (pattern: `tests/remote_attach.rs:69-107`; saved machines as in `tests/machine_setup.rs`), where the fake ssh runs the remote command locally.

**Observability.** With `HERDR_MOSH_TRACE=<dir>` set, the local bridge and the keeper append one JSON line per session event to `<dir>/local.jsonl` and `<dir>/keeper.jsonl`. Events: `leg_up`, `leg_down{reason}`, `welcome{status}`, `resume{replayed_up, replayed_down}`, `server_connect`, `session_end{reason}`, `inject{state}`. With the variable unset, nothing is written. "No snapshot or full surface after resume" is asserted structurally: the trace shows `welcome resumed` and exactly one `server_connect` for the token, so the server never saw a new client connection and sent no reconnect snapshot. Tests also observe pane content through the existing `tests/support` screen helpers.

## Acceptance criteria

- [ ] With a resume-capable remote, killing the leg ssh process mid-stream, including in the middle of a downstream frame, resumes the session. Keys typed before, during and after the outage all reach the pane exactly once, in order. After resume the client receives no `ClientShellSnapshot` and no full `PaneSurface` attributable to the reconnect.
- [ ] A leg stalled with SIGSTOP for more than 8 s is detected as dead, and a fresh leg resumes the session; this stands in for netem loss in tests.
- [ ] A saved-machine outage longer than 10 s (up to the grace) does not trigger the endpoint health timeout or a supervisor reconnect, including when the leg stalls immediately after a client health ping (the about 2 s margin in §Local bridge behaviour "Health margin").
- [ ] During an outage the active endpoint shows the outage banner with a last-contact time that updates every second. The banner clears within one frame of resume.
- [ ] After the grace period expires with no reachable remote, the client sees today's behaviour: `--remote` exits with the reattach hint, and saved machines take the supervisor reconnect path. No hang.
- [ ] Two simultaneous clients (two local client processes, each with its own bridge and token): killing one's leg leaves the other unaffected, and the killed one resumes.
- [ ] Client exit ends its keeper session within 2 s when the leg is alive, whether the bridge sees client EOF or `should_stop` first. A full reconnect through the same local socket path evicts the previous token. That covers a saved machine's supervisor reconnect, which builds a new bridge, and a second client connection on one `--remote` bridge. The keeper holds at most one session per local socket path after reconnect.
- [ ] A live handoff during a session delivers `ServerShutdown` to the client (not `ConnectionLost`), and today's reconnect runs. This also holds when the server EOF arrives while the leg is down and the session then resumes.
- [ ] A request buffered past `ENDPOINT_COMMAND_TIMEOUT` during an outage executes exactly once on resume (late execution, D6). A test pins this.
- [ ] (Windows remote covered by an in-process `start_ui_bridge` platform-gate unit test.) Against a remote binary without `x-mosh-resume-bridge-v1`, with `HERDR_MOSH_RESUME=0`, or against a Windows remote, behaviour is byte-identical to upstream (the plain `SshStdioBridge`). The existing `tests/remote_attach.rs` suite passes unchanged.
- [ ] Reconnect legs never use the ControlMaster, even when the user's own ssh config sets `ControlMaster auto`/`ControlPath`, and always run BatchMode. Auth and host-key failures stop retrying immediately and fall to today's path.
- [ ] On leg 0, a keeper bring-up failure (unwritable or foreign-owned rendezvous dir, spawn failure, 5 s wait), a downgraded remote, or a keeper `server-connect-failed` serves that connection through the plain bridge, for `--remote` as well as for saved machines; the client then sees upstream behaviour for that connection. The fallback's second ssh connection (a second prompt without a ControlMaster) after up to the 20 s pre-`WELCOME` bound is the accepted cost (§Discovery and fallback "The leg-0 rule").
- [ ] On a reconnect leg, a stall or EOF between `HELLO` and `WELCOME` is retried within the give-up deadline and the session resumes; only an `END` before `WELCOME` or a remote error exit ends it.
- [ ] A resume `HELLO` reaching a keeper that no longer has the token (keeper restarted, grace expired) is answered `unknown`, never `new`, and opens no server connection.
- [ ] On a healthy but slow live leg, unacked bytes in flight never exceed 2 MiB in either direction. A 16 MiB clipboard image sent while the leg is up completes without ending the session under the same RTT and bandwidth conditions under which the plain bridge completes it.
- [ ] `git diff origin/master` outside `src/mosh/`, `tests/mosh_*`, `scripts/mosh-*`, `HERDR_MOSH_VISION.md` and `docs/superpowers/` touches only the files in the upstream touch list, with about 11 lines, each tagged `// mosh-fork hook`. `scripts/mosh-diff-budget.sh` passes. `Cargo.toml` and `Cargo.lock` are unchanged.
- [ ] Builds with `HERDR_STABLE_UPDATE_MANIFEST_URL`/`HERDR_PREVIEW_UPDATE_MANIFEST_URL` set use those manifest URLs; builds without them are unchanged.
- [ ] `cargo clippy` (repo `clippy.toml`) and the full test suite pass.

## Key abstractions

**SessionMsg** — new closed enum (`session.rs`), the six session wire messages (HELLO, WELCOME, DATA, ACK, HEARTBEAT, END) with their `[u8 type][u32 LE len][payload]` codec and incremental decoder.
- Owns: encode/decode, the JSON bodies of HELLO/WELCOME/END, malformed/unknown input → leg protocol error.
- Hides: message byte layout.
- Must not: do I/O or apply offset/ack rules (`SessionEndpoint` does). Never grows a per-transport variant (Phase 3 keeps it as is).
- Relationships: used by `SessionEndpoint`, `LocalBridge`, `Keeper`; depends on nothing fork-side.
- Errors: decode error always means "end this leg", never "end this session".

**SessionEndpoint** — new struct (`session.rs`), one end of a resumable byte session, sans-IO, used symmetrically by the local end (per client connection) and the keeper end (per token).
- Owns: the session `Token` (128-bit newtype from `/dev/urandom`); outbound offset; inbound `recv_offset` and dedup (rule 2: overlap discarded, gap = error); the one flow-control rule (§Ring caps and drain policy): `DATA` chunking at 64 KiB, ack-after-delivery scheduling (the owner reports delivered offsets; 64 KiB / 200 ms / every heartbeat), the 2 MiB leg-up in-flight window and the `may_read_source()` answer (below `WINDOW` while a leg is up, below the ring cap while down), window-limited replay; send ordering (`ACK`, `HEARTBEAT` and a pre-`WELCOME` refusal `END` before the next chunk; a session-final `END` after the ring is sent); heartbeat timing, the 8 s silence verdict (clock from `WELCOME`, reset on any inbound byte), the 20 s pre-`WELCOME` bound (from `HELLO`) and the reconnect-leg setup deadline; HELLO/WELCOME negotiation (`resume` flag, `v` check) and the new/resumed/unknown meaning; replay from the peer's offset on resume. Holds exactly one outbound `ReplayRing`, told its capacity at construction (8 MiB local, 16 MiB keeper).
- Hides: offset arithmetic and ack/heartbeat rules, so neither end re-implements them.
- Must not: touch sockets, threads or the clock directly (time is an input); decide what overflow means (reports it; owner ends the session).
- Relationships: composes `ReplayRing`, uses `SessionMsg`. Two users — `LocalBridge`'s connection run and the keeper's per-token session — the duplication that justifies sharing it.
- Lifecycle: created at HELLO time (local: per accepted client connection; keeper: per fresh token); lives for the session across any number of legs; single owner, not shared across threads.
- Errors: callers distinguish leg protocol error (drop leg, redial), ring overflow during an outage (end session), peer END (end session, with reason), resume refused (`unknown`, or `new` on a reconnect leg).

**ReplayRing** — new struct (`ring.rs`), bounded byte ring keyed by absolute offset.
- Owns: push, ack-trim, replay-from-offset, occupancy query (`SessionEndpoint` compares it with `WINDOW` and the cap).
- Hides: storage and wraparound.
- Must not: decide overflow policy or act as flow control. It reports overflow; overflow can only happen during an outage and always ends the session (spec §Ring caps and drain policy).
- Lifecycle: owned by one `SessionEndpoint`.
- Errors: overflow, reported, never silent.

**FrameSplitter** — new struct (`link_frames.rs`), incremental u32-LE length-prefix splitter, plus a free function encoding an injected `x-mosh.link.v1` `EndpointControl` frame via `crate::protocol`'s frame writer.
- Owns: partial-frame hold-back, "at a frame boundary" knowledge, rejecting frames over `MAX_GRAPHICS_FRAME_SIZE`.
- Hides: herdr framing; nothing else in the fork parses length prefixes.
- Must not: interpret frame contents or status semantics.
- Relationships: used by `LocalBridge` only (whole-frame delivery, inject-between-frames, dropping a trailing partial frame at server EOF). The keeper never parses frames.
- Errors: oversized/torn frame → session end (spec §Error handling).

**Leg** — new struct (`ssh_leg.rs`, transport-neutral), one live connection to the keeper side.
- Owns: a duplex byte stream, a teardown handle, a structured end report (`ExitStatus`, captured stderr on reconnect legs, whether `WELCOME` was reached, any pre-`WELCOME` `END` reason). For ssh it owns the ssh child directly. Its stream is the child's stdin/stdout after upstream's `discard_remote_output_preamble`; there is no socketpair and no `bridge_child` (§Legs "Leg process ownership").
- Hides: which process or transport carries the bytes.
- Must not: know session semantics.
- Lifecycle: produced by a `LegDialer`, owned by the connection run; teardown kills the child and joins the pump on silence, protocol error or session end.

**LegDialer** — new trait, "produce the next leg". One implementation now (`SshLegDialer`); the spec names the next one: the Phase 3a QUIC leg with ssh fallback (`HERDR_MOSH_VISION.md` §4.2).
- Owns (`SshLegDialer`): per-leg remote command via upstream builders; leg-0 vs reconnect options (reconnect: `apply_noninteractive_ssh_options`, piped stderr, `control_path: None` and always `-o ControlPath=none -o ControlMaster=no`); no locale override (no `LC_ALL`); back-off schedule (1, 2, 4, 8, 8… s); failure classification from the leg's end report (§Legs "Failure classification": exit 255 first, then a pre-`WELCOME` `END`, then a remote error exit, else EOF/bound).
- Hides: ssh arguments and ControlMaster bypass.
- Must not: touch session bytes or the give-up deadline (caller passes the deadline and stop flag and stops dialing when either fires).
- Relationships: `LocalBridge` holds one as a boxed trait object; dependencies point `local_bridge` → trait, `ssh_leg` → `attach` internals.
- Lifecycle: created by `start_ui_bridge`, owned by the `LocalBridge` for the bridge's lifetime.
- Errors: `Retryable` (including silence or EOF before `WELCOME` on a reconnect leg, within the give-up deadline), `Fatal` (on a reconnect leg: auth or host key at exit 255, or a positive remote refusal: an `END` before `WELCOME` or a non-zero, non-255 exit the bridge did not cause), `Unsupported` (any remote-side failure before `WELCOME` on leg 0 only → plain `bridge_connection` for that connection). An ssh-level leg-0 failure is reported as today. Kept distinct.

**LocalBridge** — new struct (`local_bridge.rs`), the resume-mode body of the thread inside an `SshStdioBridge`.
- Owns: the user-facing listener; serial connection serving; the process-global last-token map keyed by local socket path, for ghost eviction; the pre-`WELCOME` gate (no client read before leg 0's `WELCOME new`) and the leg-0 plain fallback hand-off; the per-connection run: a leg I/O thread (session pump, timers, `should_stop` polling ≤100 ms) and a client-writer thread (all blocking client-socket writes), resuming/linked state, `LinkStatus` injection (first one at the dead verdict, then every 1 s; only after the first frame, only between frames, never past the deadline), the give-up deadline, session-end triggers.
- Hides: that an outage is happening at all; the client sees whole frames plus status frames.
- Must not: build ssh commands (`LegDialer`), parse frames beyond `FrameSplitter`, or block its leg I/O thread on the network or on the client socket.
- Lifecycle: built by `start_ui_bridge`; runs on the bridge thread until `should_stop` or listener failure; released by upstream `SshStdioBridge::Drop` (stop flag, socket removal, join).
- Errors: fatal leg class → `failure_tx`, so `reported_failure()` keeps its meaning; every other session end closes the client socket at a frame boundary (outcome (b)).

**`start_ui_bridge` (mosh/mod.rs)** — new module function, the single plain-vs-resume choice.
- Owns: reading env config once (kill switch; grace clamped 5..=3600 s), the Windows-remote and non-Unix-local checks, capability probe and candidate re-selection (implemented in `ssh_leg`), choosing the `LegDialer`, returning the `SshStdioBridge`.
- Must not: be called from anywhere except the two swap points.

**Keeper** — new struct (`remote/keeper.rs`), the detached process-lifetime daemon.
- Owns: listener; token table (per token: server `LocalStream`, a `SessionEndpoint` with one 16 MiB outbound ring, state absent/live/parked/eof, current leg, grace deadline); the Per-`HELLO` table (§Keeper); leg stealing; `previous_token` eviction; a server-writer thread per session (acks after server writes); reading the server only when `SessionEndpoint` allows it (§Ring caps and drain policy); server EOF then `END{"server-eof"}`, kept for replay while parked; lock-ordered idle exit after 30 s.
- Hides: the parked server connection from both peers.
- Must not: parse herdr frames at all. Its leg handler takes a plain duplex stream so a Phase 3 UDP acceptor can feed the same handler.
- Lifecycle: spawned by the attacher with null stdio; concurrent spawns and idle exit are serialised by the rendezvous lock files (spec §Keeper "Rendezvous"). The table is shared by each session's server-drain thread and the leg threads behind one lock; releasing a session closes its server stream.
- Errors: answers `unknown`, `END{"server-connect-failed"}`, `END{"protocol-version"}` or `END{"token-in-use"}` per the table; ends a session on ring overflow or grace expiry; drops the leg on leg protocol error.

**Attacher** — new function (`remote/attacher.rs`), a dumb pump.
- Owns: validating the private rendezvous dir, locating or spawning the keeper (null stdio), connecting to it and checking the peer uid; on any bring-up failure, writing `END{"keeper-unavailable"}` and exiting non-zero; otherwise it reuses `crate::platform::forward_remote_bridge_stdio(keeper_stream, false)` unchanged — not a new pump.

**`remote::dispatch`** — new closed `match` over the two `--x-mosh-*` flags. Fixed kinds, no trait.

**LinkStatus** — new serde enum (`src/mosh/outage.rs`, mounted as `client::mosh_outage`), `Resuming{last_contact_ms}` / `Linked`, tagged by `state`. The one definition of the payload: the bridge encodes it, the client decodes it.

**OutageState** — new process-global per-endpoint map behind `observe`/`paint`.
- Owns: transitions (clear on `linked`, on endpoint removal, after a 2 s stale gap) and painting the banner row.
- Must not: detect outages (the bridge does).

**SshStdioBridge** — extended: constructed by struct literal from the fork, its thread running `LocalBridge`; fields and Drop unchanged.

Variation: leg transport (ssh now, QUIC in Phase 3a) → the `LegDialer` trait, the kind chosen once in `start_ui_bridge`. Session message kinds and link-status states are fixed → closed enums with `match`. Plain vs resume → decided once in `start_ui_bridge`; downgrade surfaces as `LegError::Unsupported`.
Walk-through: `attach.rs:90` calls `start_ui_bridge` → capability probe → `SshLegDialer` → `LocalBridge` thread → returns `SshStdioBridge`. Client connects; local end creates a `SessionEndpoint` and, without reading the client, dials leg 0 (an owned ssh child, `--x-mosh-session-v1`); remote `dispatch` runs the attacher, which validates the private dir, connects to the keeper, checks its uid and runs `forward_remote_bridge_stdio`; keeper gets HELLO `resume:false` with a fresh token, connects to the server, answers `new`; the bridge starts reading the client. DATA/ACK flow within the 2 MiB window; acks follow delivery. Downstream passes `FrameSplitter` to the client as whole frames. ssh is killed → 8 s silence or EOF → leg torn down; `LocalBridge` injects `Resuming` at once and then every 1 s; both ends buffer up to their ring caps; `observe`/`paint` show the banner. Dialer back-off → reconnect leg N (BatchMode, no ControlMaster) → HELLO `resume:true` with the token → keeper steals the leg, answers `resumed` → both rings replay from peer offsets, window-limited → `Linked` clears the banner. Future change, Phase 3a QUIC: one new `QuicLegDialer` (wrapping `SshLegDialer` for the 3 s fallback) plus one registration in `start_ui_bridge`; keeper side, one new acceptor feeding the existing leg handler. Session, ring and splitter untouched.
Per spec §Discovery step 4: `start_ui_bridge` returns an upstream `SshStdioBridge` struct literal rather than a fork type — deliberate, so callers' types don't change.
Per spec §Outage banner: `observe`/`paint` hooks take no handle, so `OutageState` is deliberately process-global.
Per spec §Legs: the dialer deliberately branches on leg 0 vs reconnect legs for interactivity and ControlMaster use.
Per spec §Discovery "The leg-0 rule": a remote-side failure before leg 0's `WELCOME` (a downgraded remote among them) serves that connection through upstream's plain `bridge_connection` — deliberate, leg 0 only. On a reconnect leg a positive remote refusal ends the session (outcome (b)) and silence or EOF is retried; there is never a plain fallback mid-session.

Digest:
- SessionMsg: owns the session message codec; new message kinds are a variant plus a match arm; callers never hand-pack type/len bytes.
- SessionEndpoint: owns offsets, dedup, acks, flow control (window and ring-cap reading rule), link timers and resume negotiation for one session end; protocol rule changes go here; bridge and keeper never do offset arithmetic themselves.
- ReplayRing: owns offset-keyed replay storage; capacity supplied by the owner; never decides overflow policy.
- FrameSplitter: owns herdr length-prefix framing and injected-frame encoding; nothing else parses frame lengths.
- Leg: owns one live transport connection and its teardown; never interprets bytes.
- LegDialer: owns how the next leg is made, back-off and failure classification; a new transport is a new impl registered in `start_ui_bridge`; callers never build ssh commands.
- LocalBridge: owns the user-facing socket, per-connection pump, injection, deadline and end triggers; new outage behaviour goes here; never touches ssh arguments.
- start_ui_bridge: the single choice of plain vs resume and of dialer kind; callers never branch on mosh support.
- Keeper: owns parked server connections, token table and Per-`HELLO` state table, grace and idle exit; new leg sources are acceptors feeding its one leg handler; never parses frames.
- Attacher: owns the rendezvous checks and locating/spawning the keeper; pumps via existing `forward_remote_bridge_stdio`; never interprets session bytes and originates only `END{"keeper-unavailable"}`.
- remote::dispatch: owns `--x-mosh-*` flag routing; a new flag is a match arm.
- LinkStatus: the one definition of the `x-mosh.link.v1` payload; new states are variants; never redefined bridge-side.
- OutageState: owns per-endpoint banner state and transitions; new display rules go in `observe`/`paint`; never detects outages.

## Unchanged behavior

Behavior the redesign-cycle-1 amendment (flow control, rendezvous, leg-0 fallback, keeper HELLO states, leg timing, and the independent fixes) must not change:
- The Goal, the non-goals (API bridge, observer/direct-attach clients, Windows remotes, QUIC/adaptive pacing/prediction, server-side parking) and the D1 choice of a link-level byte session through a keeper.
- The upstream footprint stays the 11 hook lines in §Upstream touch list. No new hook, no `Cargo.toml`/`Cargo.lock` change, and no herdr wire-enum, protocol-version or hello-field change. Every new mechanism lives in `src/mosh/`.
- Against a remote without `x-mosh-resume-bridge-v1`, with `HERDR_MOSH_RESUME=0`, or against a Windows remote, behaviour stays byte-identical to the plain `SshStdioBridge`, and `tests/remote_attach.rs` passes unchanged.
- Leg 0 uses exactly today's ssh options: for `--remote`, interactive with inherited stderr; for saved machines, noninteractive. An ssh-level leg-0 failure is reported through `failure_tx` / `reported_failure()` as today.
- Grace: default 60 s, chosen on the client, clamped 5..=3600, carried in `HELLO`. Heartbeat 2 s, silence 8 s, back-off 1, 2, 4, 8, 8… s, one leg at a time, reconnect legs BatchMode with piped stderr.
- Absolute byte offsets per direction, overlap dedup and gap = leg error, replay from the peer's `recv_offset`, exactly-once delivery in both directions.
- Ring caps stay 8 MiB local and 16 MiB keeper, and ring overflow during an outage ends the session (outcome (b)).
- Whole-frame hold-back. Injected `EndpointControl{kind:"x-mosh.link.v1"}` frames with the `LinkStatus` `resuming`/`linked` payloads, every 1 s while resuming, only after the first frame, only between frames, never past the give-up deadline. The three banner hook lines and `OutageState` behaviour.
- The keeper is one detached process per (user, session, build) that holds the server socket across legs. It is spawned via `detach_server_daemon_command` plus a plain spawn (never launchd), without `SSH_AUTH_SOCK`, with `.live`/`.spawn` flock serialisation and a 30 s idle exit.
- Leg stealing on resume, `previous_token` ghost eviction, and `END{"server-eof"}` after the remaining bytes, so the client gets `ServerShutdown` and today's reconnect path runs.
- D6 late execution: input buffered through an outage is delivered exactly once on resume.
- `SshStdioBridge` is returned as a struct literal, with its fields, `reported_failure()` and `Drop` unchanged.

## Post-Implementation Notes

> *As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*

## Post-Implementation Note (2026-10-24, mosh-dqw.10)

The status.rs capability hook is two lines, not one: rustfmt always splits a `#[cfg(unix)]` attribute off a `vec!` element, so the attribute and the `"x-mosh-resume-bridge-v1"` entry are separate lines, both tagged `// mosh-fork hook`. The upstream touch list is therefore 12 lines, not 11.

## Post-Implementation Note (2026-10-24, mosh-dqw.23): keeper back-pressures at exactly 16 MiB instead of overflowing

§Ring caps point 4 says the keeper's ring overflows at its cap and the session ends. The keeper does not: `server_reader` in `src/mosh/remote/keeper.rs` reads the server only while `session.endpoint.may_read_source()` is true, which while the link is down is `occupancy < ring cap`. A full 16 MiB ring therefore stops reading the server (back-pressure) rather than ending the session, so the session lives, back-pressured, until grace expiry; it is bounded by grace, as the final review observed. The local side behaves as the spec says: `on_client` in `src/mosh/local_bridge.rs` keeps feeding the client reader while down, so the local upstream ring overflows and ends the session at 8 MiB. Back-pressure on the server side loses no data, so the divergence is accepted.

## Post-Implementation Note (2026-10-24, mosh-dqw.23): resume.replayed_down is null in the local trace

The local `resume` trace event carries `"replayed_down": null` (`src/mosh/local_bridge.rs`, the `resume` trace with `replayed_up` and `replayed_down`), always. The local end cannot observe how much the keeper replays downstream, only what it receives afterwards. The keeper's own `resume` trace event carries the real `replayed_down`, so the figure is available from the keeper side and nothing is lost.

## Post-Implementation Note (2026-10-24, mosh-dqw.23): reconnect-leg ssh spawn failure is classified Fatal

§Legs failure classification treats only auth and host-key failures at exit 255 on a reconnect leg as Fatal. `SshLegDialer::dial_command` in `src/mosh/ssh_leg.rs` instead maps any `spawn_leg(..)` error to `LegError::Fatal` on every leg kind. A transient spawn error on a reconnect leg (for example EAGAIN) therefore ends a resumable session instead of retrying. This is a known gap, deferred in the final review and left as is; it is not a decision to keep the behaviour.

## Post-Implementation Note (2026-10-24, mosh-dqw.23): graceful --remote quit versus server hang-up final-ACK race

The acceptance criterion "Client exit ends its keeper session within 2 s" can be missed when the server hangs up as the user quits `--remote`. `final_ack` in `src/mosh/local_bridge.rs` runs after `close_client(true)` and is gated by the 200 ms ack timer, so the keeper's server-eof session can stay parked until grace expiry. The fix is tracked in mosh-dqw.19 ("retain session-final END and flush final ACK across leg loss"), which had not landed when this note was written; once it does, this race is resolved by that task and this note should be read as history.

## Post-Implementation Note (2026-10-24, code redesign cycle 1): one Closing phase per endpoint

Two fix rounds patched session teardown one call site at a time, and each fix moved the defect to a neighbouring site. The implementation now ends a session through a single Closing phase on each side, which realises §Local bridge behaviour "Session-end triggers", §Keeper "Server EOF" and rule 3 (only bytes actually written to the consumer are acked) without changing them:
- Each side enters Closing once, and that phase owns one deadline: the 500 ms stop budget of §Local bridge behaviour. Every wait in it (client-writer join, leg write completion, ssh child wait, stderr-thread join, `server_writer` drain) is bounded by that deadline, so teardown never hangs.
- Drain order: queued stream bytes to their consumer first (the client socket on the bridge; the server via `to_server` on the keeper), then `END`, then the final `ACK` at the offset the consumer has actually written. Bytes the consumer has not written when the deadline passes are not acked, so the keeper keeps those unwritten bytes until grace expiry rather than counting them delivered; this sentence concerns bytes only, never `END`. Only then are sockets shut down and the ssh child killed and reaped.
- Bridge, stop / client-gone: `END{"client-gone"}` is written to the leg independently of the client-socket drain, not after it. The leg I/O thread never blocks on the client socket, and the 500 ms budget bounds `END`'s leg write and flush. The client-writer drain and join has its own bounded sub-wait, which never delays `END`.
- Bridge, server-eof (live handoff): this Closing is not bounded by the 500 ms stop budget. Delivering the remaining complete frames, including `ServerShutdown`, to the client uses its own drain deadline, the remaining grace, before the client socket is closed (§Keeper "Server EOF"). Teardown stays bounded because acks follow delivery (rule 3).
- Bridge: the session is closed only after the leg write of `END`/`ACK` has completed and the leg has reached EOF or the peer has closed, or the deadline has passed; reaching the ssh stdin pipe is not completion.
- Keeper: the `to_server` sender is closed and `server_writer` drains before the server stream is shut down. "END sent" is recorded in one place, on write completion. Whether a session is removed is decided by one predicate, evaluated before any per-leg state is reset.
- §Legs "Failure classification" now states that a reconnect-leg spawn error is `Retryable`; this replaces the mosh-dqw.23 note "reconnect-leg ssh spawn failure is classified Fatal", which recorded a known gap.
