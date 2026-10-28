# Phase 3b: render frames on cancellable QUIC streams with resync

Parent spec: `2026-10-28-mosh-phases-3-4-quic-prediction-design.md` (§2, §6 3b). Bead: mosh-19g.2.
Builds on: `2026-10-28-mosh-phases-3-4-quic-prediction--3a-quic-leg-design.md` (QUIC leg, `KeeperLeg`,
the `Leg` render-lane slot, the render-stream header in the `quic` module).
Research brief: `inputs/seam-brief.md` §2 (server queue, tags, strict chain).

## Goal

Under heavy loss, a stale screen update never delays a newer one. With 10 % loss at 150 ms one-way
delay, typed output becomes visible within 2 s and output keeps advancing. The client's strict render
chain is never broken, so the client never disconnects because of a dropped frame.

## Problem description

On one reliable stream, a lost packet holds back every later byte (head-of-line blocking), so a
burst of output under loss queues stale screen states behind each other. QUIC streams are
independent, so a render frame sent on its own stream can be reset when a newer one makes it
obsolete. herdr's render frames are chained, though:

- `PaneSurfacePatch` (tag 19) and the `endpoint.surface-reuse/delta/scroll.v1` kinds depend on the
  previous frame.
- A reuse/delta/scroll mismatch disconnects the client (seam brief §2, "Strict chain").

Dropping a frame therefore requires dropping its dependents too, then asking the server for a fresh
full surface.

## Main challenges

- Never delivering a chained frame whose base was dropped, whether it was dropped on the wire, by
  the keeper (leg down, backpressure), or across a leg change.
- Leg changes: per 3a, a session can move between QUIC legs (with a render lane) and ssh legs
  (without one).
- Injecting a client→server `EndpointControl` without corrupting the client's own byte stream.
- Bounding keeper memory and QUIC stream count when the link is slow.

## Key decisions made

- **Per-leg split, keeper-side chain gate.** The keeper classifies every server frame when the
  session negotiated render split. On a QUIC leg with a render lane, render frames go to the lane;
  otherwise they go on the session stream, as in Phase 1. Whenever the keeper itself fails to deliver
  a render frame, it closes a **chain gate** and drops chained frames until the next self-contained
  frame. This covers frames dropped while the leg is down, under backpressure, or at a leg change.
- **Bridge-side wire gaps.** On a QUIC leg, `RenderReorder` detects frames lost on the wire, either
  because their stream was reset or because they are overdue. It drops chained frames until the next
  self-contained one, then requests a resync.
- **Resync from both sides, through one encoder.** Each side injects `x-mosh.surface-resync.v1` with
  the crate's own `ClientMessage` encoder, never hand-rolled bytes:
  - the bridge, after a wire gap and once per resume;
  - the keeper, when its own gate closes while a leg is up.
  The server hook forces a full `PaneSurface`.

## Decision points, by section

### 1. Negotiation (chosen: probe plus HELLO flag)

- **Recommended:**
  - The remote advertises `x-mosh-supersede-v1` in `status client --json` (one tagged hook line).
  - The keeper, the server hook and the status line all ship in the same remote binary, so the
    capability tells the bridge that all three are present.
  - The bridge sets a new optional HELLO field `render_split: true` on every leg when the probe shows
    the capability. The field is optional JSON; old keepers ignore it.
  - The keeper fixes the session's render-split flag from the HELLO that answered `new`. A later
    HELLO cannot change it.
  - When the flag is set, the keeper's `server_reader` classifies frames for the whole session. The
    flag does not depend on the leg type: on ssh legs, render frames simply ride the session stream.
- **Considered:** a split flag only on QUIC legs. Rejected: the keeper's chain gate has to run on
  ssh legs too, after a QUIC → ssh change.

### 2. Keeper: classification, gate and lane (chosen)

- **`server_reader`** (render-split sessions only) runs `FrameSplitter` and `FrameClass` over server
  bytes. It routes each frame:
  - `Reliable` → the ring and session stream, unchanged;
  - render frame while the chain gate is **closed**:
    - `Chained` → dropped;
    - `SelfContained` → opens the gate and is routed as below;
  - render frame while the gate is open, current leg has a `RenderLane` → `try_send` into the
    lane's bounded channel (capacity 64 frames). If the channel is full, the frame is dropped and the
    gate closes. A `SelfContained` frame that does not fit evicts the queued render frames instead
    (they are all superseded by it).
  - render frame while the gate is open, current leg has no lane (ssh) → the ring and session stream,
    in order. Classification still runs.
  - no current leg (down) → dropped, and the gate closes.
- **Gate closing while a leg is up** (lane overflow): the keeper injects one
  `x-mosh.surface-resync.v1` towards the server. This needs a client→server `FrameSplitter` in the
  keeper's `server_writer` so the injection lands on a frame boundary.
- **Gate closed at leg admission:** the bridge's resume rule (§4) sends the resync, so the keeper
  does not inject.
- **`RenderLane`** (keeper runtime, per QUIC leg) behaviour:
  - It assigns render sequence numbers starting at 0 for each leg.
  - It opens one unidirectional stream per frame, writing the header (sequence number and class)
    and then the frame bytes.
  - On a `SelfContained` frame, it resets every older stream still in flight, so a reset batch always
    ends just before a self-contained frame. Before those resets, it sends the new frame's own stream
    header, so the bridge can tell a reset caused by supersede from a loss.
  - The leg's quinn transport config caps concurrent unidirectional streams at 64. When none are
    free, the lane waits, the channel fills, and the overflow rule above applies.
  - Stream priority is the same as the session stream.
- **Frames never acked:** render frames do not consume session offsets and are not in the replay ring.
  `InflightWindow` (Phase 2) governs only the session stream. QUIC congestion control and supersede
  govern the lane.

### 3. Bridge: `RenderReorder` (chosen)

- The bridge reads the leg's render-lane receiver. Each completed stream yields
  `(seq, class, bytes)`, and each reset stream yields `(seq, reset)`.
- **`RenderReorder`** (sans-IO, time as an input):
  - It delivers frames in sequence order. A frame that arrives ahead of a missing one waits.
  - A missing sequence number becomes a **gap** when:
    - its stream was reset; or
    - it has been outstanding longer than `max(1 × SRTT, 50 ms)` since a later sequence number
      arrived. SRTT comes from the process-local read (mosh-19g.6); if unknown, 250 ms is used.
  - On a gap:
    - It drops all held and later `Chained` frames until the next `SelfContained` frame, which it
      delivers and which clears the gap.
    - A `SelfContained` frame that arrives after a gap releases immediately, even if older sequence
      numbers are still missing: they are superseded.
    - It requests one resync per gap. A gap that a `SelfContained` frame closes before the request
      fires sends no request.
  - Each new leg starts in the gap state with sequence 0 expected. This covers the case where the
    first frame on the new leg is chained after a keeper-side drop.
- **Interleaving:** `LocalBridge` writes delivered render frames into the client output at
  `FrameSplitter` boundaries of the session stream, by arrival. They carry no session offset.
- **Resync injection:** a client→server `FrameSplitter` in the bridge's upstream path. The resync
  `ClientMessage::EndpointControl` frame is written between client frames, using the crate's own
  encoder.

### 4. Resume rule (chosen)

On every admitted leg after the first, in a render-split session, the bridge sends one resync after
WELCOME, whatever the leg type. On an ssh leg this pairs with the keeper's chain gate: the gate
discards chained frames until the full surface arrives.

### 5. Server hook (chosen)

- `src/mosh/server_hooks.rs` (seam from mosh-19g.5) handles `x-mosh.surface-resync.v1` by marking the
  client for a full `PaneSurface` on its next render, through the existing reset path that
  reactivation uses. The server also discards its stale pending render and the transport baseline, so
  the next frame is self-contained.
- One tagged hook statement at the client `EndpointControl` dispatch (`client_transport.rs:1349`)
  calls the module's dispatch entry point.
- Older servers ignore the kind; the bridge then recovers on the next naturally sent full frame.

### 6. Acceptance and tests (chosen)

- **Unit tests:**
  - `FrameClass` table: every tag and kind, plus unparseable input → `Reliable`.
  - `RenderReorder`: in order; out of order; reset → drop chained until self-contained; overdue
    gap with a fake clock; a self-contained frame supersedes missing ones; once-per-gap resync; new
    leg starts in gap.
  - Keeper chain gate: down, overflow and evict-on-self-contained paths.
  - `RenderLane` supersede resets older streams (loopback quinn).
- **Integration** (`tests/mosh_supersede.rs`):
  - No disconnect: a burst of patches with 20 % loss, both leg types and a QUIC → ssh leg change.
  - The server hook produces a full surface after an injected resync.
  - An old remote without the capability → no split.
- **Timing** (`#[ignore]`, `timing_lock()`, dense flood, `UdpRelay` at 10 % loss and 150 ms
  one-way):
  - A typed marker's echo is visible on screen within 2 s while the flood runs.
  - Output keeps advancing: the screen changes at least once per second over 20 s.
  - The same test over a Phase 1 ssh-only leg is recorded for comparison, with no assertion.

### 7. Out of scope

- The server-side surface ring with acked bases (root §2 "Considered").
- Per-pane supersede: the server sends whole-client surfaces, so per-client is enough.
- Priority tuning between the lane and the session stream.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
