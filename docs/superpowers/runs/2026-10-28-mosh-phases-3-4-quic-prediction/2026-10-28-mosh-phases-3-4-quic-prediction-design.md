# Mosh Phases 3 and 4: QUIC leg, cancel-on-supersede, predictive echo

## Goal

A herdr remote session can run over a QUIC leg that survives a network switch or a long suspend
and resumes within about one RTT. Under 10 % packet loss, typing and output keep flowing instead of
stalling behind lost render frames. With prediction enabled at 150 ms RTT, most printable keys
appear locally within 5 ms, and wrong predictions are cleared within one RTT.

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
  shell is under 5 ms, and a wrong prediction is cleared within 1 RTT plus one render.

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

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
