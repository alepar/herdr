# Mosh Phase 1 — link-level resumable remote session (design)

Fork plan: `HERDR_MOSH_VISION.md` §4.1, §5 (Phase 1), §6. Detailed verified touch map: `inputs/herdr-mosh-touch-map.md` §2.1 (this spec's source of file:line facts). Background: `inputs/herdr-remote-report.md`, `inputs/mosh-report.md`.

## Goal

`herdr --remote <target>` and saved-machine connections keep the same TUI session through an ssh drop or a stalled link. Within a 60 s grace period they resume with no lost or duplicated input, and with no `ClientShellSnapshot` or full `PaneSurface` after resume. While the link is down, an outage banner shows how long ago the remote was last heard from. All of this lands in fork-owned code under `src/mosh/`, with about 11 hook lines in upstream files.

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
                                                        │ unix socket (0600, name includes protocol version + build id)
                                                        ▼
                                                    keeper (src/mosh/remote/keeper.rs), detached daemon:
                                                    token → { server LocalStream, upstream ring, downstream ring, grace }
                                                        │ unix socket (client_socket_path(), unchanged)
                                                        ▼
                                                    herdr server (unmodified)
```

### Components and responsibilities

| Unit (new file) | Owns | Does not own |
|---|---|---|
| `src/mosh/mod.rs` | Public fork API: `start_ui_bridge`, `pub(in crate::remote)`. Constants: `STATUS_CAPABILITY = "x-mosh-resume-bridge-v1"`, the flags `--x-mosh-session-v1` / `--x-mosh-keeper-v1`, `LINK_KIND = "x-mosh.link.v1"`. Env knobs: `HERDR_MOSH_RESUME=0` kill switch; `HERDR_MOSH_GRACE` (seconds, default 60, clamped 5..=3600) | Any I/O |
| `src/mosh/session.rs` | The session wire protocol (below), the per-direction offset and ack state machine, and hello/welcome negotiation. Pure: it operates on buffers, no sockets | Leg lifecycle, rings' storage policy |
| `src/mosh/ring.rs` | A bounded replay ring keyed by absolute byte offset: `push`, `ack(offset)`, `replay_from(offset)`. Overflow is reported, never silently dropped | What to do on overflow (the caller decides) |
| `src/mosh/link_frames.rs` | Splitting the herdr frame stream into whole frames (u32-LE length prefix, `wire.rs:1647-1715` framing; frames over `MAX_GRAPHICS_FRAME_SIZE` are a protocol error that ends the session); encoding injected `x-mosh.link.v1` frames with the existing `crate::protocol` frame writer | Status semantics |
| `src/mosh/ssh_leg.rs` | Capability probe and candidate re-selection, per-leg remote command and `Command` construction, leg spawn/kill, leg failure classification (fatal vs retryable), reconnect back-off | Session bytes |
| `src/mosh/local_bridge.rs` | The user-facing socket (accept loop), the client↔session pump, whole-frame hold-back, injection rules, the give-up deadline, session-end triggers, returning a working `SshStdioBridge` value | ssh argument details |
| `src/mosh/remote/mod.rs` | `dispatch(args) -> Option<io::Result<()>>` for the `--x-mosh-*` flags | — |
| `src/mosh/remote/attacher.rs` | Locating or spawning the keeper and pumping stdio ⇄ keeper socket | Session semantics (it is a dumb pump) |
| `src/mosh/remote/keeper.rs` | The token table, server connections, rings, drain policy, grace timers, leg stealing, ghost eviction, EOF-at-frame-boundary, idle exit | The herdr protocol beyond frame boundaries |
| `src/client/mosh_outage.rs`, mounted from `client/mod.rs` | `LinkStatus` (the JSON payload of `x-mosh.link.v1`); per-endpoint outage state; `observe(endpoint_id, kind, data) -> bool`; `paint(frame, active_endpoint)` for the banner | Detecting outages (the bridge does) |

### Session wire protocol (`session.rs`)

The protocol starts on the leg after the existing ready marker `herdr-remote-output-ready:1`, so the current preamble stripping in `bridge_child`/`start_command` still applies. Every message has the form `[u8 type][u32 LE len][payload]`.

| Type | Direction | Payload |
|---|---|---|
| `HELLO` (1) | local→keeper | JSON `{v:1, token:hex16, recv_offset:u64, grace_ms:u32, build_id:str, previous_token:hex16?}` |
| `WELCOME` (2) | keeper→local | JSON `{v:1, status:"new"\|"resumed"\|"unknown"\|"ended", recv_offset:u64}` |
| `DATA` (3) | both | `[u64 LE offset][bytes]`: the absolute offset of the first byte in that direction's stream |
| `ACK` (4) | both | `[u64 LE offset]`: all bytes below the offset have been received |
| `HEARTBEAT` (5) | both | empty |
| `END` (6) | both | JSON `{reason:str}`: the session is over and both sides discard it |

**Rules**

1. **Offsets.** They start at 0 per direction when the session is created.
2. **Receive and dedup.** A receiver delivers `DATA` bytes at or above its `recv_offset` and discards the overlap below it. A gap, where the offset is above `recv_offset`, is a protocol error that ends the leg (not the session).
3. **Acks.** An `ACK` is sent after at least 64 KiB received or 200 ms since the last ack, whichever comes first, and on every heartbeat.
4. **Resume.** On `WELCOME resumed`, each side replays from its ring starting at the peer's reported `recv_offset`.
5. **New, unknown and ended.**
   - `new` is valid only for a fresh token.
   - `unknown` or `ended` for a resume attempt means the session is gone. The local bridge closes the client socket, and today's disconnect path runs (exit with the reattach hint for `--remote`, supervisor reconnect for saved machines).
6. **Liveness.** `HEARTBEAT` is sent every 2 s. A leg is dead after 8 s with no inbound message.
7. **Tokens.** 128 bits read from `/dev/urandom`. They are not a security boundary: the 0600 sockets and the ssh channel are.

### Ring caps and drain policy

**Local upstream ring** (client→server bytes not yet acked): 8 MiB.
- During an outage the local bridge keeps reading client writes into the ring, so the client writer's 5 s write deadline (`writer.rs:15`) never fires.
- On overflow the session ends: the bridge sends `END` if possible and closes the client socket, and today's path runs.

**Keeper downstream ring** (server→client bytes not yet acked): 16 MiB.
- *Leg up:* the keeper stops reading the server while the ring is full. That is today's backpressure.
- *Leg down:* it keeps draining the server into the ring, because the server's control lane has no cap (`client_transport.rs:310-318`). On overflow the session ends: the keeper closes its server stream, and the local bridge learns `ended` on its next resume attempt.

### Local bridge behaviour

**Whole frames.** Downstream bytes are passed to the client only as complete herdr frames. A partial frame is held back until it completes.

**Injection.** While a resume is in progress, the bridge writes injected frames into the client socket. Each one is a `ServerMessage::EndpointControl { kind: "x-mosh.link.v1", data: <LinkStatus JSON> }` encoded with the existing frame writer. Rules:
- only after the first complete downstream frame (the handshake `Welcome`) has been delivered;
- only between whole frames;
- every 1 s while resuming;
- never after the give-up deadline.

**Status payloads.**
- During an outage: `{"state":"resuming","last_contact_ms":N}`.
- After a successful resume: one `{"state":"linked"}`.
- The client counts every injected frame as liveness (`client/mod.rs:1457-1458`). It decodes unknown kinds as `Ignored` (`client/endpoint/control.rs:61-66`), unless the banner hook consumes them first.

**Give-up deadline.** It equals the grace period from the moment of link loss. At the deadline the bridge closes the client socket.

**Session-end triggers.**
- Client EOF: the bridge sends `END` over the live leg, or over the next leg when none is live; this is best effort.
- `SshStdioBridge` drop or stop: the bridge polls `should_stop` every ≤100 ms and never blocks on the network.
- Grace expiry.
- Ring overflow.

**Ghost eviction.** Each local bridge instance (one per `run_remote` or saved profile) remembers its last session token. A new client connection's `HELLO` carries it as `previous_token`, and the keeper ends that session immediately.

**Concurrency.** Connections are served one at a time, as upstream does (`attach.rs:3056-3068`).

### Legs (`ssh_leg.rs`)

**Leg 0** (the first connection) uses exactly the ssh options today's path uses: for `--remote`, interactive with an inherited stderr, so password and host-key prompts still work; for saved machines, noninteractive.

**Reconnect legs**
- Always noninteractive: BatchMode, piped stderr, so prompts never land on the raw-mode TUI.
- Always bypass the ControlMaster:
  - With managed config: a clone of `ManagedSshOptions` with `control_path: None` (`attach.rs:725-731`, `1421-1440`).
  - With `manage_ssh_config=false`: a `Command` with `-o ControlPath=none`, run through `bridge_child` (`attach.rs:3287`).
- One leg at a time. Back-off 1, 2, 4, 8, 8… seconds until the give-up deadline.

**Remote command per leg.** `posix_remote_output_command("exec " + exe.command(session_args(.., ["remote-client-bridge", "--x-mosh-session-v1"])))`, reusing upstream builders (`attach.rs:367-394`, `417-436`).

**Fatal classes** stop retrying at once and close the client socket:
- ssh authentication failure;
- host-key verification failure;
- the remote generation mismatch from `ensure_remote_server_running` (`host.rs:40-61`).

They are detected from `reported_failure()` plus the ssh exit status and stderr.

### Discovery and fallback

`start_ui_bridge(ssh, remote_herdr, local_socket, session, options, noninteractive, stop) -> io::Result<SshStdioBridge>`:
1. With `HERDR_MOSH_RESUME=0`, a Windows remote, or no capable candidate: call `SshStdioBridge::start` unchanged.
2. Otherwise probe `remote_client_status` once for `STATUS_CAPABILITY`.
3. If the selected binary lacks it, walk `remote_binary_candidates` for one that has both the capability and `supports_endpoint_requirement` (`attach.rs:1519-1540`, `1588-1593`). If none qualifies, use the plain bridge. For `--remote`, print one hint line naming `HERDR_REMOTE_BINARY`.
4. Return an `SshStdioBridge` struct literal that owns the local-bridge thread, its `should_stop` and a `failure_tx`, so `reported_failure()` and `Drop` keep their semantics (`attach.rs:2996-3002`, `3101-3120`).

If a remote binary is downgraded after the probe, it rejects `--x-mosh-session-v1` with "unsupported remote client bridge option". The leg then falls back to the plain bridge once for that session.

### Keeper (`remote/keeper.rs`)

**Spawn.** The attacher spawns the keeper when its socket is absent: `current_exe [--session S] remote-client-bridge --x-mosh-keeper-v1`, via `detach_server_daemon_command` plus a plain `spawn` (not the launchd path; `launchd_spawn.rs:31-36`), with `env_remove("SSH_AUTH_SOCK")`. A spawn race between two attachers is resolved by bind failure: the loser connects to the winner.

**Socket.** `crate::platform::remote_bridge_endpoint_path(...)` with a short hashed name over (uid, session, session-protocol version, build id), mode 0600. Version skew therefore never pairs incompatible peers.

**Per `HELLO`:**
- A fresh token creates a session, connects to `client_socket_path()` and answers `new`.
- A known token steals the leg (closing any previous leg) and answers `resumed` with its `recv_offset`.
- An unknown token is answered `unknown`.
- `previous_token` is evicted.

**Grace.** A per-session timer starts when its leg drops. It is cancelled on resume, and on expiry it closes the server stream and drops the session.

**Server EOF** (live handoff, server restart). The keeper keeps draining until EOF, then forwards the remaining bytes followed by `END{"server-eof"}`. The local bridge delivers every complete frame (including the handoff `ServerShutdown`, `headless.rs:1716-1735`) and then closes the client socket, so today's reconnect path runs.

**Idle exit.** The keeper exits 30 s after its table becomes empty.

### Outage banner (D5)

**Mount.** `client/mod.rs` gains `#[path = "../mosh/outage.rs"] pub(crate) mod mosh_outage; // mosh-fork hook`, so `src/mosh/outage.rs` is the one source of `LinkStatus`. The bridge side encodes with `crate::client::mosh_outage::LinkStatus`; the crate-root `client` module is private, so its `pub(crate)` items are reachable crate-wide.

**Hook 1, `client/mod.rs` `EndpointControl` arm** (top of the arm, l.~2093). A single `if crate::client::mosh_outage::observe(&endpoint_id, &kind, &data) { <request repaint as the arm's other UI-affecting branches do>; continue; } // mosh-fork hook`. It returns `true` only for `LINK_KIND`, which keeps it out of `decode_endpoint_control`.

**Hook 2, `client/shell/composition.rs`** (in `compose`, after the endpoint-status block and before `compose_graphics`, l.~719). `crate::client::mosh_outage::paint(&mut frame, &self.active_endpoint_id); // mosh-fork hook`.
- It paints a single-row banner `⚠ <remote> unreachable — last contact <N>s ago, resuming…` on the top row of the frame, inverse video, truncated to width.
- Painting happens only while the state for the active endpoint is `resuming`.
- `linked` clears the state. Endpoint removal or a 2 s gap with no status while `resuming` also clears it, so a lost `linked` frame never strands the banner.

### Upstream touch list (the complete diff outside fork files)

| File | Hook | Lines |
|---|---|---|
| `src/remote/attach.rs` | `#[path = "../mosh/mod.rs"] pub(super) mod mosh; // mosh-fork hook` after the `use` block | 1 |
| `src/remote/attach.rs` l.90 | `SshStdioBridge::start(` → `mosh::start_ui_bridge(&remote_ssh, ` (same remaining arguments) | 1 |
| `src/remote/saved.rs` l.35 | `SshStdioBridge::start(` → `super::attach::mosh::start_ui_bridge(&ssh, ` | 1 |
| `src/remote/host.rs` l.7-21 | match arm `[o, ..] if o.starts_with("--x-mosh-") => false, // mosh-fork hook` | 1 |
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

**Dead-link timing.** *Chosen:* 2 s heartbeat and an 8 s silence timeout, fixed. *Considered:* SRTT-adaptive detection (deferred to Phase 2/3 with real RTT data from Phase 0).

**Outage banner (D5).** *Chosen:* bridge-injected status frames plus 3 client hook lines. This removes the "online but frozen" regression without client timers: frames arrive every 1 s and drive the repaint. *Considered:* deferring to Phase 5 (rejected by decision D5); a client-side timer (rejected: it needs a Timer-arm hook in a hot region).

**Late execution (D6).** *Chosen:* accept and document. Input and requests buffered during an outage are delivered exactly once on resume, even if the client already reported the request failed after `ENDPOINT_COMMAND_TIMEOUT` = 60 s (`client/endpoint_commands.rs:12`); a test pins this behaviour. *Considered:* the bridge decoding and dropping stale `EndpointRequest`s (rejected: it couples the bridge to the request frame layout).

**Discovery.** *Chosen:* a capability string in `status client --json` plus candidate re-selection inside fork code, with zero edits to upstream discovery. *Considered:* editing `prepare_discovered_remote_herdr` (rejected: a hot latch region; listed as an upstream candidate instead).

**Release blocker: self-update.** *Chosen:* build-time `option_env!` manifest URL overrides in `update.rs` (2 lines, upstreamable). *Considered:* config-only (rejected: the `[update]` knobs only disable checks; `config/model.rs:33-37`).

**Out of scope (stated):**
- Resumability of the API bridge (`remote-api-bridge`).
- Observer and direct-attach clients.
- Windows remotes (the capability is Unix-only; Windows local clients may still resume against Unix remotes through `crate::ipc`).
- QUIC, adaptive pacing and prediction (Phases 2–4).
- Server-side parking (Phase 1.5).

## Error handling summary

Every failure resolves to one of three outcomes, never a hang:
- **(a) resume:** a transient leg failure within the grace period;
- **(b) today's disconnect path**, by closing the client socket at a frame boundary: grace expiry, ring overflow, `unknown`/`ended`, a fatal leg class, server EOF after delivery;
- **(c) the plain bridge from the start:** no capability, kill switch, Windows remote.

Protocol errors on a leg end the leg, not the session. A torn or oversized herdr frame ends the session.

## Testing

**Unit tests** (`#[cfg(test)]` in `src/mosh/*`)
- Session: loss at every offset, duplicate and overlapping `DATA`, gaps, double reconnect, and `END` in both directions.
- Ring: cap overflow and replay boundaries.
- Link frames: partial-frame hold-back, injection only between frames, and a round-trip decode of the injected frame through `crate::client::endpoint::decode_endpoint_control` yielding `Ignored`.
- Outage: `observe`/`paint` state transitions, including the 2 s stale clear.
- Leg: command construction, with `ControlPath` bypassed on reconnect legs.

These are in-process tests that can reach private `attach` items through the `#[path]` mount.

**End-to-end tests** (`tests/mosh_resume.rs`, `tests/mosh_multi_client.rs`). They drive the `herdr` binary with a fake `ssh` on `PATH` (pattern: `tests/remote_attach.rs:69-107`), where the fake ssh runs the remote command locally.

## Acceptance criteria

- [ ] With a resume-capable remote, killing the leg ssh process mid-stream, including in the middle of a downstream frame, resumes the session. Keys typed before, during and after the outage all reach the pane exactly once, in order. After resume the client receives no `ClientShellSnapshot` and no full `PaneSurface` attributable to the reconnect.
- [ ] A leg stalled with SIGSTOP for more than 8 s is detected as dead, and a fresh leg resumes the session; this stands in for netem loss in tests.
- [ ] A saved-machine outage longer than 10 s (up to the grace) does not trigger the endpoint health timeout or a supervisor reconnect.
- [ ] During an outage the active endpoint shows the outage banner with a last-contact time that updates every second. The banner clears within one frame of resume.
- [ ] After the grace period expires with no reachable remote, the client sees today's behaviour: `--remote` exits with the reattach hint, and saved machines take the supervisor reconnect path. No hang.
- [ ] Two simultaneous clients (two local client processes, each with its own bridge and token): killing one's leg leaves the other unaffected, and the killed one resumes.
- [ ] Client exit ends its keeper session within 2 s when the leg is alive. A full reconnect from the same bridge evicts the previous token (the keeper holds at most one session per bridge slot after reconnect).
- [ ] A live handoff during a session delivers `ServerShutdown` to the client (not `ConnectionLost`), and today's reconnect runs.
- [ ] A request buffered past `ENDPOINT_COMMAND_TIMEOUT` during an outage executes exactly once on resume (late execution, D6). A test pins this.
- [ ] Against a remote binary without `x-mosh-resume-bridge-v1`, with `HERDR_MOSH_RESUME=0`, or against a Windows remote, behaviour is byte-identical to upstream (the plain `SshStdioBridge`). The existing `tests/remote_attach.rs` suite passes unchanged.
- [ ] Reconnect legs never use the ControlMaster and always run BatchMode. Auth and host-key failures stop retrying immediately and fall to today's path.
- [ ] `git diff origin/master` outside `src/mosh/`, `tests/mosh_*`, `scripts/mosh-*`, `HERDR_MOSH_VISION.md` and `docs/superpowers/` touches only the files in the upstream touch list, with about 11 lines, each tagged `// mosh-fork hook`. `scripts/mosh-diff-budget.sh` passes. `Cargo.toml` and `Cargo.lock` are unchanged.
- [ ] Builds with `HERDR_STABLE_UPDATE_MANIFEST_URL`/`HERDR_PREVIEW_UPDATE_MANIFEST_URL` set use those manifest URLs; builds without them are unchanged.
- [ ] `cargo clippy` (repo `clippy.toml`) and the full test suite pass.

## Post-Implementation Notes

> *As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
