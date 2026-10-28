# Mosh Phases 3 and 4: QUIC leg, cancel-on-supersede, predictive echo

## Goal

A herdr remote session can run over a QUIC leg that survives a network switch or a long suspend
and resumes within about one RTT. Under 10 % packet loss, typing and output keep flowing instead of
stalling behind lost render frames. With prediction enabled at 150 ms RTT, most printable keys
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
needs a one-statement server hook. The vision's server-side ring of sent surfaces with an acked base is
not built: forcing a full frame achieves the same "no stall" outcome with one hook instead of a server
rewrite (recorded below as a parked follow-up). Prediction (4) runs in the client and draws over the
retained surface; the server reports per-pane echo progress as `x-mosh.echo-ack.v1` through a hook on
its input and render paths. The upstream hook budget rises from 20 to 30 lines for these phases.

## Decision points, by section

### 1. QUIC leg (Phase 3a): a second `LegDialer` (chosen)

**Recommended:**
- **Keeper side.** The keeper opens one UDP listener per keeper process and runs a `quinn` endpoint on
  its own tokio runtime thread. Each QUIC connection is a leg. The keeper's existing per-connection leg
  handling (HELLO/WELCOME/DATA/ACK/HEARTBEAT/END) runs over one bidirectional QUIC stream adapted to
  blocking `Read`/`Write`.
- **Bootstrap over ssh.** The attacher, which owns the ssh stdio, asks the keeper over the rendezvous
  socket for `port`, the certificate's SHA-256 fingerprint and a per-session 32-byte token. It prints
  one line `herdr-mosh-quic:1 <port> <fingerprint> <token>` after the ready marker. The certificate is
  self-signed (`rcgen`) and lives as long as the keeper. The token binds the QUIC leg to the session:
  the first message on the stream is the existing HELLO, carrying the token.
- **Local side.** `QuicLegDialer` dials `<ssh target host>:<port>`, pins the fingerprint, and hands the
  bridge a `Leg` whose reader and writer wrap the stream. The bridge is unchanged.
- **Roaming.** QUIC connection migration carries the leg across an address change. The client side
  calls `Endpoint::rebind` on a network change detected by a failed send or a silence timeout, and
  keeps the connection ID. The keeper's idle timeout is the session grace period (`HELLO.grace_ms`), so
  a 10-minute suspend within grace resumes the same QUIC connection, or dials a new one in one RTT.
- **Fallback.** Leg 0 dials QUIC only after the ssh bootstrap printed the line. If the QUIC handshake
  does not complete within 3 s, or the line is absent (older remote), the dialer falls back to the
  ssh leg for this session, and later legs use ssh too. A capability `x-mosh-quic-v1` in
  `status client --json` gates the attempt.
- **Dependencies.** `quinn` (features `runtime-tokio`, `rustls-ring`), `rustls`, `rcgen`; ring, never
  aws-lc-rs. tokio is already a dependency with `net` reachable.

**Considered:** a QUIC leg that replaces the session layer (QUIC's own reliability instead of Phase 1's
offsets and replay). Rejected: a new QUIC connection after a long suspend loses QUIC's stream state,
and Phase 1's replay is what makes resume exactly-once. Mosh-style raw UDP with its own SSP: rejected,
QUIC gives encryption, congestion control and migration for free.

### 2. Cancel-on-supersede (Phase 3b): fork-side classification, one server hook (chosen)

**Recommended:**
- **Classification** (keeper, server→client direction). Each server frame is classified by its bincode
  tag and, for tag 20, its kind string:
  - **self-contained render:** `PaneSurface` (13), `ClientShellSnapshot` (12);
  - **chained render:** `PaneSurfacePatch` (19), `EndpointControl` kinds `endpoint.surface-reuse.v1`,
    `endpoint.surface-delta.v1`, `endpoint.surface-scroll.v1`;
  - **everything else:** reliable, in order, on the session stream as today.
- **Streams.** Render frames go on their own unidirectional QUIC streams, each prefixed with a render
  sequence number and its class. A frame becomes superseded when a later self-contained render frame
  for the same client is enqueued; the keeper resets every older render stream still in flight.
  Chained frames are never reset on their own: they are only reset together with everything up to a
  later self-contained frame.
- **Ordering and gaps** (local bridge). The bridge delivers render frames to the client in render
  sequence order, interleaved with session-stream frames by arrival. When a render frame is missing
  (its stream was reset or is overdue by more than 1 RTT), the bridge drops every later chained
  frame until the next self-contained frame, so the client's decoder never sees a broken chain. Then
  it sends one `EndpointControl` kind `x-mosh.surface-resync.v1` towards the server.
- **Server hook.** On `x-mosh.surface-resync.v1` the server marks that client for a full
  `PaneSurface` on its next render, through an existing reset path (reactivation already sends a
  full surface). This is one hook statement at the server's client `EndpointControl` dispatch
  (`src/server/client_transport.rs:1349`). Unknown kinds are already ignored, so an older server is
  safe: the bridge then just waits for the next naturally sent full frame.
- **Capability.** `x-mosh-supersede-v1` in `status client --json`; the keeper splits render frames only
  when both ends advertise it.
- **Resume.** Render frames are not in the session replay ring; after a reconnect the bridge sends
  `x-mosh.surface-resync.v1` once.

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
  and tracks them in epochs as mosh does: content-only validation against incoming surfaces, an epoch
  ends on a non-printable key, a confirmed wrong prediction clears the epoch, paste and resize reset.
  Shown only when SRTT > 60 ms, underlined when SRTT > 160 ms.
- **SRTT source.** The local bridge already measures RTT (`InflightWindow`); it exposes a process-local
  SRTT the client reads (same process). Over the plain bridge (no mosh), prediction stays off.
- **Echo signal.** The server emits `EndpointControl` kind `x-mosh.echo-ack.v1` with
  `{pane_id, input_seq}` on the render lane after a render that follows processed input, so the client
  can judge predictions made before that input. Two hook statements: one where the server applies
  pane input, one where it sends a render. The client tags its own input sequence locally; the server
  echoes the count of input events it has applied per pane.
- **Painting.** Predictions are painted in the existing composition hook (the outage banner's paint
  function is extended, no new line), and one hook line makes the patch fast path decline while
  predictions are visible.
- **Flag.** Off unless `HERDR_MOSH_PREDICT=1` (or `adaptive`, mosh's default behaviour).

**Considered:** a client-side terminal emulator to predict (mosh's way). Rejected by the vision: the
retained surface is already on the client.

### 4. Upstream footprint (chosen: budget 30)

**Recommended:** the hook budget rises from 20 to 30 for these phases (the vision set 20 "through
Phase 2"). Expected new hooks:
- `status.rs` capability lines for `x-mosh-quic-v1` and `x-mosh-supersede-v1`;
- the server resync dispatch;
- two server echo-ack hooks;
- the client fast-path guard and the client input tap for the predictor.

That is about 7–9 new lines, each one statement tagged `// mosh-fork hook`.
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

- **3a:** a relay rebind mid-session, and a 10 minute idle within grace, both resume and show the next
  keystroke's echo within 2 RTT of the link returning. Fallback: with UDP blocked, the session comes
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
  - the leg-0 `QuicBootstrap`;
  - the sticky "fell back to ssh" decision for the session;
  - the 3 s handshake bound;
  - fingerprint pinning;
  - roaming (`Endpoint::rebind` on a failed send or a short silence watchdog, keeping the connection ID);
  - classification of QUIC leg ends.
- Hides: QUIC. The bridge sees a `Leg` and never learns about migration, rebinds or fallback.
- Must not: build ssh commands or read ssh preambles. It composes an `SshLegDialer` for the leg-0 bootstrap and for every leg after fallback. It must not touch session bytes or the give-up deadline (still the `LocalBridge`'s, Phase 1).
- Relationships: implements `LegDialer`; composes `SshLegDialer` (delegation); uses the `quic` module and `QuicBootstrap`. Dependencies point `local_bridge` → trait ← `quic_leg` → `ssh_leg`.
- Lifecycle: built by `start_ui_bridge` and owned by the `LocalBridge` behind the existing `Arc<Mutex<Box<dyn LegDialer>>>`. Dropping it shuts the runtime down. On leg 0 it dials ssh, takes the bootstrap, then returns a QUIC leg (tearing the ssh child down) or that same ssh leg as the fallback, so fallback costs no second ssh handshake.
- Errors:
  - Leg 0: any QUIC failure (timeout, UDP blocked, fingerprint mismatch, missing line) fails over to ssh and is never surfaced as a `LegError`.
  - Reconnect legs: an unreachable host or a timeout is `Retryable`; a fingerprint mismatch is `Fatal`.
  - A keeper `END` keeps Phase 1's classification.

**QuicBootstrap** — new value type, the single definition of `herdr-mosh-quic:1 <port> <fingerprint> <token>` and of the keeper's rendezvous answer that produces it.
- Owns: encoding (attacher side), parsing (dialer side), validating the fields, and the 32-byte token type.
- Must not: do I/O.
- Relationships: shared by Attacher, Keeper and `QuicLegDialer`, like `LinkStatus`.
- Errors: a malformed or absent line means "no bootstrap", which the dialer turns into the ssh fallback.

**quic module** — new module of QUIC plumbing shared by both ends (`src/mosh/quic.rs`).
- Owns:
  - the blocking `Read`/`Write` adapter over quinn streams, driven from a runtime handle;
  - the TLS configs: rcgen self-signed certificate plus SHA-256 fingerprint on the keeper, a pinning verifier on the client, ring only;
  - the render-stream header (render sequence number plus `FrameClass`).
- Hides: async/blocking bridging and rustls setup from the std-thread code.
- Must not: know session or HELLO semantics, or decide which frames go on render streams.
- Relationships: used by `QuicLegDialer`, `QuicAcceptor`, `RenderLane` and the bridge's render-lane reader.

**KeeperLeg** — new struct, the keeper's transport-neutral accepted leg: reader and writer halves, a shutdown handle callable from another thread (stealing, the silence verdict, session end), and an optional `RenderLane`.
- Owns: what `admit`/`run_leg`/`leg_writer`/`LegSlot` take today from a concrete `UnixStream`.
- Hides: whether the leg is a rendezvous Unix connection or a QUIC stream.
- Must not: carry session state.
- Relationships: two producers (the Unix accept loop and `QuicAcceptor`) feed the one `admit` → `run_leg` handler.
- Lifecycle: per accepted connection, owned by its `run_leg` thread; the `LegSlot` keeps the shutdown handle. Dropping it closes the stream, and for QUIC the connection.

**QuicAcceptor** — new struct, the keeper's UDP endpoint: one per keeper process, on its own tokio runtime thread.
- Owns:
  - the certificate, port and fingerprint (answering the bootstrap query through the keeper);
  - minting and checking per-session tokens;
  - accepting connections and their first bidirectional stream;
  - the pre-WELCOME bound;
  - the connection idle timeout, from `HELLO.grace_ms`;
  - building a `KeeperLeg` with a `RenderLane`.
- Hides: QUIC admission. A bad or missing token is refused here, so `admit` stays transport-blind.
- Must not: apply the HELLO table, steal legs, or touch the token table beyond the token check.
- Lifecycle: started lazily by the first bootstrap query; dies with the keeper process.
- Errors: a refused token closes the connection with an END.

**FrameClass** — new closed enum `SelfContained | Chained | Reliable`, plus the one function that classifies a whole herdr frame (`link_frames.rs`, next to `FrameSplitter`).
- Owns: spec §2's table:
  - tags 13 and 12 → `SelfContained`;
  - tag 19 and the tag-20 kinds `endpoint.surface-reuse/delta/scroll.v1` → `Chained`;
  - everything else → `Reliable`.
  It decodes the bincode tag, and the kind string for tag 20.
- Hides: the frozen wire layout from the keeper and the bridge.
- Must not: decide what happens to a class.
- Errors: an unparseable frame is `Reliable`, never dropped.

**RenderLane** — new struct, the keeper-side render sender for one QUIC leg.
- Owns: render sequence numbers; one unidirectional stream per render frame with its header; the in-flight set; supersede. A `SelfContained` frame resets every older in-flight stream; `Chained` frames are only reset with that batch.
- Hides: QUIC stream reset.
- Must not: classify frames, touch the ring or the `InflightWindow`, or buffer while the leg is down (the keeper drops render frames with no lane).
- Relationships: owned by a `KeeperLeg`; fed from the session's `server_reader` through a channel into the keeper runtime. Concrete, QUIC only.
- Lifecycle: per leg; a new leg starts a fresh sequence.

**RenderReorder** — new struct, the bridge-side render ordering and gap state machine; sans-IO, time as an input.
- Owns:
  - in-order delivery by render sequence;
  - gap detection (a reset stream, or a frame overdue by more than 1 SRTT);
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
  - the input tap: printable text from `ClientShellPaneInput`, and the per-pane input-event count;
  - predicted cells placed at the cursor through `inner_rect`;
  - epochs and mosh's rules: content-only validation against the retained surface at paint time, an epoch ending on a non-printable key, clearing on a confirmed wrong prediction, reset on paste or resize;
  - consuming `x-mosh.echo-ack.v1`;
  - the thresholds (shown above 60 ms, underlined above 160 ms);
  - the "predictions visible" answer the fast-path guard reads.
- Hides: the prediction rules from every client hook.
- Must not: mutate the retained surface or the decoder baseline (it paints an overlay only); measure RTT (the bridge supplies SRTT); send anything to the server.
- Relationships: reached only through the fork's client hooks. The existing `mosh_outage::observe`/`paint` become the fork's client facade: a closed `match` on kind routes `x-mosh.link.v1` → `OutageState` and `x-mosh.echo-ack.v1` → `Predictor`, and paints both overlays. Two new hooks: the input tap and the fast-path guard.
- Lifecycle: process-global behind a mutex, like `OutageState`; entries go when an endpoint is removed or reset.
- Errors: a malformed echo-ack is consumed and ignored.

**EchoTracker / server hooks** — new fork module for the server side (`src/mosh/server_hooks.rs`), the bodies behind the server hook statements.
- Owns:
  - the resync hook: `x-mosh.surface-resync.v1` marks that client for a full `PaneSurface` through the existing reactivation reset path;
  - the applied input-event count per client and pane;
  - the decision to emit `x-mosh.echo-ack.v1 {pane_id, input_seq}` after a render that follows newly applied input.
- Hides: echo bookkeeping from `client_transport.rs` and `render.rs`.
- Must not: put the echo-ack in the latest-wins render slot. It follows the render it acknowledges through the ordered lane.
- Lifecycle: a process-global map keyed by client, with entries removed on client disconnect.

**UdpRelay** — new test helper (`tests/mosh_harness.rs`): a userspace UDP relay with one-way delay, seeded loss and `rebind`; test-only, beside the Phase 2 `link` knob. The fake-ssh wrapper rewrites the bootstrap port to the relay's port, so production code gets no address override.

**Extended:**
- **Leg** — optionally carries a render-lane receiver (`None` for ssh), taken like the reader and writer. Its end report gains a QUIC close reason in place of an `ExitStatus`. It still never interprets bytes.
- **SshLegDialer** — in bootstrap mode it reads one line after the ready marker with a short bound and exposes the parsed `QuicBootstrap`; absent or malformed means none.
- **start_ui_bridge** — still the single registration point. It wraps `SshLegDialer` in `QuicLegDialer` when the probe shows `x-mosh-quic-v1`.
- **Keeper**:
  - a pre-HELLO bootstrap query on a rendezvous connection (a closed match on the first byte, not a new `SessionMsg` variant);
  - legs arrive as `KeeperLeg`;
  - a per-session render-split flag, fixed by the leg that answered `new` (QUIC lane plus both capabilities). When set, `server_reader` runs `FrameSplitter` + `FrameClass`: `Reliable` frames go to the ring; render frames go to the current leg's `RenderLane`, or are dropped while down.
- **Attacher** — after the ready marker and before pumping, it queries the keeper and prints the `QuicBootstrap` line.
- **LocalBridge** — reads the leg's render lane into `RenderReorder`; interleaves delivered frames into the client output without offsets (not acked, not replayed); injects the resync `ClientMessage` frame upstream at a client frame boundary, which needs a client→server `FrameSplitter`.
- **InflightWindow** — also keeps a smoothed RTT (SRTT), still the only RTT sampler.
- **LinkStatus** — `Linked` gains an optional `srtt_ms`, re-injected only on meaningful SRTT change; this is how SRTT reaches the right client endpoint.
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
  2. On leg 0, the attacher queries the keeper (starting `QuicAcceptor`) and prints the line.
  3. `SshLegDialer` parses the bootstrap; `QuicLegDialer` finishes a pinned handshake in under 3 s and returns a `Leg` with a render lane, then the ssh child is torn down.
  4. The bridge sends HELLO. `QuicAcceptor` checks the token and builds a `KeeperLeg` with a `RenderLane`; `admit` answers `new` and sets render-split.
- Output:
  1. Server frames go through `FrameSplitter` + `FrameClass`. `Reliable` frames go to the ring; render frames go to `RenderLane`, where a newer `PaneSurface` resets older streams.
  2. The bridge's `RenderReorder` sees the gap, drops chained frames, and injects one resync.
  3. The server hook marks a full surface, which arrives and closes the gap.
- Input and prediction:
  1. A keystroke goes through the input tap to `Predictor` and is painted through the facade. The fast-path guard declines while predictions are visible.
  2. The server input hook counts the input; its render hook sends the echo-ack after the render.
  3. The facade's `observe` passes it to `Predictor`, which confirms or clears the prediction.
- Future changes: a new chained surface kind touches one `FrameClass` arm. A third transport adds one `LegDialer` impl plus one `KeeperLeg` producer, registered in `start_ui_bridge`.

**Deliberate exceptions:**
- Per spec §2: the keeper now classifies herdr frames, overriding Phase 1's "keeper must not parse herdr frames". This is confined to `FrameSplitter` + `FrameClass` in `server_reader`, render-split sessions only.
- Per spec §2: the server resync hook is a deliberate per-kind branch at the client `EndpointControl` dispatch (client_transport.rs:1349).
- Per spec §3: `Predictor` is deliberately process-global, like `OutageState`. Painting and observing reuse the existing `mosh_outage` hooks, so that module is now the fork's client facade under its old mount name.
- Per spec §1: the ssh fallback is leg-0-only and sticky. A QUIC session whose UDP path breaks later retries until give-up and never switches to ssh, because render-split is fixed for the session.

**Gaps for the subepic designs:**
- (a) Echo-ack ordering. The echo-ack rides the session stream while renders ride separate streams, so it can arrive first. `Predictor` must judge only against a surface received after the ack, for example via an optional surface revision in the echo-ack JSON.
- (b) SRTT sampling. `InflightWindow` samples only on ACKs advancing ≥ `ACK_EVERY_BYTES`, so a typing-only session gets no samples. SRTT needs its own rule, such as the heartbeat/ACK round trip.
- (c) Cleanup. `EchoTracker` entries need removing on client disconnect, either via one more hook or by pruning inside the existing ones.
- (d) QUIC token. The 32-byte QUIC token differs from the 128-bit session `Token`, so it needs a new optional HELLO field, checked only by `QuicAcceptor`.

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
