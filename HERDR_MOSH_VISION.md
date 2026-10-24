# HERDR_MOSH_VISION

Mosh-like remote sessions for herdr: survive network drops, IP changes and laptop sleep without losing a keystroke, and make typing over high-latency links feel local.

This is a fork-only planning document for `github.com/alepar/herdr` (remote `fork`), which tracks `herdrdev/herdr` (remote `origin`). Citations are `path:line` at upstream `5608de06` and will drift.

---

## 1. Goals and non-goals

**Goals**

- Sessions survive an ssh drop, a network switch (Wi-Fi to tethering), NAT rebinding and suspend/resume.
- No keystroke is lost or duplicated across an outage.
- Resuming costs one round trip and one diff, not a re-bootstrap plus a full snapshot.
- Supports multiple simultaneous clients per server session, as herdr already does.
- Low-latency feel on links with RTT above 60 ms, from predictive local echo.
- The fork stays cheap to rebase on a very active upstream (about 1600 commits in 6 months).

**Non-goals (for now)**

- Port, agent or X11 forwarding.
- A UDP transport for Windows remotes.
- Surviving a server restart or live handoff while detached. Today's reconnect path handles those.
- Replacing ssh for authentication. ssh stays the bootstrap, as in mosh.

## 2. Where herdr is today

Verified against the code; full report available on request.

- **Transport.** The only remote transport is the stdio of `ssh -C -T <target>`, multiplexed through an ssh ControlMaster. That gives one TCP connection per machine and a single framed stream (u32-LE length plus bincode) carrying every pane.
  - Local `SshStdioBridge`: `src/remote/attach.rs:2996-3394`.
  - Remote `remote-client-bridge`, a dumb pump: `src/remote/host.rs:6`.
- **Rendering already works the way mosh's does.** The server owns the PTYs and the libghostty-vt emulators, and renders cell grids. It sends revisioned patches and deltas (`src/server/render_stream.rs`, `src/server/headless/retained_surface.rs`) through a single-slot, latest-wins render lane (`src/server/client_transport.rs:320-330`). The client never parses VT output.
- **Gaps:**
  1. TCP head-of-line blocking.
  2. No roaming.
  3. `herdr --remote` exits on disconnect (`src/client/mod.rs:2278`).
  4. Reconnect re-runs the whole bootstrap and sends a full snapshot.
  5. The server commits its diff baseline when a frame is queued, not when the client acks it, and patches form a strict `n -> n+1` chain. One desync is fatal (`src/protocol/surface_reuse.rs:160`).
  6. Input typed during an undetected dead link is silently lost (`src/client/endpoint/writer.rs:186-256`).
  7. No prediction: every keystroke costs a full round trip.

## 3. What we take from mosh, and what we deliberately don't

| Mosh idea | herdr adoption |
|---|---|
| SSH only for bootstrap: print port, key, detach | Yes (Phase 3). The keeper prints port, cert fingerprint and token |
| Latest-state-wins screen sync, diff from the acked state | Already half there (single render slot). Full ack-based diffs land in Phase 3b, where frames can be dropped |
| Client→server input as a reliable ordered log | Yes. Phase 1 gets it at the byte level, through the replay rings |
| Roaming: accept the newest authentic source address | Yes, through QUIC connection migration (Phase 3) |
| RTT-paced frame interval | Yes (Phase 2). Keeper in-flight window first, server pacing only if measurements demand it |
| Speculative local echo with epochs and server echo ack | Yes (Phase 4), behind a flag, adaptive |
| Hand-rolled AES-OCB UDP layer | **No: QUIC (`quinn`)**. Gives migration, congestion control, PLPMTUD, streams plus cancellable frames |
| Visible-screen-only sync (no scrollback) | **No**. The herdr server already owns history |
| Key lives only in client memory (no reattach) | **No**. The keeper holds sessions by token; any client process can resume |

## 4. Architecture

### 4.1 Phase 1 (decision D1: link-level resume, "design B")

Resume lives in a fork-owned **resumable byte-stream session** between a local bridge and a remote **keeper**. The herdr server and TUI exchange exactly the same frames as today and never observe a disconnect within the grace period.

```
herdr client --local sock--> local_bridge (fork) ==session==> [leg: ssh (fresh, BatchMode on reconnect)
   -> sshd -> `herdr remote-client-bridge --x-mosh-session-v1` (attacher, one per leg)]
   --unix--> keeper (detached, per user/session/build; holds many tokens)
   --unix--> herdr server client socket
```

- **Session protocol.** Hello and welcome carry `(token, recv_offset, grace, build_id, previous_token)`. Messages: chunk, ack, heartbeat, end. Both ends keep bounded byte-offset replay rings and retransmit from the peer's offset on resume. This gives exactly-once delivery of every byte in both directions.
- **Whole frames only.** The local bridge forwards only whole herdr frames to the client, so an outage that starts mid-frame never leaves the client stream torn.
- **Keepalive injection.** While a resume is in progress, the local bridge injects `EndpointControl{kind:"x-mosh.link.v1"}` keepalives so the client's 10 s health check (`src/client/endpoint/health.rs:3-4`) does not tear the endpoint down. Rules:
  - only after `Welcome`;
  - only between whole frames;
  - never past the give-up deadline.

  Unknown kinds are ignored by the client (`src/client/endpoint/control.rs:61-66`).
- **Keeper drain policy.** While a leg is up, the keeper stops reading from the server when its ring is full (today's backpressure). While the leg is down, it keeps draining the server into the ring, because the server's control lane has no cap (`client_transport.rs:310-318`). If the ring overflows, the session ends and today's full reconnect runs.
- **Session ends** on any of:
  - client EOF;
  - bridge drop or stop;
  - grace expiry (keeper side, with a matching local give-up deadline);
  - ghost eviction: a full reconnect names the previous token and the keeper evicts it.
- **Reconnect legs** are fresh ssh connections that bypass the ControlMaster (a mux channel through a TCP-dead master can hang indefinitely). They run one at a time, with back-off of 1, 2, 4 and 8 s. A link is declared dead after 8 s of silence with data outstanding, with a 2 s heartbeat. Fatal classes stop retrying: auth failure, host-key change, generation mismatch.
- **Discovery and fallback.** The capability `x-mosh-resume-bridge-v1` is advertised in `status client --json` (`src/cli/status.rs:298`).
  - If the selected remote binary lacks it, the fork picks another candidate that has it.
  - Otherwise it falls back to the plain `SshStdioBridge`, today's behaviour.
  - Kill switch: `HERDR_MOSH_RESUME=0`.
- **Defaults.** Grace is 60 s, set on the client with `HERDR_MOSH_GRACE`. The local upstream buffer cap is at least 8 MiB, so the client writer's 5 s deadline never fires during an outage.

**Why B rather than the protocol-level design (A).** Over ssh the stream is reliable, so frames are lost only at disconnect, and B makes disconnects lossless. B meets the Phase 1 acceptance criteria with **about 8 lines in existing files and none in hot files**. A needs about 220 lines, about 55 of them in `headless.rs`, `client/mod.rs` and `client_transport.rs`. A's surface-ack and resync work is still needed, but only once QUIC starts dropping superseded frames, so it moves to Phase 3b.

**Accepted Phase 1 trade-offs.** Documented and bounded by the 60 s grace.

- A parked client is still an active client to the server:
  - it keeps its geometry claim and may stay the foreground client (keybindings, focus);
  - held keys stay held;
  - single-client "resize all tabs" stays disabled for the other client.

  The proper fix is Phase 1.5 (a server-side park record).
- Requests that time out on the client during an outage (`ENDPOINT_COMMAND_TIMEOUT` = 60 s, `ACTIVATION_TIMEOUT` = 5 s) still execute when the link returns.
- During an outage a saved machine would look online but frozen, rather than "reconnecting". Phase 1 therefore includes the outage banner (D5): `x-mosh.link.v1` carries a status payload, and there is a 1-line hook in the `client/mod.rs` `EndpointControl` arm plus a 1-line hook in `client/shell/composition.rs`.
- The API bridge (`remote-api-bridge`) is not resumable in Phase 1.

### 4.2 Later phases

- **Phase 2: adaptive pacing.**
  - The keeper's ring becomes an RTT-sized in-flight window while a leg is up. The server's single render slot then coalesces frames, which gives link-adaptive latest-wins with no server change.
  - Per-client server pacing at `clamp(SRTT/2, 16, 250)` ms is the fallback, only if Phase 0 measurements show socket-buffer staleness.
- **Phase 3a: QUIC leg.**
  - The keeper grows a UDP listener. The ssh bootstrap prints `port + cert fingerprint + token`. The local bridge's leg becomes a QUIC bidi stream, and the session layer is unchanged.
  - QUIC migration provides roaming. The client falls back to the ssh leg if the QUIC dial fails within about 3 s.
  - Dependencies: `quinn` with runtime-tokio and rustls-ring, `rustls`, `rcgen`. Use ring rather than aws-lc-rs because of the musl and windows-msvc targets.
- **Phase 3b: cancel-on-supersede.**
  - Render frames each go on their own QUIC stream, classified by peeking the bincode discriminant. A frame is cancelled when a newer one supersedes it.
  - This needs the protocol-level surface acks and resync: a server-side ring of sent surfaces with an acked base, a decoder with a retained ring, and a relaxed chain. All of it is negotiated via `EndpointControl` kinds plus a capability.
- **Phase 4: predictive local echo.**
  - A client overlay draws on the retained `pane_surface` cells, mapped through `PaneSurfacePane.inner_rect`. No client emulator is needed.
  - The server sends a per-pane echo ack as `x-mosh.echo-ack.v1` on the render lane, never in the frozen bincode surface structs.
  - Mosh's rules apply: epochs, content-only validation, show above about 60 ms SRTT, underline above about 160 ms, reset on paste or resize.
- **Phase 5: outage UX.**
  - A "last contact N s" banner, carried as a status payload on `x-mosh.link.v1`.
  - The blind typeahead Phase 1 already buffers becomes visible.

## 5. Phase plan and acceptance

| Phase | Deliverable | Acceptance |
|---|---|---|
| 0 | netem harness: 50/150/300 ms RTT, 1-20 % loss, IP change, suspend; bytes per keystroke via `render_prof` | Baseline report checked in |
| **1** | Link-level resumable session (§4.1) for `herdr --remote` and saved machines. **Ships as its own release** | Killing ssh mid-stream and mid-frame, or applying netem loss: the session resumes with zero lost or duplicated input and no `ClientShellSnapshot` or full `PaneSurface` after resume. A saved-machine outage longer than 10 s keeps health intact. Two clients, one killed: the other is unaffected and the killed one resumes. A live handoff during an outage delivers `ServerShutdown`. Fallback to the plain bridge works against an upstream remote binary |
| 1.5 (opt.) | Server-side park record (parked client is not an active client) | Parked-client side effects (§4.1) gone |
| 2 | Keeper in-flight window | Ctrl-C responsive under an output flood at 300 ms RTT |
| 3a | QUIC leg with migration and ssh fallback | A switch from Wi-Fi to tethering, or a 10-minute suspend, resumes within about 1 RTT of the link returning |
| 3b | Surface acks, resync, cancel-on-supersede | 10 % loss does not stall typing or output |
| 4 | Prediction (flagged) | At 150 ms RTT in zsh, most printable keys appear in under 5 ms and wrong predictions are cleared within one RTT |
| 5 | Outage banner | Outage visible within the heartbeat interval |

Order: 0 -> 1 -> (1.5) -> 2 -> 3a -> 3b -> 4 -> 5. Phases 3 and 4 can overlap.

## 6. Minimal-diff strategy

Upstream churn over 6 months, in commits touching the file: `app/mod.rs` 176, `server/headless.rs` 156, `main.rs` 145, `client/mod.rs` 98, `client_transport.rs` 34, `wire.rs` 34, `render_stream.rs` 31, `remote/attach.rs` 28.

**Rules**

1. **Fork code lives in a fork tree:** `src/mosh/`, `tests/mosh_*.rs`, `scripts/mosh-*`. The review surface is `git diff origin/master -- ':!src/mosh' ':!tests/mosh_*' ':!scripts/mosh-*' ':!HERDR_MOSH_VISION.md'`.
2. **Mount the tree with `#[path]`.** Phase 1 uses one line in `src/remote/attach.rs`: `#[path = "../mosh/mod.rs"] pub(super) mod mosh; // mosh-fork hook`.
   - That makes every fork module a descendant of `remote::attach`, so it can call private upstream helpers (`start_command`, `bridge_child`, `remote_client_status`, `remote_binary_candidates`, `posix_remote_output_command`) without visibility bumps or copies.
   - Later phases that need server or client internals mount separate fork files under those owners.
   - Upstream already uses `#[path]` (`server/headless/tests/mod.rs:5-13`).
3. **Hooks are one statement at a stable anchor,** tagged `// mosh-fork hook`. `git grep -n mosh-fork` lists the whole touch surface.
4. **Prefer swap points to edits.** The Phase 1 and Phase 3 transport seams are the two `SshStdioBridge::start(` UI call sites, `attach.rs:90` and `saved.rs:35`. The fork's `start_ui_bridge` returns an `SshStdioBridge`, so callers' types don't change.
5. **Call upstream internals; never copy them.**
6. **Frozen contracts stay byte-identical:**
   - `ClientMessage` and `ServerMessage` variant order, `PROTOCOL_VERSION`, `ENDPOINT_PROTOCOL_GENERATION`;
   - `EndpointClientHello` fields and the bincode surface structs;
   - `tests/fixtures/endpoint-method-shapes-v1.json`.

   New semantics ride on `EndpointControl{kind,data}` plus capabilities.
7. **Prefix every fork-introduced name with `x-mosh`:** kinds, capabilities, flags and env vars.
8. **Never add hello fields.** Each one spreads to about 24 `ClientShellConnected` literals. Opt in implicitly with the first fork control message.
9. **Use runtime capability negotiation, not cargo features.** Upstream has no `[features]` table.
10. **Dependencies.** Phase 1 adds none. Phase 3 adds one comment-headed block in `Cargo.toml`. For `Cargo.lock`, take upstream's version and run `cargo check`; never `cargo update` in the fork. Never bump `version =`; label fork builds with `HERDR_BUILD_CHANNEL` / `HERDR_BUILD_ID`.
11. **Tests live in fork files.** The crate is binary-only, so in-process tests go in `#[cfg(test)]` modules under `src/mosh/`, and end-to-end tests drive the binary with fake ssh on `PATH` (pattern: `tests/remote_attach.rs:69-107`). Do not add cases to upstream test modules.
12. **Merge upstream weekly,** and run `scripts/mosh-diff-budget.sh`, which fails above 20 changed upstream lines through Phase 2.

**Phase 1 touch list (existing files)**

| File | Change | Lines |
|---|---|---|
| `src/remote/attach.rs` | `#[path] mod mosh` line; `run_remote` l.90 `SshStdioBridge::start(` → `mosh::start_ui_bridge(&remote_ssh, ` | 2 |
| `src/remote/saved.rs` | `connect_saved_ssh` l.35 swap | 1 |
| `src/remote/host.rs` | guard arm `[o] if o.starts_with("--x-mosh-") => false`; dispatch to `mosh::remote::dispatch(args)` after `ensure_remote_server_running` | 2 |
| `src/cli/status.rs` | push the literal `"x-mosh-resume-bridge-v1"` into `endpoint_capabilities` (Unix) | 1 |
| `src/client/mod.rs` EndpointControl arm (l.~2093) | outage-banner hook: consume `x-mosh.link.v1` status (D5) | 1 |
| `src/client/shell/composition.rs` | outage-banner paint hook (D5) | 1 |
| `src/update.rs` l.25-26 | `option_env!` manifest URL override, so `auto_update` doesn't replace a fork build with upstream. **Release blocker**; upstreamable | 2 |

**Phase 1 new files.**

| File | Contents |
|---|---|
| `src/mosh/mod.rs` | API surface, constants, kill switch |
| `src/mosh/session.rs` | Session protocol |
| `src/mosh/ring.rs` | Replay rings |
| `src/mosh/link_frames.rs` | Frame splitter and keepalive encoder |
| `src/mosh/ssh_leg.rs` | Leg manager, capability probe, candidate re-selection, failure classification |
| `src/mosh/local_bridge.rs` | Local bridge |
| `src/mosh/remote/{mod,attacher,keeper}.rs` | Flag dispatch, per-leg attacher, keeper daemon |
| `tests/mosh_resume.rs` | End-to-end resume tests |
| `tests/mosh_multi_client.rs` | End-to-end multi-client tests |
| `scripts/mosh-diff-budget.sh` | Upstream diff budget check |

**Conflict budget** (lines in existing upstream files)

| Through | Total lines | In the hottest files |
|---|---|---|
| Phase 1 | about 10 | 1 (`client/mod.rs`, banner) |
| Phase 3a | about 13 | 0 |
| Phase 5 | about 143 | about 47 |

Most of the later-phase lines are Phase 3b acks (about 90) and Phase 4 hooks (about 38).

**Hot regions to avoid** (hook only at a stated anchor, if at all):

| File | Region |
|---|---|
| `server/headless.rs` | `ClientShellConnected` handler (l.1899-1990) and `remove_client` (l.946-1027) |
| `client/mod.rs` | ServerMessage, PaneSurfacePatch, EndpointControl and failure-drain arms |
| `client/handshake.rs` | `do_handshake` |
| `client/endpoint/supervisor.rs` | l.441-540 (upstream is reworking reconnect) |
| `main.rs` | `mod` block |
| `app/mod.rs` | whole file |
| `protocol/wire.rs` | whole file |

## 7. Pieces to upstream separately

Each fixes a real upstream bug or adds a generic seam, and shrinks the long-lived fork diff.

1. **Resync instead of fatal desync.** Today a mismatched patch clears the decoder baseline, and the next codec message disconnects (`surface_reuse.rs:160-163`, `client/transport.rs:63-69`). This is also the base for Phase 3b.
2. **Handshake `TimedOut` misclassification.** It currently lands in the 30 s attention lane instead of the backoff ladder (`saved.rs:155-180`).
3. **`run_remote` skips `prepare_shared_transport`.** It can attach through a stale ControlMaster (`attach.rs:80`).
4. **Manifest URL override seam** (`update.rs:25-26`, `attach.rs:46-47`).
5. **Prefer a remote candidate that advertises a requested capability.**
6. **Idle watchdog for interactive `--remote` bridges** (`attach.rs:3016-3022`).
7. **`SO_SNDBUF` cap and a control-lane cap** on shell client sockets, if Phase 0 confirms staleness or memory growth.
8. **Visibility-only refactor** of the `SshStdioBridge` building blocks.

## 8. Decisions

| ID | Decision | Status |
|---|---|---|
| — | QUIC (`quinn`) over a hand-rolled UDP layer | **Decided** |
| — | Multiple simultaneous clients supported | **Decided** |
| — | Phase 1 ships as its own release | **Decided** |
| D1 | Phase 1 = link-level session (B), with protocol acks moved to 3b | **Decided** |
| D2 | Grace default 60 s, client-set, longer opt-in | Proposed |
| D3 | Phase 2 = keeper window first; server pacing only on evidence | Proposed |
| D5 | Pull the 2-line outage banner into Phase 1 to remove the "online but frozen" regression | **Decided: in Phase 1** |
| D6 | Late execution of client-timed-out requests: accept and document (vs. bridge decoding and dropping stale `EndpointRequest`s) | Proposed: accept |

## 9. Risks

1. **Resume window vs. the client health timeout.** If injection breaks its rules, the client falls back to today's full reconnect. That is safe, but resume is lost.
2. **Password-only ssh auth cannot resume.** Reconnect legs bypass the ControlMaster and run in BatchMode. Such a leg is classified fatal, and today's path runs.
3. **Busy panes can overflow the keeper ring during a long outage.** The session ends and a full reconnect runs.
4. **The parked-client side effects in §4.1.** Phase 1.5 is the fix.
5. **Socket-buffer staleness at the keeper→server hop.** Measure in Phase 0.
6. **A remote PATH that prefers an upstream binary.** Handled by re-selection; otherwise `HERDR_REMOTE_BINARY` covers it.
7. **An upstream rename of `attach.rs`.** The `#[path]` line breaks with a compile error and a one-line fix.
8. **Prediction may predict poorly on modern TUIs** (zsh autosuggest, fzf, agent TUIs) compared with mosh's 2012 traces. Measure before defaulting it on.
