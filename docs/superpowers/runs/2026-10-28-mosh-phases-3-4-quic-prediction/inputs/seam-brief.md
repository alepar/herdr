# Seam brief (research for Phases 3-4, 2026-10-28, worktree at ef7caeac)

Facts read in code; file:line citations. Unverified points at the end.

## 1. Transport

**Topology:**
- `start_ui_bridge` (src/mosh/mod.rs:83) calls `local_bridge::start` (mod.rs:163, local_bridge.rs:115).
- Leg = `ssh -T <target> "<marker printf>; exec herdr remote-client-bridge [--idle-timeout-v1] --x-mosh-session-v1"` (ssh_leg.rs:376-427).
- Remote attacher (src/mosh/remote/attacher.rs:20) finds or spawns the keeper over the rendezvous Unix socket, checks the peer uid, then pumps stdio (`forward_remote_bridge_stdio`, attacher.rs:27-37).
- Keeper connects to the herdr server client socket (keeper.rs:503).

**Leg:**
- `LegKind{Initial,Reconnect}` (ssh_leg.rs:16).
- `Leg` (ssh_leg.rs:55): transport-neutral boxed Read/Write plus a teardown closure; `take_reader`/`take_writer`/`mark_welcome`/`finish` → `LegEndReport` (:23).
- `LegError{Retryable,Fatal,Unsupported}` (:160); Unsupported on leg 0 falls back to the plain bridge.
- `trait LegDialer: Send` (:175-190): `dial(kind, setup_deadline, &AtomicBool) -> Result<Leg,LegError>` returns only after the ready marker is consumed; `classify`; `backoff` (1,2,4,8,8 s; :498).
- Doc comment :171-173: "A new transport is a new impl registered in `start_ui_bridge`". The single registration point is mod.rs:155-176, which builds `SshLegDialer::new(...)` as `Box<dyn LegDialer>`; shared as `Arc<Mutex<Box<dyn LegDialer>>>` (local_bridge.rs:215).
- `dial` runs on its own thread (local_bridge.rs:486-495).
- Upstream callers: src/remote/attach.rs:91, src/remote/saved.rs:35.
- Capability probe and candidate selection: ssh_leg.rs:521-565.

**Ready marker:**
- `herdr-remote-output-ready:1` (attach.rs:50), printed by `posix_remote_output_command` (attach.rs:556).
- `dial_command` (ssh_leg.rs:431-475) spawns ssh, runs `discard_remote_output_preamble` (attach.rs:3460) on a thread, polls every 100 ms.
- The keeper has stdin/stdout/stderr = null, is detached, and has `SSH_AUTH_SOCK` removed (attacher.rs:192-205), so bootstrap lines must be printed by the attacher, which owns the ssh stdio.

**Rendezvous:**
- Paths: `$TMPDIR/herdr-mosh-<euid>/k-<hash>.{sock,live,spawn}`, falling back to /tmp when the path exceeds 103 bytes (remote/mod.rs:180-192).
- Hash: sha256 over euid, session, `SESSION_PROTOCOL_VERSION`, build id and client socket path (:157-177).
- Directory mode 0700; flock'd live and spawn locks (attacher.rs:68-110).
- Keeper idle exit after 30 s (keeper.rs:44).

**Session protocol (session.rs):**
- Framing: `[u8 type][u32 LE len][payload]` (:217).
- Types: HELLO=1, WELCOME=2, DATA=3, ACK=4, HEARTBEAT=5, END=6 (:47-52). `SessionMsg` at :207.
- `Hello{v, token, resume, recv_offset, grace_ms, previous_token?}` (:174).
- `Welcome{v, status New|Resumed|Unknown, recv_offset}` (:193).
- Control payloads are JSON (≤4096 B); DATA payloads ≤64 KiB.
- `SESSION_PROTOCOL_VERSION=1`.
- Timing constants: heartbeat 2 s, silence 8 s, pre-WELCOME 20 s, setup 15 s, window cap 2 MiB, ack every 64 KiB or 200 ms.
- `SessionEndpoint` (:360) is pure and owns the offsets, replay ring and `InflightWindow` (window.rs:59; base_rtt :69/:192; `WindowSnapshot` :26).
- The keeper never parses herdr frames (keeper.rs:1-3).

**Threads:**
- src/mosh uses std threads, mpsc and condvars, no tokio.
- Bridge (local_bridge.rs:6-10): leg I/O/state thread; reader and writer per leg; client reader/writer; dial thread.
- Keeper: polling accept loop at 50 ms (keeper.rs:46); thread per connection (:403); per session `leg_writer`, `server_reader`, `server_writer` (:542-543, :629).
- Client runtime: tokio current_thread (src/client/mod.rs:345). Server runtime: multi_thread (src/server/headless/bootstrap.rs:58).

**Cargo:**
- tokio = 1 with rt-multi-thread, macros, sync, time, process, io-util, signal (Cargo.toml:48).
- `net` is on transitively (probably via zbus).
- Cargo.lock has no quinn, rustls, rcgen, ring or aws-lc-rs.

## 2. Server → client render lane

**Lanes (client_transport.rs):**
- `ClientWriter{control, render}` (:122).
- Queue (:274): control VecDeque, ordered VecDeque, `render: Option<Vec<u8>>`.
- `try_send_render` → Full when occupied (:320-331).
- `send_ordered` moves the pending render into `ordered` (:340-354).
- `discard_pending_render` (:333).
- `recv` priority is control > ordered > render (:356-377).
- The writer loop emits `ServerEvent::ClientWriterDrained` before each render write (:951-972).

**Render senders:**
- render.rs:963-967 (full) and retained_surface.rs:566-570 (patch). They use `send_ordered` when native graphics are pending, else `try_send`.
- `commit_sent_frame` runs on enqueue (render_stream.rs:284; retained_surface.rs:591). The base advances on enqueue, which assumes reliable in-order delivery; there are no client acks.
- `discard_pending_render` on deactivation (headless/surface_interest.rs:46).
- Snapshot (`shell.snapshot.v1` EndpointControl) goes on the control lane (render.rs:720-722).

**Encoding:**
- `write_message` = u32 LE length + bincode serde `config::standard()` (protocol/wire.rs:1656).
- The varint tag is the first payload byte for values < 251.
- `PROTOCOL_VERSION=22` (wire.rs:20). `MAX_FRAME_SIZE` 2 MiB; graphics 32 MiB.

**ServerMessage tags (frozen, wire.rs:1381-1503):**

| Tag | Variant |
|---|---|
| 0 | Welcome |
| 1 | Terminal |
| 2 | Graphics |
| 3 | ServerShutdown |
| 4 | Notify |
| 5 | Clipboard |
| 6 | WindowTitle |
| 7 | ReloadSoundConfig |
| 8 | MouseCapture |
| 9 | TerminalBell |
| 10 | GraphicsFile |
| 11 | GraphicsTransmissionRetired |
| 12 | ClientShellSnapshot |
| 13 | PaneSurface(PaneSurfaceFrame) |
| 14 | SemanticNotification |
| 15 | ClientShellError |
| 16 | DirectTerminalKeyboardProtocol |
| 17 | ClientShellKeyboardReportAll |
| 18 | ClientShellEndpointResponseChunk |
| 19 | PaneSurfacePatch |
| 20 | EndpointControl{kind,data} |

- Render-related kinds: `endpoint.surface-reuse.v1` (protocol/surface_reuse.rs:7), `endpoint.surface-delta.v1` (surface_delta.rs:11), `endpoint.surface-scroll.v1` (surface_scroll.rs:16).
- `ClientMessage` (wire.rs:464): `ClientShellPaneInput`=15, `EndpointControl`=20 (wire.rs:616).

**Revisions:**
- `PaneSurfaceFrame{boot_id, projection_revision, surface_revision, frame, panes, splits, popup, graphics}` (wire.rs:1268).
- `PaneSurfacePatch{boot_id, projection_revision, base_surface_revision, surface_revision, rows, panes, cursor}` (wire.rs:1299).
- `PaneSurfacePane{pane_id, content_revision, rect, inner_rect, scrollbar_rect, scroll, focused, ...}` (wire.rs:1090).

**Strict chain:**
- (1) Transport `surface_reuse::Decoder` (surface_reuse.rs:85-196) runs in the client reader thread (client/transport.rs:63-69). A reuse/delta/scroll mismatch → Err → `FramingError::Io(InvalidData)` → **ServerDisconnected** (transport.rs:94-100). A plain `PaneSurfacePatch` mismatch only drops the baseline (surface_reuse.rs:161-162).
- (2) `ClientShellState::apply_pane_surface_patch` (client/shell/surface_patch.rs:110-125) → Rejected → no-op (client/mod.rs:1606).
- No resync request exists. Recovery is the next full PaneSurface, or snapshot + surface after reconnect.

**Client EndpointControl at the server:** unknown kinds are ignored (client_transport.rs:1349-1353); only health ping is handled (server/client_endpoint_control.rs:1-8).

## 3. EndpointControl, capabilities, link frames

**Endpoint handshake:**
- `endpoint.hello.v1` / `endpoint.welcome.v1` (protocol/endpoint.rs:16-17).
- `EndpointClientHello` (:46): `#[serde(default)]` bools surface_reuse, surface_delta, surface_scroll; codec lists.
- `EndpointServerWelcome` (:94): `methods: Vec<String>`, `capabilities: Vec<String>`.
- `compatible()` (:158-180).
- Client checks with `negotiation.supports_capability` (client/mod.rs:169-173); hello flags sent at client/handshake.rs:207-209.

**Status capabilities:**
- `status client --json` `endpoint_capabilities` (cli/status.rs:298-304) includes `x-mosh-resume-bridge-v1` (hook lines 302-303).
- Constant at src/mosh/mod.rs:28; tested at remote/mod.rs:108-112; probing requires it (ssh_leg.rs:537-543).

**AGENTS.md stable client endpoint contract (AGENTS.md:83-94):**
- Codecs are immutable; new JSON fields must be optional.
- Features go through advertised methods/capabilities; a missing feature disables only that action.
- Frozen fixtures are never re-blessed; enums are append-closed.
- wire.rs:1497-1502: the tag-20 EndpointControl two-string payload is frozen for generation 1.

**`x-mosh.link.v1`:**
- The local bridge writes `ServerMessage::EndpointControl{kind:"x-mosh.link.v1", data: JSON LinkStatus}` (src/mosh/link_frames.rs:82-94), injected at frame boundaries via `FrameSplitter` (:19-55) as `ClientOut::Injected` (local_bridge.rs:937-943), repeated every 1 s while resuming.
- Client hook src/client/mod.rs:2095 → `mosh_outage::observe` (outage.rs:110-118); true ⇒ recompose and continue.

## 4. Client rendering and input

- Retained surface: `ClientShellState.pane_surface: Option<PaneSurfaceFrame>`, `pending_pane_surface` (client/shell/state.rs:864-868). The transport decoder has its own baseline (surface_reuse.rs:45-54).
- Compose (client/shell/composition.rs:150):
  - early bail on missing or pending surface (:173-186);
  - pane `inner_rect` mapped by adding the `layout.pane_surface` x/y (:240-260);
  - `blit_pane_surface` (:346);
  - overlays; outage hook (:723) before `compose_graphics` (:724).
- **The patch fast path bypasses compose:** `Applied(Some(patch))` → `state.present_surface_patch` (client/state.rs:325-368) encodes rows straight to stdout. `fast_path_blocker` (client/shell/surface_patch.rs:56-90) ignores the outage banner and any prediction overlay.
- Input: `ClientShellState::handle_input_bytes` (client/shell/input.rs:155) → `push_target_event` builds `ClientMessage::ClientShellPaneInput{pane_id, events}` (client/shell.rs:69-120) → sent from client/shell_runtime.rs:805.
  - `ClientPaneInputEvent{Key{... generated_text ...}, TextCommit, Mouse, Paste}` (wire.rs:161-182).
  - Server handles input at client_transport.rs:1196.
- No client RTT: the health ping (5 s / 10 s timeout, client/endpoint/health.rs:3-4) tracks liveness only. The only RTT is the bridge's `InflightWindow.base_rtt`, which runs in the same process.

## 5. Test harness (tests/mosh_harness.rs)

- Fake ssh script (`write_wrappers` :405-505) runs `$last` locally with the remote env (HOME, TMPDIR, HERDR_MOSH_TRACE, ...).
- Knobs (:170-190): fail255, hostkey255, hang, stall-welcome, eof-welcome, eof-reconnect, fake-end, strip-capability, downgrade, spawn-fail, throttle, link.
- `set_link(one_way_delay_ms, rate)` (:598) runs a perl delay line on byte streams (:203-261, :454-458), with no loss.
- Remote API: start, request, pane_text, pane_run, set_knob, linked_legs, leg_pids, signal_leg, keeper_events/wait_keeper_event (:677-695), keeper_pids, spawn_remote_client/spawn_saved_client (:832-837).
- Client API: screen, type_keys, type_mark, wait_screen, events/wait_event, has_welcome, wait_exit (:1003-1110).
- `timing_lock()` (:54-70) is a host-wide flock.
- Pacing tests (tests/mosh_pacing.rs:5-13) are `#[ignore]`, run with `--run-ignored all`; `HERDR_MOSH_PACING_TESTS` is no longer read. They need perl and ss, printing SKIP otherwise.
- **No UDP facility anywhere**: a userspace UDP relay is needed.

## 6. Upstream hook lines (12/20)

- cli/status.rs:302, 303
- client/mod.rs:33, 2095
- client/shell/composition.rs:723
- remote/attach.rs:30, 91
- remote/host.rs:15, 26
- remote/saved.rs:35
- update.rs:25, 26

## Not determined

- Which crate enables tokio `net`.
- The full server path from a client EndpointControl beyond the ignore at client_transport.rs:1349-1353 (a new ack/resync kind needs a server hook there).
- Whether `discard_pending_render` can strand the client's base revision.
