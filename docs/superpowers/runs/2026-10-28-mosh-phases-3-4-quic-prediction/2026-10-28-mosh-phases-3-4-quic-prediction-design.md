# Mosh Phases 3 and 4: QUIC leg, cancel-on-supersede, predictive echo

## Goal

A herdr remote session can run over a QUIC leg that survives a network switch or a long suspend:
a switch or a gap under 30 s shows the next echo within 2 RTT of the link returning on the same
connection, and a longer suspend within grace redials QUIC first (ssh only if that QUIC attempt itself
fails) within 3 RTT plus one reconnect
backoff step, plus up to the 3 s handshake bound of an attempt already in flight when the link
returns. Under 10 % packet loss, typing and output keep flowing instead of
stalling behind lost render frames. With prediction enabled at 150 ms one-way delay, most printable keys
appear locally within 5 ms, and wrong predictions are cleared within one RTT plus mosh's 50 ms echo delay and one render (refined in the Phase 4 design).

Builds on Phases 1 and 2 (`docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/`,
`docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/`, code in `src/mosh/`).
Vision: `HERDR_MOSH_VISION.md` §4.2 and §5, rows 3a, 3b and 4.

## Problem description

Phases 1 and 2 made the session resumable and paced, but every byte still rides one ssh TCP
stream. A network switch kills the TCP connection, so the session pays a new ssh handshake
(several RTTs, plus auth) before it resumes. Under packet loss, TCP head-of-line blocking delays
everything behind a lost segment, including the frame that would show the result of a keystroke.
And every keystroke waits a full RTT before the user sees it. The three phases address one cause
each: QUIC replaces TCP for the leg (3a), render frames stop blocking each other (3b), and the
client predicts its own echo (4).

## Main challenges

- **The render chain is strict.** `PaneSurfacePatch` and the `endpoint.surface-reuse/delta/scroll.v1`
  frames each extend the client's retained surface by one revision. A missing reuse, delta or scroll
  frame makes the client's transport decoder return `InvalidData`, which disconnects the client
  (`src/client/transport.rs:94-100`). There is no resync request today; recovery is a later full
  `PaneSurface` (`ServerMessage` tag 13). So a superseded frame can only be dropped together with every
  frame that depends on it, and the client then needs a full frame.
- **The session layer is a byte stream.** Phase 1's `SessionEndpoint` replays bytes by offset after a
  reconnect. Dropped render frames must not be replayed, but everything else must be.
- **Frozen protocol.** `ServerMessage` and `ClientMessage` are frozen; new behaviour rides
  `EndpointControl` kinds and advertised capabilities (AGENTS.md "Stable client endpoint contract").
  Render frames also travel as tag-20 `EndpointControl`, so classifying a frame needs its kind string,
  not just the tag byte.
- **Fork footprint.** The server needs to know about resync (3b) and echo acknowledgement (4), and the
  client needs to paint predictions and stop the patch fast path from overwriting them. Every one of
  those is an upstream hook.
- **Testing without root.** Phase 2's harness emulates delay and rate on byte streams; QUIC needs a
  userspace UDP relay that adds delay, loss and address changes.

## Key decisions made

The QUIC leg is a second `LegDialer` implementation behind the existing seam
(`src/mosh/ssh_leg.rs:171`); the session layer, the replay ring and the adaptive window are unchanged
and run over one reliable QUIC stream. Supersede (3b) is done in the fork's transport, not the server:
the keeper classifies each server frame, sends render frames on their own QUIC streams and resets a
stream when a later self-contained frame supersedes it; the local bridge drops every chain-dependent
frame after a gap and asks the server for a full frame through one new `EndpointControl` kind, which
needs a small server hook (3–4 lines, 3b §5). The vision's server-side ring of sent surfaces with an acked base is
not built: forcing a full frame achieves the same "no stall" outcome with one hook instead of a server
rewrite (recorded below as a parked follow-up). Prediction (4) runs in the client and draws over the
retained surface; the server reports per-pane echo progress as `x-mosh.echo-ack.v1` through hooks on
its input path and its main-loop deadline (Phase 4 §2). The upstream hook budget rises from 20 to 30 lines for these phases.

## Decision points, by section

### 1. QUIC leg (Phase 3a): a second `LegDialer` (chosen)

**Recommended:**
- **Keeper side.** The keeper opens one UDP listener per keeper process and runs a `quinn` endpoint on
  its own tokio runtime thread. It binds a dual-stack wildcard (`[::]` with `IPV6_V6ONLY` off, falling
  back to `0.0.0.0`). Each QUIC connection is a leg. The keeper's existing per-connection leg
  handling (HELLO/WELCOME/DATA/ACK/HEARTBEAT/END) runs over one bidirectional QUIC stream adapted to
  blocking `Read`/`Write`.
- **Bootstrap over ssh** (3a §1). Only when started with `--x-mosh-quic-v1`, the attacher, which owns
  the ssh stdio, asks the keeper for `port`, the certificate's SHA-256 fingerprint and a 32-byte token
  (minted per query). It sends the query (`Q`) on its own short-lived rendezvous connection, never on
  the session connection: it reads the reply with a 2 s timeout and closes that connection, and EOF,
  an error or the timeout all mean `none` (an older keeper that closes on `Q` is just `none`). It then
  writes exactly one line after the ready marker and before any session bytes:
  `herdr-mosh-quic:1 <port> <fingerprint> <token>`, or `herdr-mosh-quic:1 none` when the keeper's UDP
  bind failed or the query gave no reply. With the flag, the attacher opens its session rendezvous
  connection lazily, on the first byte from stdin (the HELLO), so a late reply can never reach the
  session stream and a standby ssh leg holds no keeper connection while QUIC is tried (3a §1).
  The certificate is self-signed (`rcgen`) and lives as long as the keeper. The token binds the QUIC
  leg to the session: the first message on the stream is the existing HELLO, carrying it as
  `quic_token`.
- **Local side** (3a §3, §6). The ssh leg's dialer waits for that one line (a 5 s safety timeout counts
  as `none`). `QuicLegDialer` dials the `hostname` that `ssh -G <target>` reports, in the address
  family it resolves to; when `ssh -G` shows a `proxyjump` or `proxycommand`, QUIC is skipped for the
  session (`start_ui_bridge` does not wrap the ssh dialer). It pins
  the fingerprint and hands the bridge a `Leg` whose reader and writer wrap the stream. The ssh child
  stays alive until WELCOME arrives over QUIC, so any QUIC failure before WELCOME on leg 0 continues on
  the live ssh leg. There the bridge sends a leg-0-style HELLO with a fresh session token and
  `previous_token` set to the QUIC attempt's session token, so Phase 1's ghost eviction
  (`previous_token` in `keeper.rs`) removes a QUIC session the keeper may already have admitted.
- **Roaming** (3a §4, §6, §7). The client socket binds the wildcard address, so QUIC connection
  migration carries the leg across an address change; there is no error-triggered
  `Endpoint::rebind`. `max_idle_timeout` is fixed in `TransportConfig` on both peers before the dial:
  the keeper advertises 3600 s and the client its grace, so the negotiated value is the client's grace.
  Grace itself stays a keeper application timer. QUIC legs carry a 30 s silence window (a
  silence-timeout override on `Leg` and `KeeperLeg`): a gap under it resumes on the same connection, a
  longer gap ends the leg and redials QUIC (§6 3a).
- **Fallback** (3a Key decisions, §6). Fallback is per dial, not sticky. Leg 0 dials QUIC only after a
  bootstrap line that is not `none`; if the handshake does not complete within 3 s or QUIC fails before
  WELCOME, the live ssh leg carries the session. Reconnect legs try QUIC first with the cached bootstrap,
  then ssh in the same dial, and ssh legs refresh the bootstrap. Once a QUIC leg has reached WELCOME
  with the cached token (the token is bound), a refresh updates only port and fingerprint and keeps
  that token; before the first binding a refresh replaces the token too (3a §2). A capability `x-mosh-quic-v1` in
  `status client --json` gates the attempt.
- **Dependencies.** Under a `cfg(unix)` target table: `quinn` and `rustls` with
  `default-features = false` (`quinn`: `runtime-tokio`, `rustls-ring`; `rustls`: `ring`, `std`,
  `tls12`), and `rcgen` with `ring`. ring, never aws-lc-rs: `cargo tree -i aws-lc-rs` (and
  `aws-lc-sys`) must come back empty. tokio is already a dependency with `net` reachable.

**Considered:** a QUIC leg that replaces the session layer (QUIC's own reliability instead of Phase 1's
offsets and replay). Rejected: a new QUIC connection after a long suspend loses QUIC's stream state,
and Phase 1's replay is what makes resume exactly-once. Mosh-style raw UDP with its own SSP: rejected,
QUIC gives encryption, congestion control and migration for free.

### 2. Cancel-on-supersede (Phase 3b): fork-side classification, one server hook (chosen)

**Recommended:**
- **Classification** (keeper, server→client direction). Each server frame is classified by its bincode
  tag and, for tag 20, its kind string:
  - **self-contained render:** `PaneSurface` (13). (`ClientShellSnapshot`, tag 12, is never sent live:
    the snapshot travels as EndpointControl `shell.snapshot.v1` on the control lane, so it is
    reliable.)
  - **chained render:** `PaneSurfacePatch` (19), `EndpointControl` kinds `endpoint.surface-reuse.v1`,
    `endpoint.surface-delta.v1`, `endpoint.surface-scroll.v1`;
  - **everything else:** reliable, in order, on the session stream as today.
- **Streams.** Render frames go on their own unidirectional QUIC streams, each prefixed with a render
  sequence number and its class. A frame becomes superseded when a later self-contained render frame
  for the same client is enqueued; the keeper resets every older render stream still in flight.
  Chained frames are never reset on their own: they are only reset together with everything up to a
  later self-contained frame. Older render streams send first: each stream's priority decreases with
  its sequence number, and send fairness is off for render streams where quinn allows it (3b §2).
- **Ordering and gaps** (local bridge). The bridge delivers render frames to the client in render
  sequence order, interleaved with session-stream frames by arrival. When a render frame is missing
  (its stream was reset, or it is overdue by more than `max(SRTT, 50 ms)` after a later one arrived;
  3b §3), the bridge drops every later chained frame until the next self-contained frame, so the
  client's decoder never sees a broken chain. Then it sends one `EndpointControl` kind
  `x-mosh.surface-resync.v1` towards the server, once per gap and not for a supersede reset. Frames
  the keeper itself fails to deliver (leg down, lane overflow, leg change) are covered by its chain
  gate (3b §2).
- **Server hook** (3b §5). On `x-mosh.surface-resync.v1` the server resets that client's transport
  baseline, discards its pending render and requests a repaint, so the next frame is a full
  `PaneSurface`. The dispatch at `src/server/client_transport.rs:1349` maps the kind to a new
  `ServerEvent` variant handled by one arm in `headless.rs` (3–4 hook lines). Unknown kinds are
  already ignored, so an older server is safe: the bridge then just waits for the next naturally sent
  full frame.
- **Capability** (3b §1). `x-mosh-supersede-v1` in `status client --json`. When the probe shows it,
  the bridge sets `render_split` in every HELLO, and the keeper fixes the session's flag from the HELLO
  that answered `new`. Render frames ride a lane only on legs that carry one; on ssh legs they stay on
  the session stream.
- **Resume.** Render frames are not in the session replay ring; on every admitted leg after the first
  the bridge sends `x-mosh.surface-resync.v1` once after WELCOME (3b §4).

**Considered:**
- The vision's protocol-level surface acks: a server-side ring of sent surfaces with an acked base and
  a relaxed chain. It removes even the resync round trip, but rewrites the server's render commit
  path (`render_stream.rs:284`, `retained_surface.rs:591`), far beyond one-statement hooks. Parked as
  a follow-up, only if measurements show the resync round trip matters.
- Dropping frames with no resync: rejected, the client's surface stays stale until something else
  triggers a full frame.

### 3. Predictive local echo (Phase 4): client overlay plus a server echo signal (chosen)

**Recommended:**
- **Predictor** (client, fork module). Watches outgoing `ClientShellPaneInput` key events for printable
  text, places predicted cells at the pane's cursor (mapped through `PaneSurfacePane.inner_rect`),
  and tracks them in epochs as mosh does: content-only validation against the retained surface, an
  epoch ends on a non-printable key, a confirmed wrong prediction clears the epoch, paste and resize
  reset. `on` shows predictions whenever the link reports SRTT, `adaptive` only when SRTT > 60 ms;
  both underline when SRTT > 160 ms (Phase 4 §3).
- **SRTT source.** The client reads `LinkStatus.srtt_ms`, delivered per endpoint through the existing
  link-frame observe hook (Phase 4 Key decisions). Over the plain bridge (no mosh), prediction stays
  off.
- **Echo signal** (Phase 4 §§1–2). The unit is one element of `ClientShellPaneInput.events`, counted at
  the client's input tap and at the server's receipt (client_transport.rs:1196). Counts are per
  (server client connection, pane). The bridge, not the client, injects `x-mosh.echo-subscribe.v1`;
  the server creates the client's `EchoTracker` entry on the first subscribe, and a repeated subscribe
  (replayed on a new leg) is idempotent and does not reset counts. The client resets on a new
  connection generation, which is exactly when the server side is a new client connection. Acks are
  deadline-driven: input received at least 50 ms earlier is acked as `EndpointControl` kind
  `x-mosh.echo-ack.v1` `{pane_id, input_seq, surface_revision}` on the control lane (session stream,
  `FrameClass` `Reliable`). While a render for that client is pending or deferred at the deadline, the
  ack waits for a later tick; it uses the last committed `surface_revision` only when nothing is
  pending (Phase 4 §2). The client judges a prediction only against a retained surface at or above
  that `surface_revision`, and clears a prediction no ack covers after max(1 s, 4×SRTT).
- **Painting.** Predictions are painted in the existing composition hook (the outage banner's paint
  function is extended, no new line), and one hook line makes the patch fast path decline while any
  prediction exists, hidden ones included.
- **Flag.** `HERDR_MOSH_PREDICT` is `off` (the default), `on` or `adaptive`.

**Considered:** a client-side terminal emulator to predict (mosh's way). Rejected by the vision: the
retained surface is already on the client.

### 4. Upstream footprint (chosen: budget 30)

**Recommended:** the hook budget rises from 20 to 30 for these phases (the vision set 20 "through
Phase 2"). Expected new hooks:
- `status.rs` capability lines for `x-mosh-quic-v1`, `x-mosh-supersede-v1` and `x-mosh-echo-v1`;
- the server resync dispatch, its `ServerEvent` variant and `headless.rs` arm (3b §5);
- the server echo hooks: input count, `echo_due`, `echo_deadline`, client disconnect (Phase 4 §4);
- the client fast-path guard and the client input tap for the predictor.

Each is one statement tagged `// mosh-fork hook`; the running total is about 25 of 30 (Phase 4 §4).
`scripts/mosh-diff-budget.sh` gets the new limit.

**Considered:** keep 20. Rejected: it forces 3b or 4 out of scope, and the vision deliberately
scoped the 20-line limit to Phases 1 and 2.

### 5. Test harness: a userspace UDP relay (chosen)

**Recommended:** a Rust UDP relay in `tests/mosh_harness.rs` between the local dialer and the keeper's
UDP port, with one-way delay, a seeded random loss rate and a rebind operation (change the relay's
local port mid-session to emulate an address change). Opt-in timing tests follow Phase 2's policy:
`#[ignore]`, run with `--run-ignored all`, take `timing_lock()`, and use a dense flood
(`base64 /dev/urandom` at a large pane size). Herdr sends screen diffs, so a `yes` flood does not load
the link.

### 6. Acceptance (agent-set bounds, measured not tuned)

- **3a:** a relay rebind mid-session, and an idle gap under the 30 s QUIC silence window, both resume on
  the same QUIC connection and show the next keystroke's echo within 2 RTT of the link returning. A
  longer gap within grace (up to 10 min) redials QUIC first (ssh only if that QUIC attempt itself
  fails) within 3 RTT plus one backoff step
  plus up to the 3 s handshake bound of an attempt already in flight when the link returns
  (coverage round 1; design roast r1). Fallback: with UDP blocked, the session comes
  up over ssh within 3 s plus the ssh time.
- **3b:** at 10 % loss and 150 ms one-way delay, typing produces visible output within 2 s, and
  `yes`-style output keeps advancing. Measured against the same profile over the ssh leg.
- **4:** at 150 ms one-way delay with prediction on, the median local echo of printable keys in a
  shell is under 5 ms, and a wrong prediction is cleared within 1 RTT + 50 ms + one render (Phase 4 design).

### 7. Constraints held

- New code under `src/mosh/`.
- Upstream edits are one-statement hooks, at most 30 lines.
- No change to frozen codecs or `PROTOCOL_VERSION`; new behaviour only through `EndpointControl` kinds
  and advertised capabilities.
- New Cargo dependencies only for QUIC.
- Tests only in fork files.
- No AI co-author lines in commits.

### 8. Out of scope

- The server-side surface ring with acked base (§2 Considered).
- QUIC 0-RTT.
- Windows support for the QUIC leg (unix only, like Phase 1).
- Prediction for non-printable editing keys beyond mosh's rules.

## Key abstractions

**QuicLegDialer** — new struct, the second `LegDialer` implementation: a QUIC leg with ssh bootstrap and ssh fallback (`src/mosh/quic_leg.rs`).
- Owns:
  - the client quinn endpoint and its tokio runtime thread;
  - the cached `QuicBootstrap` (leg 0's, refreshed by ssh legs) and per-dial fallback (3a §6);
  - the dial address: `ssh -G`'s `hostname`, in the family it resolves to (3a §3);
  - the 3 s handshake bound;
  - fingerprint pinning;
  - the wildcard-bound client endpoint: roaming by QUIC migration, no error-triggered `Endpoint::rebind` (3a §6);
  - classification of QUIC leg ends (3a Key abstractions).
- Hides: QUIC. The bridge sees a `Leg` and never learns about migration, rebinds or fallback.
- Must not: build ssh commands or read ssh preambles. It composes an `SshLegDialer` for the leg-0 bootstrap and for every ssh leg. It must not touch session bytes or the give-up deadline (still the `LocalBridge`'s, Phase 1).
- Relationships: implements `LegDialer`; composes `SshLegDialer` (delegation); uses the `quic` module and `QuicBootstrap`. Dependencies point `local_bridge` → trait ← `quic_leg` → `ssh_leg`.
- Lifecycle: built by `start_ui_bridge` and owned by the `LocalBridge` behind the existing `Arc<Mutex<Box<dyn LegDialer>>>`. Dropping it shuts the runtime down. On leg 0 it dials ssh and waits for the one bootstrap line. It returns a QUIC leg that keeps the live ssh leg as its standby until WELCOME arrives over QUIC (the ssh child is torn down then), or that same ssh leg as the fallback, so fallback costs no second ssh handshake (3a §6).
- Errors:
  - Leg 0: any QUIC failure before WELCOME (timeout, UDP blocked, fingerprint mismatch, a `none` line, token refusal, pre-WELCOME bound, early loss) continues on the live ssh leg and is never surfaced as a `LegError`. The bridge's HELLO there carries a fresh session token with `previous_token` = the QUIC attempt's token, so ghost eviction removes a QUIC session the keeper already admitted (3a §6).
  - Reconnect legs: any QUIC failure, a fingerprint mismatch included, falls through to an ssh leg in the same dial (`Retryable`), never `Fatal`.
  - A keeper `END` keeps Phase 1's classification.

**QuicBootstrap** — new value type, the single definition of `herdr-mosh-quic:1 <port> <fingerprint> <token>` (and `herdr-mosh-quic:1 none`) and of the keeper's rendezvous answer that produces it.
- Owns: encoding (attacher side), parsing (dialer side), validating the fields, and the 32-byte token type.
- Must not: do I/O.
- Relationships: shared by Attacher, Keeper and `QuicLegDialer`, like `LinkStatus`.
- Errors: a `none` line, a malformed line, or no line within the dialer's 5 s safety timeout means "no bootstrap", which the dialer turns into the ssh fallback.

**quic module** — new module of QUIC plumbing shared by both ends (`src/mosh/quic.rs`).
- Owns:
  - the blocking `Read`/`Write` adapter over quinn streams, driven from a runtime handle;
  - the TLS configs: rcgen self-signed certificate plus SHA-256 fingerprint on the keeper, a pinning verifier on the client, ring only;
  - the render-stream header (render sequence number plus `FrameClass`).
- Hides: async/blocking bridging and rustls setup from the std-thread code.
- Must not: know session or HELLO semantics, or decide which frames go on render streams.
- Relationships: used by `QuicLegDialer`, `QuicAcceptor`, `RenderLane` and the bridge's render-lane reader.

**KeeperLeg** — new struct, the keeper's transport-neutral accepted leg: reader and writer halves, a shutdown handle callable from another thread (stealing, the silence verdict, session end), an optional `RenderLane`, and an optional silence-timeout override (the 30 s QUIC silence window).
- Owns: what `admit`/`run_leg`/`leg_writer`/`LegSlot` take today from a concrete `UnixStream`.
- Hides: whether the leg is a rendezvous Unix connection or a QUIC stream.
- Must not: carry session state.
- Relationships: two producers (the Unix accept loop and `QuicAcceptor`) feed the one `admit` → `run_leg` handler.
- Lifecycle: per accepted connection, owned by its `run_leg` thread; the `LegSlot` keeps the shutdown handle. Dropping it closes the stream, and for QUIC the connection.

**QuicAcceptor** — new struct, the keeper's UDP endpoint: one per keeper process, on its own tokio runtime thread.
- Owns:
  - the dual-stack wildcard bind, certificate, port and fingerprint (answering the bootstrap query through the keeper);
  - minting and checking tokens (per query, bound to the session at the first QUIC HELLO; the client keeps a bound token across ssh-leg bootstrap refreshes; 3a §2);
  - accepting connections and their first bidirectional stream;
  - the pre-WELCOME bound;
  - the transport config: `max_idle_timeout` advertised as 3600 s before any dial, so the client's advertised grace wins (3a Key abstractions); grace stays a keeper application timer;
  - building a `KeeperLeg` with a `RenderLane` and the 30 s silence-timeout override.
- Hides: QUIC admission. A bad or missing token is refused here, so `admit` stays transport-blind.
- Must not: apply the HELLO table, steal legs, or touch the token table beyond the token check.
- Lifecycle: started lazily by the first bootstrap query (sent on its own rendezvous connection, never the session connection; 3a §1); dies with the keeper process.
- Errors: a refused token closes the connection with an END.

**FrameClass** — new closed enum `SelfContained | Chained | Reliable`, plus the one function that classifies a whole herdr frame (`link_frames.rs`, next to `FrameSplitter`).
- Owns: spec §2's table:
  - tag 13 → `SelfContained`;
  - tag 19 and the tag-20 kinds `endpoint.surface-reuse/delta/scroll.v1` → `Chained`;
  - everything else, every other tag-20 kind included (`shell.snapshot.v1`, `x-mosh.echo-ack.v1`) → `Reliable`.
  It decodes the bincode tag, and the kind string for tag 20.
- Hides: the frozen wire layout from the keeper and the bridge.
- Must not: decide what happens to a class.
- Errors: an unparseable frame is `Reliable`, never dropped.

**RenderLane** — new struct, the keeper-side render sender for one QUIC leg.
- Owns: render sequence numbers; one unidirectional stream per render frame with its header; the in-flight set; supersede. A `SelfContained` frame resets every older in-flight stream; `Chained` frames are only reset with that batch.
- Hides: QUIC stream reset.
- Must not: classify frames, touch the ring or the `InflightWindow`, or buffer while the leg is down (the keeper's `ChainGate` drops render frames then; on an ssh leg they ride the session stream; 3b §2).
- Relationships: owned by a `KeeperLeg`; fed from the session's `server_reader` through a channel into the keeper runtime. Concrete, QUIC only.
- Lifecycle: per leg; a new leg starts a fresh sequence.

**RenderReorder** — new struct, the bridge-side render ordering and gap state machine; sans-IO, time as an input.
- Owns:
  - in-order delivery by render sequence;
  - gap detection (a reset stream, or a frame overdue by more than `max(SRTT, 50 ms)` after a later one arrived; 3b §3);
  - dropping `Chained` frames after a gap until the next `SelfContained` frame;
  - the once-per-gap `x-mosh.surface-resync.v1` decision.
  Each leg starts in the gap state, which covers spec §2 "Resume".
- Hides: supersede and chain rules from `LocalBridge`.
- Must not: do I/O, encode frames, or interleave with session-stream frames. `LocalBridge` interleaves by arrival at `FrameSplitter` boundaries.
- Lifecycle: created per leg by `LocalBridge`, only when the `Leg` brings a render lane; single owner on the leg I/O thread.
- Errors: none; a gap is a state.

**Predictor** — new client fork module (`src/mosh/predict.rs`, mounted in the client), with state per endpoint and per pane.
- Owns:
  - the `HERDR_MOSH_PREDICT` mode (`off | on | adaptive`, a closed enum, read once);
  - the input tap: printable text from `ClientShellPaneInput`, and the per-pane count of `events` elements;
  - predicted cells placed at the cursor through `inner_rect`;
  - epochs and mosh's rules: content-only validation against the retained surface at paint time, judged only once an ack covers the prediction and the surface revision is at or above the ack's `surface_revision`; an epoch ending on a non-printable key, clearing on a confirmed wrong prediction, reset on paste or resize;
  - consuming `x-mosh.echo-ack.v1`, and clearing a prediction no ack covers after max(1 s, 4×SRTT) (Phase 4 Key abstractions);
  - the thresholds (`on`: whenever SRTT is reported; `adaptive`: above 60 ms; underlined above 160 ms);
  - the "any prediction exists" answer the fast-path guard reads.
- Hides: the prediction rules from every client hook.
- Must not: mutate the retained surface or the decoder baseline (it paints an overlay only); measure RTT (the bridge supplies SRTT); send anything to the server.
- Relationships: reached only through the fork's client hooks. The existing `mosh_outage::observe`/`paint` become the fork's client facade: a closed `match` on kind routes `x-mosh.link.v1` → `OutageState` and `x-mosh.echo-ack.v1` → `Predictor`, and paints both overlays. Two new hooks: the input tap and the fast-path guard.
- Lifecycle: process-global behind a mutex, like `OutageState`; state is keyed by (endpoint, connection generation), so a new generation resets it (Phase 4 Key abstractions).
- Errors: a malformed echo-ack is consumed and ignored.

**EchoTracker / server hooks** — new fork module for the server side (`src/mosh/server_hooks.rs`), the bodies behind the server hook statements.
- Owns:
  - the resync hook: `x-mosh.surface-resync.v1` resets the client's baseline, discards its pending render and requests a repaint, through a `ServerEvent` arm (3b §5);
  - the subscription: the first `x-mosh.echo-subscribe.v1` creates the client's entry; a repeated one is idempotent and keeps the counts;
  - the received `events` count per (client connection, pane), counted on receipt;
  - the deadline-driven emission of `x-mosh.echo-ack.v1 {pane_id, input_seq, surface_revision}` for input received at least 50 ms earlier (Phase 4 §2).
- Hides: echo bookkeeping from `client_transport.rs` and `headless.rs`.
- Must not: put the echo-ack in the latest-wins render slot. It goes on the control lane (session stream, `FrameClass` `Reliable`); ordering against renders comes from `surface_revision`, not from the wire.
- Lifecycle: a process-global map keyed by client, with entries created on the first subscribe and removed on client disconnect (`remove_client`).

**UdpRelay** — new test helper (`tests/mosh_harness.rs`): a userspace UDP relay with one-way delay, seeded loss and `rebind`; test-only, beside the Phase 2 `link` knob. The fake-ssh wrapper rewrites the bootstrap port to the relay's port, so production code gets no address override.

**Extended:**
- **Leg** — optionally carries a render-lane receiver (`None` for ssh), taken like the reader and writer. Its end report gains a QUIC close reason in place of an `ExitStatus`. It also gains an optional silence-timeout override (30 s on QUIC legs) that `LocalBridge` honours, an optional opaque admission token copied into HELLO, and, on a leg-0 QUIC leg, the live ssh leg as a standby that the bridge switches to if the QUIC leg ends before WELCOME and drops once WELCOME arrives (3a §6). It still never interprets bytes.
- **SshLegDialer** — in bootstrap mode it adds `--x-mosh-quic-v1`, waits for the one bootstrap line after the ready marker (5 s safety timeout, treated as `none`) and exposes the parsed `QuicBootstrap`; `none` or malformed means none.
- **start_ui_bridge** — still the single registration point. It wraps `SshLegDialer` in `QuicLegDialer` when the probe shows `x-mosh-quic-v1`, `HERDR_MOSH_QUIC` is not `0`, and `ssh -G` shows no jump (3a Key abstractions).
- **Keeper**:
  - a bootstrap query, answered on any rendezvous connection whose first byte is `Q` (a closed match on the first byte, not a new `SessionMsg` variant); the attacher uses a dedicated query connection, so the reply never shares a stream with session bytes (3a §1, §5);
  - legs arrive as `KeeperLeg`;
  - a per-session render-split flag, fixed from the HELLO that answered `new` and independent of the leg type (3b §1). When set, `server_reader` runs `FrameSplitter` + `FrameClass` + `ChainGate`: `Reliable` frames go to the ring; render frames go to the current leg's `RenderLane`, to the ring on an ssh leg, or are dropped while down (3b §2).
- **Attacher** — only when started with `--x-mosh-quic-v1`: after the ready marker and before any session bytes, it queries the keeper on its own short-lived rendezvous connection (2 s reply timeout; EOF, error or timeout → `none`; then closed) and writes exactly one line, the `QuicBootstrap` line or `herdr-mosh-quic:1 none`. It opens the session rendezvous connection lazily, on the first byte from stdin (the HELLO).
- **LocalBridge** — reads the leg's render lane into `RenderReorder`; interleaves delivered frames into the client output without offsets (not acked, not replayed); injects the resync and echo-subscribe `ClientMessage` frames upstream at a client frame boundary, which needs a client→server `FrameSplitter`.
- **InflightWindow** — also keeps a smoothed RTT (SRTT); on QUIC legs the `Leg`'s RTT probe supplies the connection's own estimate (3a §6).
- **LinkStatus** — `Linked` gains an optional `srtt_ms`, re-injected only on meaningful SRTT change, and `echo` (Phase 4); this is how SRTT and echo support reach the right client endpoint.
- **FrameSplitter / link_frames** — holds `FrameClass` and the resync frame encoder next to the link-frame encoder.

**Variation:**
- Leg transport (ssh, QUIC) → the `LegDialer` trait, with the kind chosen once in `start_ui_bridge`.
- Keeper leg sources → `KeeperLeg` producers feeding one `admit`.
- Render frame kinds are fixed → `FrameClass`, a closed enum.
- Fork client EndpointControl kinds are fixed → a closed `match` in the client facade.
- Prediction mode is fixed → an enum in `Predictor`.

**Walk-through.**
- Setup:
  1. `start_ui_bridge` sees `x-mosh-quic-v1` and registers `QuicLegDialer(SshLegDialer)`.
  2. On leg 0, the attacher queries the keeper on a dedicated connection (starting `QuicAcceptor`), closes it and writes the one bootstrap line before any session bytes; it opens no session connection until HELLO arrives on stdin.
  3. `SshLegDialer` waits for and parses the line; `QuicLegDialer` finishes a pinned handshake in under 3 s and returns a `Leg` with a render lane, holding the live ssh leg as its standby.
  4. The bridge sends HELLO with the token. `QuicAcceptor` checks it and builds a `KeeperLeg` with a `RenderLane`; `admit` answers `new` and fixes render-split from HELLO. WELCOME arrives over QUIC and the standby ssh child is torn down. Had the QUIC leg ended before WELCOME, the bridge would have continued on the standby ssh leg with a HELLO carrying a fresh session token and `previous_token` = the QUIC attempt's token; only then does that attacher open its session connection.
- Output:
  1. Server frames go through `FrameSplitter` + `FrameClass`. `Reliable` frames go to the ring; render frames go to `RenderLane`, where a newer `PaneSurface` resets older streams.
  2. The bridge's `RenderReorder` sees the gap, drops chained frames, and injects one resync.
  3. The server hook forces a full surface, which arrives and closes the gap.
- Input and prediction:
  1. The bridge injected `x-mosh.echo-subscribe.v1` after Hello. A keystroke goes through the input tap to `Predictor`, which counts its `events` and paints through the facade. The fast-path guard declines while any prediction exists.
  2. The server counts the events on receipt; a main-loop deadline 50 ms later queues the echo-ack with the committed `surface_revision` on the control lane.
  3. The facade's `observe` passes it to `Predictor`, which confirms or clears the prediction once the retained surface reaches that revision.
- Future changes: a new chained surface kind touches one `FrameClass` arm. A third transport adds one `LegDialer` impl plus one `KeeperLeg` producer, registered in `start_ui_bridge`.

**Deliberate exceptions:**
- Per spec §2: the keeper now classifies herdr frames, overriding Phase 1's "keeper must not parse herdr frames". This is confined to `FrameSplitter` + `FrameClass` in `server_reader`, render-split sessions only.
- Per spec §2: the server resync hook is a deliberate per-kind branch at the client `EndpointControl` dispatch (client_transport.rs:1349), plus a `ServerEvent` variant and its `headless.rs` arm (3b §5).
- Per spec §3: `Predictor` is deliberately process-global, like `OutageState`. Painting and observing reuse the existing `mosh_outage` hooks, so that module is now the fork's client facade under its old mount name.
- Per spec §1 and 3a Key decisions: the ssh fallback is per dial, not sticky, and render-split is a session flag with per-leg lanes (3b §1). A QUIC session whose UDP path breaks later reconnects QUIC-first, then over ssh in the same dial, and goes back to QUIC at the next reconnect dial once UDP returns (the bound QUIC token survives ssh-leg refreshes).

**Gaps for the subepic designs:**
- (a) Echo-ack ordering. Resolved in Phase 4: the ack carries `surface_revision`, rides the control lane, and `Predictor` judges only against a retained surface at or above that revision.
- (b) SRTT sampling — resolved by mosh-19g.6: QUIC legs read quinn's connection RTT; ssh legs keep an EWMA over InflightWindow samples plus heartbeat→ACK round trips, so a typing-only session still gets samples.
- (c) Cleanup. Resolved in Phase 4: one hook statement in `remove_client` removes the client's `EchoTracker` entries.
- (d) QUIC token. Resolved in 3a §2: a new optional HELLO field `quic_token`, checked only by `QuicAcceptor`.

Digest:
- QuicLegDialer: owns QUIC legs, ssh bootstrap and fallback, pinning and roaming;
  new QUIC dial behaviour goes in `quic_leg.rs`; the bridge never knows a leg is QUIC.
- QuicBootstrap: the one definition of the `herdr-mosh-quic:1` line and the keeper's bootstrap answer;
  new bootstrap fields go here; no one else formats or parses the line.
- quic module: owns the blocking adapter over quinn streams, TLS and pinning configs, and the render-stream header;
  shared QUIC plumbing goes here; callers never touch async or rustls directly.
- KeeperLeg: owns a keeper-side leg's stream halves, shutdown and optional render lane;
  a new leg source is a new producer of KeeperLeg; admit/run_leg never name a transport.
- QuicAcceptor: owns the keeper's UDP endpoint, certificate, token admission and idle timeout;
  QUIC admission rules go here; admit never checks QUIC tokens.
- FrameClass: owns which herdr frames are self-contained, chained or reliable;
  a new render kind is one match arm; no caller inspects tags or kind strings itself.
- RenderLane: owns render sequence numbers, per-frame streams and supersede resets for one QUIC leg;
  supersede policy changes go here; never classifies or touches the ring.
- RenderReorder: owns render ordering, gap detection, chain dropping and the resync decision on the bridge;
  gap and resync rules go here; LocalBridge never reasons about render chains.
- Predictor: owns client-side predictive echo (mode, epochs, validation, echo-ack, overlay, fast-path veto);
  prediction rules go in predict.rs; hooks only call it and never touch prediction state.
- EchoTracker / server hooks: owns server-side resync marking and per-pane echo-ack bookkeeping;
  new server-side fork behaviour goes behind these hook functions; upstream lines stay one statement.
- UdpRelay: owns test-only UDP delay, loss and rebind;
  new network faults are relay knobs; tests never open raw sockets themselves.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*

- 2026-10-28: Design roast r1 fixes applied (stale-root sweep against 3a/3b/4; ssh handoff contract; dial address family; echo counting unit; dependency features; redial bound). Escalations parked: QUIC PTO backoff vs 2 RTT after a blackhole; throughput at 10 % loss vs the 2 s 3b bound — both measured by the opt-in timing tests.
- 2026-10-28: Design roast r2 converged (0 Blocking); punch list applied: standby HELLO with fresh token + previous_token; bootstrap query on its own connection with a lazy session connection; bound QUIC token kept across ssh refreshes; render streams oldest-first by priority; echo-ack waits for pending renders; redial wording.
- 2026-10-29: Integration sweep (mosh-19g.7). Upstream hook budget: `scripts/mosh-diff-budget.sh` reports 27 inserted upstream lines, 0 untagged hunks, against the script's budget of 30 (3 lines of headroom) and the user's stated 20 (7 lines over). The script now excludes Cargo.toml and Cargo.lock (the allowed QUIC dependency changes are not hook lines) and the committed `evidence/` run logs were removed from the branch, so the count is tagged hook lines only. The budget limit is unchanged. The three-phase scenario is `tests/mosh_end_to_end.rs` (opt-in): QUIC through `UdpRelay` at 150 ms one way and 10 % loss, supersede on, `HERDR_MOSH_PREDICT=on`; first key echoes locally in about 53 ms (RTT 300 ms), a dense flood keeps flowing across a relay rebind on the same connection (one leg, no ssh), and the client never disconnects. Unwired sweep: every fork constant, capability and env var has a production reader; the blanket `allow(dead_code)` in `predict.rs` and `server_hooks.rs` was removed together with the inert `on_render_sent` hook (no callers).
