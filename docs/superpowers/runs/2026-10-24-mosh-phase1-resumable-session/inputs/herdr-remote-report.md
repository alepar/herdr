# herdr Remote-Machine Support: Transport, Protocol and Seams for a Mosh-like Layer

All citations are relative to `/var/home/alepar/AleCode/herdr`. Claims marked **[verified]** were re-checked against the code after review; see "Verification notes" at the end.

---

## 1. User-facing model

herdr can be used on a remote machine in three ways.

1. **SSH in, then run `herdr` on the host** (like tmux).
   - The whole UI runs remotely. Only a normal SSH TTY crosses the wire.
   - The docs recommend this for phones and mention the moshi iOS app (`how-to-work.mdx:34-58`).
   - In this mode herdr cannot reach the local clipboard (`persistence-remote.mdx:95`).
2. **`herdr --remote <ssh-target>`** (thin client).
   - "The remote server owns the running panes and sends their terminal content and session state over SSH. Your local Herdr draws the UI" (`persistence-remote.mdx:57`).
   - Options: `--remote-keybindings local|server` and `--session` (`src/remote/args.rs:35-117`, `persistence-remote.mdx:59,105-111`).
   - Dispatched from `src/main.rs:787-795` to `run_remote`.
3. **Saved SSH machines** (`herdr machine add <host>`), "several machines, one window" (`README.md:32`; `src/cli/spec/machine.rs:5-52`).
   - Each machine has its own server. Only the selected machine streams pane screens; the others send only metadata (`connecting-machines.mdx:8,60`).
   - Background connections run through the endpoint supervisor (`src/client/endpoint/supervisor.rs:476-484` → `src/remote/saved.rs:20-57`).
   - CLI and API commands are forwarded with `herdr --machine <id> ...` (`connecting-machines.mdx:118`).

**What runs where (modes 2 and 3):**

| Side | What runs there |
|---|---|
| Remote | herdr server daemon: PTYs, a libghostty-vt emulator per pane, and the renderer |
| Remote | `herdr remote-client-bridge`, a dumb stdio↔socket pump (`src/main.rs:565-567` → `src/remote/host.rs:6`), started by sshd through the remote user's login shell |
| Local | `SshStdioBridge` thread (inside `herdr --remote`, or inside the local client process for saved machines) plus its `ssh` child processes and, by default, a background ssh ControlMaster |
| Local | re-exec'd `herdr client` TUI: draws the chrome and composes the pane surface (`src/remote/attach.rs:3577-3631`) |

Pane processes survive SSH drops because the server is a detached daemon that ignores SIGHUP (`persistence-remote.mdx:16`; `src/remote/restart_policy.rs:33-35`).

**Bootstrap.** Before connecting, herdr prepares the remote side.
- It detects the remote platform with `uname`. Only Linux and macOS on x86_64 or aarch64 are accepted (`src/remote/attach.rs:1032-1059, 1737-1758, 310-322`).
- It looks for a remote binary (`attach.rs:1850-2017`) and probes compatibility with `status client --json` (`attach.rs:2019-2034, 2411-2448`).
- If needed, it installs the binary. On POSIX it streams it through `ssh target "tee <tmp>"` (stdout to /dev/null), then runs a separate commit script; Windows uses `scp` with the same managed options (`-C`, `-F`, `-S`) (`attach.rs:1098-1153, 1290-1335, 1443-1462`).
- It checks server readiness. If the server is incompatible, it does a live handoff or a stop/restart (`attach.rs:82-88, 2186-2235`; `restart_policy.rs:16-37`). For saved machines, server startup is a separate one-shot `saved_bridge_command` (`exec ... </dev/null`, no ready marker) run via `ssh.shell_output` in `SavedSshSetup` (`attach.rs:237-244, 438-446`), distinct from the streaming bridge.

---

## 2. Transport stack, end to end

herdr has no network transport of its own. All traffic is the stdio of a system `ssh -T <target> <cmd>` child. There is no `-L/-R/-W` forwarding, no agent forwarding (`-A`), no UDP and no herdr listener on the network (`src/remote/attach.rs:1` "Remote thin-client launcher over SSH command stdio"; `attach.rs:3259-3285`).

```
 LOCAL HOST                                                        REMOTE HOST
 ┌────────────────────┐ unix sock (0600)  ┌────────────────────┐
 │ herdr client (TUI) │◄────────────────► │ SshStdioBridge     │
 │ HERDR_CLIENT_      │ $TMPDIR/herdr-    │ thread (serial     │
 │ SOCKET_PATH=bridge │ remote-<pid>-...  │ accept loop)       │
 └────────────────────┘                   └─────────┬──────────┘
                                                    │ stdin/stdout pipes
                                          ┌─────────▼──────────┐
                                          │ ssh -C -T (mux     │
                                          │ client, per conn)  │
                                          └─────────┬──────────┘
                                                    │ control socket
                                          ┌─────────▼──────────┐
                                          │ ssh ControlMaster  │  ONE TCP conn  ┌────────────────────┐   ┌──────────────────┐
                                          │ (persist 600 s,    │◄══════════════►│ sshd → login shell │   │ herdr server     │
                                          │  owns keepalive)   │  per target    │ → exec herdr       │◄─►│ client unix sock │
                                          └────────────────────┘                │ remote-client-     │   │ PTYs + ghostty   │
                                                                                │ bridge (16K x2)    │   └──────────────────┘
                                                                                └────────────────────┘
```

### Local bridge (`SshStdioBridge`)
- It binds a 0600 Unix listener and polls accept every 50 ms (`attach.rs:31, 33, 3030-3100`).
- **Connections are served serially.** The accept loop calls `bridge_connection` inline on the single listener thread, which spawns one `ssh` child for that connection and blocks until it ends. A second local connection waits in the listen backlog (`attach.rs:3056-3068`). **[verified]**
- **Upload** (socket → ssh stdin): 16 KiB reads, `write_all` plus flush per read. It is event-driven: it blocks on a `poll()`-based wake (`RemoteBridgeWake`, up to 100 ms per wait) rather than sleeping on a timer (`attach.rs:3541-3575`; `src/platform/unix_common.rs:200-205`).
- **Download** (ssh stdout → socket): ssh stdout is wrapped in a default 8 KiB `BufReader` so the preamble can be scanned (`attach.rs:3324-3333`). After the marker, buffered bytes drain first, then 16 KiB reads bypass the buffer. Writes go out in 4 KiB nonblocking chunks (512 B on Windows), sleeping 1 ms on `WouldBlock`, with a flush per read (`attach.rs:3502-3539, 32`).
- The download thread first discards the remote preamble (MOTD and shell noise) up to the marker `herdr-remote-output-ready:1` (`attach.rs:49, 555-557, 3459-3500`).
- A supervisor loop polls every 50 ms. When the upload or download finishes or the client closes, it kills the ssh child after a 250 ms grace period (`attach.rs:3361-3394`).
- **Stderr:** interactive `--remote` inherits ssh stderr to the terminal, so password and host-key prompts work on first connect. Saved bridges capture up to 16 KiB of stderr for error classification (`attach.rs:3269-3277, 3418-3430`).
- **Socket path:** `$TMPDIR/herdr-remote-<pid>-<target>-<session>.sock`, falling back to a short name `herdr-r-<pid>-<target8>-<hash16>.sock`, then to `/tmp/<short>`, when the 103-byte `sun_path` limit would be exceeded (`attach.rs:3633-3642`; `src/platform/unix_common.rs:506-517`). Saved-machine variants: `herdr-ssh-<pid>-<profile>.sock` for the UI bridge and `herdr-api-ssh-<pid>-<profile>.sock` for the API bridge (`src/remote/saved.rs:99-106, 182-187`).

### ssh invocation
**Base command line** (`attach.rs:3259-3285`):
`ssh -C [-F <managed cfg> -S <ctl> -o ControlMaster=auto -o ControlPersist=600] [noninteractive opts] -T <target> "<remote cmd>"`
- `-C` is always passed (`attach.rs:1421-1423`).
- Mux flags are added when `remote.manage_ssh_config` is set (default `true`, `src/config/model.rs:1070`) and the platform supports multiplexing (Unix does, `unix_common.rs:429-437`; flags at `attach.rs:1428-1440`).
- **Three managed-config entry points behave differently:** `RemoteSsh::new` (used by `--remote`) honours `manage_ssh_config`; `new_noninteractive` (saved machines) additionally requires platform multiplexing; `ssh_authentication_command` hard-errors unless `manage_ssh_config=true` and multiplexing is available (`attach.rs:768-790, 825-857`).

**ControlMaster semantics.** With `ControlMaster=auto` and `ControlPersist=600`, the first ssh forks a background master that owns the TCP connection; bridge ssh children are mux clients. Killing a bridge child (the 250 ms grace kill) closes only its channel. The TCP link and its `ServerAlive` keepalives live in the master, which persists 600 s after the last client (`attach.rs:1426-1440, 3376-3384`).

**Control socket.** `/tmp/hssh-<euid>/<24hex sha256(config path + target)>-%C`. Because the path hashes the target and includes OpenSSH `%C`, **each machine has its own master and TCP connection** (`unix_common.rs:713-779`; `attach.rs:3157-3166`). The 0700 directory is validated; its parent must be root-owned and sticky, and the path length reserves 17 bytes for OpenSSH's staging suffix (`unix_common.rs:722-768`).

**Stale-master handling (`prepare_shared_transport`, saved machines only).** Called from `connect_saved_ssh`, the API-bridge retry and `check_saved_ssh` (`saved.rs:28, 87, 137`; `attach.rs:143`). **Plain `herdr --remote` (`run_remote`, `attach.rs:55-98`) never calls it.** **[verified]** The sequence (`attach.rs:877-971`):
1. Resolve the effective `controlpath` with `ssh -G <target>`.
2. Use it only if it sits in herdr's own `/tmp/hssh-<euid>` directory. A user `ControlPath` in `~/.ssh/config` wins via first-value-wins includes, and herdr then skips probe/retire (`attach.rs:906-909`, comment at `1429-1432`).
3. Pin the socket by dev/ino (`PinnedSshControlSocket`) so it never retires another helper's replacement master (`attach.rs:883-912`; `unix_common.rs:782+`).
4. Mux-only probe (`-o ProxyCommand=false ... exit`).
5. On failure, a fresh-connection probe with `-S none`; only if that succeeds, `ssh -O stop` the stale master.

**Managed ssh config.** It includes `~/.ssh/config` and `/etc/ssh/ssh_config` first, so user values win, then adds `Host * ServerAliveInterval 15 / ServerAliveCountMax 4` (`attach.rs:3156-3200`).

**Noninteractive options** (saved machines, on the command line, so they override user config even with `manage_ssh_config=false`): `BatchMode=yes`, `NumberOfPasswordPrompts=0`, `StrictHostKeyChecking=yes`, `ConnectTimeout=10`, `ConnectionAttempts=1`, `ServerAliveInterval=15`, `ServerAliveCountMax=4` (`attach.rs:1403-1419, 3270-3273`).

**Interactive auth for saved machines.** `herdr machine reconnect` runs an interactive ssh (`BatchMode=no`, `NumberOfPasswordPrompts=3`) through the managed config and ControlPath with `ControlMaster=auto/ControlPersist=600`. That establishes an authenticated master that the BatchMode bridges then multiplex over; open clients retry within 30 s (`attach.rs:768-815`; `src/cli/machine.rs:193-205`).

**Remote command.** `printf '\n%s\n' 'herdr-remote-output-ready:1'; exec <herdr> [--session S] remote-client-bridge [--idle-timeout-v1]` (`attach.rs:417-436, 500-502`).
- `<herdr>` defaults to `"$HOME/.local/bin/herdr"`, but `find_installed_remote_herdr` and the candidate probe can replace it with a discovered, shell-quoted absolute path (`with_posix_path`) (`attach.rs:496-517, 1583-1596, 2012`).
- `--session` is added only for a non-default session (`attach.rs:388-394`).
- The string is run by the remote user's login shell via sshd; herdr assumes only POSIX `printf`/`exec`.
- Windows runs it through PowerShell `-EncodedCommand` (`attach.rs:536-553, 574-592`).

### Remote bridge (`run_remote_client_bridge`, `src/remote/host.rs:6-63`)
- If no server is running, it spawns the daemon and waits 5 s for the socket.
- It requires a matching `endpoint_protocol_generation`.
- It connects to `client_socket_path()` and runs `forward_remote_bridge_stdio`: two `copy_flush` loops with 16 KiB buffers and a flush per read (`src/platform/unix_common.rs:213-261`). The bridge exits when the socket→stdout direction ends; the stdin→socket thread only half-closes the socket write side (`unix_common.rs:224-243`).
- With `--idle-timeout-v1` (**saved bridges only**), a watchdog exits after 60 s with no I/O. On Linux it uses `CLOCK_BOOTTIME`, so suspended time counts (`src/platform/remote_bridge.rs:8-56`; `attach.rs:3016-3022`).
- It registers the remote `$SSH_AUTH_SOCK` with the server (`src/remote/ssh_agent.rs:17-129`). Since herdr passes no `-A`, this is meaningful only if the user's own ssh config enables `ForwardAgent`.

### Side channel
CLI and API commands for a saved machine use a second `SshStdioBridge` running `remote-api-bridge`, which carries newline-delimited JSON (`src/remote/saved.rs:59-121`; `src/remote.rs:15-39`; `attach.rs:559-572, 2915-2942`). It shares the target's ControlMaster with the UI bridge.

---

## 3. Wire protocol

### Framing
- Each frame is `[u32 LE length][bincode serde payload, config::standard()]`, with no app-level compression (`src/protocol/wire.rs:1647-1673`).
- The reader rejects oversized lengths before allocating, and rejects trailing bytes (`wire.rs:1679-1715`).
- `MAX_FRAME_SIZE` is 2 MiB and `MAX_GRAPHICS_FRAME_SIZE` is 32 MiB (`wire.rs:24, 29`).
- The bridges never look inside the stream, so the remote link carries exactly the local client protocol. All panes of an endpoint share this one framed stream; there is no per-pane channel.

### Versioning
- **Private protocol:** `PROTOCOL_VERSION = 22`, exact match required. Used by same-install and direct-attach paths (`wire.rs:20, 1754-1772`).
- **Stable endpoint protocol:** `ENDPOINT_PROTOCOL_GENERATION = 1`, used for the normal UI client, both local and remote (`src/protocol/endpoint.rs:1-15`; `src/client/mod.rs:188, 197, 277-298`).

**Handshake.**
- The client sends `EndpointControl{kind:"endpoint.hello.v1", data: JSON}` with cell pixel size, surface cols/rows, mouse and graphics flags, codec lists, and the `surface_active`/`surface_reuse`/`surface_delta`/`surface_scroll` opt-ins (`endpoint.rs:46-75`; `client/handshake.rs:191-220`).
- The server replies with `endpoint.welcome.v1`, carrying methods and capabilities (`endpoint.rs:158-180`; `server/client_transport.rs:737-830`).
- Codecs: `shell.snapshot.v1`, `shell.surface.v1`, `shell.input.semantic.v1`, `shell.blob.v1`. Optional capabilities include `surface_interest`, `presentation_effects_fence` and `health_check` (`endpoint.rs:16-33, 142-154`).
- Handshake read timeout: 5 s locally, 60 s when remote (`client/handshake.rs:24-54`). The server side allows 4 s (`client_transport.rs:42`).

**Extension mechanism.** The top-level enum variant order is frozen. New messages ride in `EndpointControl{kind: String, data: String}`, where `data` is JSON or base64 of bincode (`wire.rs:612-617, 1500-1505`).

### Client → server (`ClientMessage`, `wire.rs:464`)
- **Pane input:** `ClientShellPaneInput{pane_id, events: Vec<ClientPaneInputEvent>}` and a popup variant. Events are Key (code plus modifiers plus text), TextCommit, Paste or Mouse (`wire.rs:161-183, 576-586`). Consecutive events for the same target are merged (`src/client/shell.rs:69-119`). The server caps a batch at 4096 events (`client_transport.rs:118`).
- **Other:** `ClientShellResize`, `ClientShellFocus`, `ClientShellHostTheme`, `ClientShellEndpointRequest{boot_id, request}` (`wire.rs:568-604`; decoding at `client_transport.rs:97-116`).
- **Health ping:** `EndpointControl{"endpoint.health.ping.v1"}` (`endpoint.rs:28`).
- **Legacy direct attach:** raw `Input{data}`, capped at 1 MB (`wire.rs:476-479`; `client_transport.rs:48`).

### Server → client (`ServerMessage`, `wire.rs:1384-1500`)
- `Welcome` / `endpoint.welcome.v1`.
- `ClientShellSnapshot`: JSON workspace, tab and agent state used to draw the chrome (`endpoint.rs:109-114`; `wire.rs:916`).
- Pane content: `PaneSurface(PaneSurfaceFrame)` (`wire.rs:1268-1279, 1452`), `PaneSurfacePatch` (`wire.rs:1289-1310`), and `EndpointControl` carrying the `surface_reuse`, `surface_delta` or `surface_scroll` codecs (see §4).
- `Terminal(TerminalFrame{seq,width,height,full,bytes})`, only on the TerminalAnsi/direct-attach path (`wire.rs:1326-1337`).
- `Notify`, OSC52 `Clipboard`, `WindowTitle`, `MouseCapture`, `TerminalBell`, Kitty `Graphics`/`GraphicsFile`, and `ClientShellEndpointResponseChunk`.
- Health pong, echoed statelessly (`src/server/client_endpoint_control.rs:1-8`).

---

## 4. Terminal emulation, rendering and update granularity

### Emulation is on the server
- One `GhosttyPaneCore` (libghostty-vt terminal plus `render_state`) per pane.
- Dirty rows are collected in `ghostty_collect_dirty_patch` via `render_state.update/dirty` and `rows.next_dirty()` (`src/pane/terminal.rs:2617-2700`), under a content-revision seqlock (`src/pane.rs:3590-3617`).
- The client never parses child VT output: "The server handles semantic parsing" (`src/client/input.rs:1-11`). The only ghostty code in the client is in tests (`src/client/terminal_setup.rs:1033-1037`).

### The server renders cell grids
- For semantic clients, the server renders the active tab's pane area with ratatui into a virtual backend (`src/server/render_stream.rs:546-574`). The surface is a composite of the whole tab pane area, borders and splits included, not per-pane terminal grids. Pane positions are given by `PaneSurfacePane.inner_rect`. No PTY mode state (echo, alt-screen, keyboard protocol) is sent.
- The unit is `CellData{symbol: String, fg: u32, bg: u32, modifier: u16, skip: bool, hyperlink: Option<u32>}` (`wire.rs:697-711`).
- A surface also carries pane geometry, splits, popup, graphics scene, `boot_id`, `projection_revision` and a monotonic `surface_revision`.

### Render encodings (`wire.rs:43-48`)
- `SemanticFrame` is chosen when the hello carries shell options (`client_transport.rs:825-829`). Remote endpoints must use it (`src/client/endpoint/supervisor.rs:510-515`).
- `TerminalAnsi` is used for direct `terminal attach`. The server keeps a `BlitEncoder` per client, sends pre-diffed ANSI and skips unchanged frames (`render_stream.rs:116-153`). The client writes those bytes straight to stdout (`client/mod.rs:1625-1634`).

### Granularity under SemanticFrame: two paths
The server picks the encoding at `render_stream.rs:171-282`; the client's decoder choice is at `client/mod.rs:166-173`.

**Full-recompute path** (`prepare_pane_surface`), preference order delta → reuse → full (`render_stream.rs:208-243`):

| Encoding | Content | Cite |
|---|---|---|
| `surface_delta` | sparse base64-bincode spans (at most 4096) against the last surface; falls back if not smaller | `src/protocol/surface_delta.rs:10-23, 60-209` |
| `surface_reuse` | metadata only, cells stripped; chosen only when the entire cell grid is unchanged (`last.frame == surface.frame`), no popup, no graphics assets | `src/protocol/surface_reuse.rs:6-46` |
| Full `PaneSurface` | entire pane-area grid | `wire.rs:1452` |

**Retained dirty-row path** (`render_stream.rs:245-282`):

| Encoding | Content | Cite |
|---|---|---|
| `PaneSurfacePatch` | changed cell spans per row, plus 1 trailing cell for wide/narrow transitions, plus cursor and pane metadata, against `base_surface_revision` | `wire.rs:1289-1310`; `retained_surface.rs:45-91, 440-476` |
| `surface_scroll` | per-pane vertical shift (rect plus i16) plus residual patch | `src/protocol/surface_scroll.rs:1-37` |

- **Dedup:** an identical full surface is not sent (`render_stream.rs:187-201`). An empty patch is skipped (`retained_surface.rs:457-459`).
- **Forced full renders** happen on: popup, CJK IME cursor reveal, `full_redraw_pending`, a baseline mismatch, DEC 2026 synchronized output on a visible pane, alt-screen geometry change, hyperlinks under a patch, and others (`retained_surface.rs:247-253, 269-317, 376-389, 441-445`).

### Client presentation
- The client composes its locally drawn chrome (sidebar, tabs, mode bar, overlays) with the pane surface and re-diffs the result with its own `BlitEncoder` into ANSI, using synchronized output `CSI ?2026 h/l` (`client/mod.rs:1547-1580`; `client/state.rs:511-595`; `client/frame_output.rs:33-101`; `src/protocol/render_ansi.rs:22, 39`).
- A patch fast path encodes only the changed spans (`client/shell/surface_patch.rs:171-247`; `client/state.rs:325-371`). It is blocked by overlays, selection, copy mode, notifications, link hover and similar states (`surface_patch.rs:57-94, 248-254`).
- **Patch acceptance** (`surface_patch.rs:113-139`) requires: an existing surface, matching snapshot generation, matching `boot_id` and `projection_revision`, `base == current` and `new == current+1`, no popup, no graphics placements or retained assets, and unchanged pane geometry and focus.

---

## 5. Connection lifecycle

There are two lifecycles. **[verified]**

- The `herdr --remote` client process connects to the bridge socket as if it were a local server, so its endpoint is `ClientEndpointId::Local` (`src/client/endpoint.rs:80-82`).
- Application health tracking is enabled only for `!endpoint_id.is_local()` (`src/client/endpoint/registry.rs:157-158`).
- A failure on a Local endpoint while not federated is fatal (`client/mod.rs:2278-2283`).
- "Federated" means at least one saved SSH machine is enabled (`client/mod.rs:243, 496`). **But in the remote client process the endpoint catalog is never loaded** (`if client_rendered_shell && !is_remote_client_process()`, `client/mod.rs:235-243`; `is_remote_client_process` = `REMOTE_KEYBINDINGS` env set, `client/handshake.rs:33-35`). So `federated` is always false under `--remote`, and a `--remote` failure is always fatal regardless of saved machines. **[verified]**

### 5a. Plain `herdr --remote`
- **Health:** no app-level ping. Liveness comes only from ssh `ServerAliveInterval 15 × CountMax 4`, about 60 s, and only via the managed config file (user values win; `attach.rs:3156-3183`). With `manage_ssh_config=false`, only the user's ssh config applies. With a ControlMaster, the master process runs the keepalive.
- **Stale master risk:** because `run_remote` skips `prepare_shared_transport`, it can attach through a stale managed master (TCP dead, process alive) and hang until that master's keepalives expire.
- **Disconnect:** reader EOF or error, or the ssh child exiting, closes the bridge socket (`client/transport.rs:84-101`; `attach.rs:3362-3390`). That produces `ClientError::ConnectionLost`, and the process **exits** after printing "lost connection to remote Herdr ... Run `<reattach>` to reattach" (`src/client/errors.rs:62-71`; reattach command at `attach.rs:2962-2984`).
- **Remote half-open:** the remote bridge's 60 s idle watchdog is disabled for interactive bridges (`attach.rs:3016-3022`). After a `--remote` link dies, the remote bridge and its server client can linger until sshd notices (ClientAlive/TCP keepalive) and can keep constraining pane geometry.
- **No automatic reconnect.** The user re-runs the command, repeating binary preparation, the server check, the ssh connect, the handshake and a full snapshot.

### 5b. Saved SSH machines (endpoint supervisor)
**Connect** (`supervisor.rs:191-231, 476-537`):
- One attempt per endpoint, each with a fresh monotonic generation.
- `connect_saved_ssh` (`saved.rs:20-57`) issues several separate ssh process spawns on every attempt, each costing an RTT when remote: `ssh -G`; mux probe; on failure a fresh probe and `-O stop` (`attach.rs:877-971`); `uname`, binary-candidate lookup and `status client --json` per candidate (`attach.rs:1583-1597`); then the bridge ssh, which must connect within 5 s (`saved.rs:44-48`). These are cheap only while the master is alive.
- Handshake: a remote endpoint is rejected unless it negotiates `SemanticFrame`, `surface_interest`, `presentation_effects_fence` and `health_check` (`supervisor.rs:510-527`).
- The endpoint becomes visible only after its first `EndpointControl` snapshot (`control.rs:56-60`; `mod.rs:2201-2237`). The selected endpoint is then re-activated automatically (`ActivateEndpoint` scheduled, `mod.rs:2203-2236`).
- Activation must return a coherent snapshot and surface pair within 5 s. Input is frozen while it runs (`activation.rs:14, 38-171, 128`).

**Health** (`health.rs:3-6, 34-37, 48-67`; `registry.rs:215-256`):
- A ping is sent after 5 s with no inbound message; if unanswered for 10 s, the connection fails ("endpoint health check timed out").
- Before the first snapshot, the connection may take up to 60 s.
- Any inbound message counts as a reply.
- If the client loop itself stalls for more than 10 s (for example after laptop sleep), probes are reset rather than expired (`registry.rs:206-219`).

**Disconnect sources:**
- reader EOF, or a decode error turned into `ServerDisconnected` (`transport.rs:62-69, 84-101`; `mod.rs:2244-2255`);
- writer error, 5 s write deadline, or a full queue, treated as `ConnectionAborted` to preserve input ordering (`writer.rs:15, 186-193, 218-256`);
- health expiry (`registry.rs:250-253`);
- ssh child exit (which, under a ControlMaster, reflects the mux channel; the TCP link's liveness is the master's keepalive).

**Failure routing:** failures are drained on each Timer tick and are never fatal for a remote endpoint (`mod.rs:2267-2294`). `handle_endpoint_disconnect` runs at `shell_runtime.rs:558-611`.

**Reconnect** (`supervisor.rs:11-17, 258-316, 563-571`):
- Backoff is `500ms·2^(n-1)`, capped at 120 s. Remote attempt counters reset only after 60 s of stable readiness; local endpoints reset immediately on Online and cap at 30 s (`MAX_LOCAL_RETRY_DELAY`, `supervisor.rs:13, 274-307`).
- Attention-class errors are rescheduled every 30 s for non-local endpoints (`supervisor.rs:280-284`). The classifier (`saved.rs:155-180`) matches auth, host-key and version errors, but also any message containing "handshake" or "protocol" and the kinds `InvalidData` and `NotFound`. A slow-link "handshake response timed out" (`client/transport.rs:123`) therefore lands in the 30 s attention lane, not the backoff ladder.
- Suspend cancels in-flight attempts. Resume pulls pending attempts to "now", and delays are capped at 2 s for 30 s after an interactive resume (`supervisor.rs:186-188, 319-379`).
- Clicking the machine badge retries immediately (`supervisor.rs:406-418`).
- Stale events are dropped by generation checks (`registry.rs:173-177`).

**UI while disconnected:**
- The badge shows "◐ reconnecting" (`shell/endpoints.rs:162-163, 720-729`).
- The last frame stays frozen and dimmed, with a notice "<label> <msg>; reconnecting" (`shell_runtime.rs:516-534, 596-602`; `state.rs:211-224`).
- **Pane input is dropped, not buffered**, both while offline and during a pending activation ("Pane input and non-focus host effects do not cross the frozen handoff boundary", `shell_runtime.rs:800-848`).
- **Input typed during the undetected-dead window** (up to about 15 s for saved machines, about 60 s for `--remote`) is accepted, queued into writer, ssh and kernel buffers, and silently lost at teardown (`writer.rs:12-16, 186-256`; `shell_runtime.rs:558-611`).

**State recovery:**
- **Kept on the server:** PTYs and emulator state only. Per-client state is dropped on disconnect: `render_state`/`last_surface` baseline, surface interest and held inputs; tab sizing is re-run (`resize_tabs_for_only_shell_client`, `src/server/headless.rs:1028-1041, 2645-2650`).
- **Kept on the client:** the cached endpoint snapshot. It cannot be activated until the endpoint is Online again (`shell/endpoints.rs:206-212`).
- **Discarded on the client:** pending API requests (`endpoint_commands.rs:246-259`); scroll state (`shell/endpoints.rs:164-172`); endpoint graphics (`shell_runtime.rs:570-571`); any in-progress activation (`shell_runtime.rs:572-588`); the writer queue; the surface decoder, recreated per connection (`mod.rs:166-173, 1386`).
- **Resume mechanism:** none. No sequence-numbered resume or replay. Recovery is a full handshake, a new snapshot, then `surface.set(true)` and a full surface (`activation/protocol.rs:124-148`).

---

## 6. Latency characteristics

- **Every pane keystroke is a full round trip:** client → bridge → ssh → sshd → remote bridge → server; the server encodes the key for the PTY, the child echoes, ghostty marks rows dirty, the server builds a patch, and it travels back the same way for the client to apply and re-blit. There is **no local echo, prediction or typeahead** (grep for predict, local echo, typeahead, speculat, optimistic finds only unrelated hits).
- **Local work:** key classification, keybindings, prefix and navigate modes, and the chrome repaint (`client/mod.rs:924-944`; `client/shell/render.rs:31-120`). Only pane content waits for the network.
- **Input-side delays:** stdin escape disambiguation holds input for 10 ms when idle (`RAW_INPUT_IDLE_FLUSH_TIMEOUT_MS`); with mouse capture, partial escape and CSI sequences hold for 50-150 ms, or 500 ms for disambiguated mouse tails (`src/raw_input.rs:42-50, 435-468`; `client/input.rs:233-266`).
- **Client writer:** a dedicated thread batches frames up to 64 KiB, at most 256 queued batches, with a 2 ms `IO_POLL_INTERVAL` sleep on `WouldBlock` (`writer.rs:12-16, 28-117`). Same-pane events are merged (`client/shell.rs:83-104`).
- **Client reader:** event-driven. `EndpointReader` waits with `poll(POLLIN)` up to 100 ms (only to notice cancellation) and returns as soon as data arrives (`client/transport.rs:106-137`; `unix_common.rs:190-211`).
- **Server render cadence:** `MIN_RENDER_INTERVAL = 16 ms` (`src/app/mod.rs:39`; `src/app/runtime.rs:74-94, 142-153`). After a forwarded keystroke, a 32 ms interactive window allows up to 16 renders per 16 ms epoch to bypass the cadence (`server/headless.rs:170-171, 546-553, 600-602, 2446-2451`; `headless/render.rs:4-34`). PTY damage accumulates and is taken in one go per render (`headless.rs:555`).
- **Server backpressure:** each client has an unbounded control lane, an ordered lane and a **single-slot render lane** ("Capacity is one so slow clients cannot build lag", `client_transport.rs:122-127, 273-377`). If the slot is full, the render is deferred (`retained_surface.rs:594-597`) and re-rendered from current state when the writer claims the slot (`client_transport.rs:957-963`; `headless.rs:2651-2656`). Latest-state-wins applies only **before** enqueue: a frame in the slot is always written, and the slot is cleared only by `discard_pending_render` on surface deactivation, which also calls `request_repaint` and nulls the baseline (`client_transport.rs:319-355`; `src/server/headless/surface_interest.rs:33-47`). After writing, frames sit in kernel, bridge and ssh buffers the server cannot see.
- **Fixed-sleep polling** exists in three places: the local bridge download write (1 ms on `WouldBlock`, `attach.rs:32, 3523-3533`), the client endpoint writer (2 ms), and the bridge supervisor (50 ms ssh-exit check). Under contention this adds a few ms; on the happy path the reader and bridge upload are event-driven.
- **Compression:** `ssh -C` (zlib) is the only compression. A mux channel inherits the master's setting (`attach.rs:1421-1423`).
- **Not measured:** bytes per keystroke round trip. The `render_prof` counters exist but no data was gathered.

---

## 7. Gaps relative to a mosh-like experience

1. **TCP head-of-line blocking.** All panes of a machine travel in one framed stream over one SSH channel, and that machine's UI bridge, API bridge and probes share one ControlMaster TCP connection (`attach.rs:1428-1440, 3157-3166, 3259-3285`). Different machines use separate masters. One lost segment stalls every later frame on that machine, including fresh state that makes older frames obsolete. The latest-state-wins slot cannot help once bytes are enqueued.
2. **No roaming.** A new IP or NAT rebinding kills the TCP connection and every channel multiplexed on it.
3. **Slow dead-link detection for `--remote`.** No app-level health check (`registry.rs:157`), only ssh keepalive at about 60 s (`attach.rs:3181-3183`), and no stale-master probe, so it can attach to a dead master. Saved machines detect a dead link in about 15 s (5 s idle plus 10 s ping timeout, `health.rs:3-6`). The remote side of a dead `--remote` link has no idle watchdog either.
4. **Expensive reconnect.**
   - `--remote` always exits on disconnect (`errors.rs:62-71`; `mod.rs:2278-2283`), because `federated` is forced false in the remote client process (`mod.rs:235-243`).
   - A saved-machine reconnect runs a chain of ssh spawns (`ssh -G`, probe, possibly fresh probe and `-O stop`, `uname`, binary lookup, `status client --json`, bridge), several RTTs before the handshake even starts, then a full handshake, snapshot, activation and full surface (`saved.rs:20-57`; `attach.rs:877-971, 1583-1603`). If the master is gone, it also costs a full SSH key exchange and authentication.
   - The server discards the client's diff baseline on disconnect (`headless.rs:1028-1041`), so nothing can be resumed incrementally.
   - Backoff can grow to 120 s (`supervisor.rs:11-12`), and slow-link handshake timeouts are misclassified into the 30 s attention lane.
   - Saved bridges run in BatchMode; interactive auth (password or 2FA) is possible only out-of-band via `herdr machine reconnect`, which seeds the shared master (`attach.rs:768-815`; `cli/machine.rs:193-205`). Once the master's `ControlPersist` expires, that step must be repeated.
5. **No acknowledged-state diffing; desync is fatal.**
   - The server commits its diff baseline when a frame is enqueued, assuming delivery. There are two production call sites: `retained_surface.rs:591` (retained patch path) and `headless/render.rs:980` (full/delta/reuse path); `commit_sent_frame` is defined at `render_stream.rs:284`.
   - Patches form a strict `base == last, new == last+1` chain (`client/shell/surface_patch.rs:116-126`; `render_stream.rs:260-266`).
   - On a mismatched plain `PaneSurfacePatch`, the codec decoder sets `self.baseline = None` and passes the patch on; the client shell rejects it (`surface_reuse.rs:160-163`; `client/mod.rs:~1605`). The next `surface_reuse`/`surface_delta`/`surface_scroll` message then fails with "... without a baseline" (`surface_reuse.rs:120-122, 185-187, 199-201`), which the reader turns into `ServerDisconnected` (`transport.rs:62-69, 92-99`). Unless a full `PaneSurface` arrives first, a single desync ends `--remote` or forces a saved-machine reconnect. No resync request exists.
   - The protocol cannot tolerate loss or reordering, which a datagram transport requires.
6. **No prediction or local echo.** Every character waits a full RTT plus render time; nothing tracks keystroke epochs or confirms predictions. The server surface carries no PTY mode state to inform a predictor.
7. **Input is lost around outages.** It is dropped while offline and during activation (`shell_runtime.rs:800-848`), and silently lost when queued during the undetected-dead window.
8. **Fixed frame policy.** The 16 ms cadence and interactive burst do not adapt to the link; the only signal is the single render slot.
9. **Serial, per-connection ssh spawn.** The local bridge serves one connection at a time and spawns one `ssh` per connection (`attach.rs:3056-3068`); without a ControlMaster each is a full new SSH connection.

---

## 8. Natural seams for a mosh-like transport or prediction layer

### Transport seams (byte-stream substitutions; client and server protocol code unchanged)
- **Local socket indirection.**
  - The client only knows `HERDR_CLIENT_SOCKET_PATH`, read in `src/server/socket_paths.rs:9, 23-31` and set in `src/remote/attach.rs:3610-3629`.
  - `SshStdioBridge::start` (`attach.rs:90-98`) and `start_command` (`attach.rs:3030-3100`) could be replaced by a UDP bridge with the same "local socket ↔ remote" contract. Note the current accept loop is serial; a replacement should decide whether it needs concurrent or fast-reattach connections.
  - Saved-machine equivalent: `connect_saved_ssh` (`src/remote/saved.rs:20-57`), called from `connect_once` (`supervisor.rs:476-484`). A new `ConnectTarget` or endpoint transport kind could go there (`supervisor.rs:49-52`; `src/client/endpoint.rs:74-77`).
- **Remote socket indirection.** `run_remote_client_bridge` → `forward_remote_bridge_stdio` (`src/remote/host.rs:6-38`; `unix_common.rs:213-261`) is a pure pump. A `remote-udp-bridge` subcommand could sit next to it (`src/main.rs:565-567`), with ssh used only for bootstrap and key exchange (print a port and key, as mosh-server does). Remote command builder: `attach.rs:417-436`.
- **Limitation.** A byte-stream swap gives roaming and survival across reconnects, but not loss tolerance: a reliable stream over UDP still has head-of-line blocking. Real gains need the protocol changes below.

### Protocol and state seams (for SSP-like, loss-tolerant updates)
- **Server diff baseline.** Switch from "committed on enqueue" to "committed on client ack" at both production call sites, `src/server/headless/retained_surface.rs:591` and `src/server/headless/render.rs:966-980`, plus `commit_sent_frame` itself (`src/server/render_stream.rs:284`) and `request_repaint`/`reset_baseline` (`render_stream.rs:93-114`), which clear `last_surface`. The single render slot and deferral (`client_transport.rs:122-127, 319-331`; `retained_surface.rs:594-597`) give latest-state-wins scheduling to build on.
- **Revision chain.** Client checks at `src/client/shell/surface_patch.rs:113-139` (including popup, graphics, geometry, focus and snapshot-generation conditions) and server checks at `src/server/render_stream.rs:260-266` would relax to "diff from acknowledged revision N". Codec baselines: `src/protocol/surface_reuse.rs:118-162`; delta codec: `src/protocol/surface_delta.rs:60-209`.
- **Ack and other new messages.** Use `EndpointControl{kind,data}` (`wire.rs:612-617, 1500-1505`) and capability negotiation (`src/protocol/endpoint.rs:23-33, 142-154, 169-171`). New fields must be optional to keep generation 1 compatible (`endpoint.rs:1-15`).
- **Resync request.** The rejected-patch branch at `src/client/mod.rs:~1605` and the baseline-clearing branch at `surface_reuse.rs:160-163` are where an explicit resync request should replace the current "fail on next codec message" behaviour; server side at `render_stream.rs:93-114`.
- **Reconnect without full re-bootstrap.**
  - Client: the decoder is recreated per connection (`client/mod.rs:166-173, 1386`); activation starts at `src/client/endpoint/activation/protocol.rs:124-148`.
  - Server: per-client state is removed on disconnect (`src/server/headless.rs:1028-1041, 2645-2650`). A resume token needs the server to keep a detached client record (baseline, interest, geometry) alive for a grace period, not only the client to keep its decoder.
  - Bootstrap: `connect_saved_ssh` re-runs `ssh -G`, probes, platform and binary detection every attempt (`attach.rs:877-971, 1583-1603`); these can be cached.
- **`--remote` resilience.** The fatal-Local branch at `src/client/mod.rs:2278-2283`, the forced-empty catalog at `mod.rs:235-243`, and health gating at `src/client/endpoint/registry.rs:157-158` are where `--remote` could get reconnect and health treatment like saved machines. Also call `prepare_shared_transport` in `run_remote` and consider enabling the remote idle watchdog for interactive bridges (`attach.rs:3016-3022`).

### Prediction and local-echo seams (client side)
- **Input emission.** Pane keys become `ClientPaneInputEvent` at `src/client/shell/input.rs:1174-1190` and are batched at `src/client/shell.rs:69-119`. A predictor would record a keystroke epoch there and tag the batch via a new `EndpointControl` or optional field.
- **Retained grid to predict against.** The client keeps `self.pane_surface` (`src/client/shell/surface_patch.rs:113`) plus the cursor from patches. A predicted-cell overlay belongs between `apply_pane_surface_patch` (`client/mod.rs:1582-1623`) and `shell.compose`/`present_frame` (`client/state.rs:511-595`). The surface is a tab composite, so predictions must map through `PaneSurfacePane.inner_rect`; the server would also need to expose PTY mode hints (echo, alt-screen, keyboard protocol) for a mosh-style predictor to decide when to predict.
- **Confirmation signal.** Server input handling at `src/server/headless.rs:2446-2451` (where `note_interactive_input` fires) could stamp the latest applied input epoch into the next `PaneSurfacePatch` or surface metadata, as mosh does with its echo ack.
- **Offline typeahead.** The drop guards at `src/client/shell_runtime.rs:800-848` (offline and pending activation) are where typed input would be buffered (bounded) and displayed predictively. Writer-queued input lost at teardown (`writer.rs:186-256`) would need replay keyed on server-acked epochs.
- **Adaptive frame rate.** `MIN_RENDER_INTERVAL` (`src/app/mod.rs:39`) and the interactive budget (`src/server/headless.rs:169-171`; `src/server/headless/render.rs:4-34`) could be scaled by an RTT estimate fed back from client acks.

---

**Open items:**
- Bytes per keystroke and per-frame sizes were not measured. The `render_prof` counters exist for this (`retained_surface.bytes`).
- The Windows bridge and named-pipe paths were not traced (`src/platform/windows.rs:344`).
- The "Cloud" endpoint mentioned at `src/protocol/endpoint.rs:5` has an unknown transport.

---

## Verification notes

Corrections applied after review. The items marked "re-checked" were confirmed in source before the change.

1. **Diagram sides fixed.** `SshStdioBridge`, the ssh client and the ControlMaster run locally; only sshd, the login shell, `remote-client-bridge` and the server are remote (`attach.rs:90-98, 3255-3277`).
2. **Serial accept loop** (re-checked, `attach.rs:3056-3068`): `bridge_connection` runs inline on the listener thread. It is one ssh per connection, in sequence.
3. **Remote command path** is a default; discovered absolute paths replace it, and `--session` is added only for non-default sessions. The shell is the login shell, not necessarily `sh`. `saved_bridge_command` is a separate setup-only command.
4. **`prepare_shared_transport` scope** (re-checked via grep): saved-machine paths only; `run_remote` never calls it. The full five-step sequence is documented, and the ControlPath-ownership and dev/ino pinning details were added.
5. **Interactive auth** is not impossible for saved machines: `herdr machine reconnect` seeds the shared master.
6. **ControlMaster per target**, not shared across machines (`unix_common.rs:713-760`).
7. **Bridge I/O details:** download goes through an 8 KiB `BufReader`; upload is event-driven via poll wake, not timer-polled.
8. **ControlMaster semantics:** bridge children are mux clients; the master owns TCP and keepalive and persists 600 s.
9. **Mismatched patch handling** (re-checked, `surface_reuse.rs:160-163`): the decoder clears its baseline rather than dropping the patch, so the next codec message is a fatal decode error. It is not a silent drop.
10. **Client reader** is poll-based (re-checked, `client/transport.rs:106-137`), not 1 ms polling. Fixed sleeps are in the bridge download write (1 ms), the endpoint writer (2 ms) and the bridge supervisor (50 ms).
11. **`commit_sent_frame` seam** (re-checked): a second production call site at `headless/render.rs:980`, definition at `render_stream.rs:284`.
12. **Granularity table** split into the full-recompute path (delta → reuse → full) and the retained path (patch, scroll). `surface_reuse` is metadata-only and applies when the grid is unchanged.
13. **Federated under `--remote`** (re-checked, `client/mod.rs:235-243`): the catalog is never loaded, so `--remote` failures are always fatal.
14. **Attention-lane citation** moved to `supervisor.rs:280-284` (re-checked). The classifier's broader match, including "handshake", was added.
15. **Keepalive sources:** saved machines pass ServerAlive on the CLI (overrides user config); `--remote` relies on the managed file; the master runs keepalive.
16. **Added facts:** server drops per-client state on disconnect (re-checked, `headless.rs:1028-1041`); no remote idle watchdog for `--remote`; input lost in the undetected-dead window; input dropped during activation (re-checked, `shell_runtime.rs:846-848`); render slot is not droppable once enqueued; surface is a tab composite with no PTY mode state; full patch-acceptance conditions (re-checked, `surface_patch.rs:113-126`); automatic re-activation after reconnect; local vs remote attempt-counter reset; three managed-config entry points; socket-path fallbacks and API socket name; stderr handling; Windows scp install; no `-A`/ForwardAgent (re-checked via grep); the remote bridge's half-close semantics; the number of ssh spawns per saved connect attempt.
