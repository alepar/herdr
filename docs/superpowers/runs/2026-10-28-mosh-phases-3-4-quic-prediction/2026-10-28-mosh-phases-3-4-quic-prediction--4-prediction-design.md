# Phase 4: predictive local echo with server echo-ack

Parent spec: `2026-10-28-mosh-phases-3-4-quic-prediction-design.md` (§3, §4, §6 4; gaps a–c in its Key abstractions). Bead: mosh-19g.3.
Builds on: 3a (`--3a-quic-leg-design.md`), 3b (`--3b-supersede-design.md`; the `server_hooks` client EndpointControl dispatch and render lanes), and root seams mosh-19g.5 (`server_hooks`, budget 30) and mosh-19g.6 (SRTT, `LinkStatus.srtt_ms`).
Research brief: `inputs/seam-brief.md` §3–§4 (client compose, fast path, input path).

## Goal

On a slow link, typed printable characters appear immediately. At 150 ms one-way delay with prediction on, the median time from keypress to the character on screen is under 5 ms. A wrong prediction disappears within 1 RTT plus one render. On a fast link, or when the remote lacks support, nothing changes.

## Problem description

Every keystroke's echo currently waits a full round trip. mosh hides this by painting a predicted echo locally and checking it against what the server later shows. herdr's client already holds the server's retained pane surface, so it can check predictions against real cells without its own terminal emulator.

To judge a prediction it needs one more fact: which of its inputs the server has applied, and in which surface revision their effect is visible.

## Main challenges

- Ordering: the echo signal and renders can travel on different streams (3b), so an echo-ack can arrive before the surface that reflects it.
- The patch fast path writes rows straight to the terminal and would overwrite or ignore the overlay.
- A prediction must trigger an immediate repaint, or it is no faster than the echo.
- Counting inputs the same way on both sides, through reconnects.
- Keeping the server cost zero for clients that do not ask for it.

## Key decisions made

- **The echo-ack carries a surface revision.** `x-mosh.echo-ack.v1` data is `{pane_id, input_seq, surface_revision}`.
  - It means: the first `input_seq` input events for this pane have been applied, and their effect is in every surface with revision ≥ `surface_revision`.
  - The client judges a prediction only once it holds both an ack covering it and a retained surface at or above that revision. This resolves root gap (a) independently of arrival order.
- **Opt-in subscription.** The server sends echo-acks only to a client that sent `x-mosh.echo-subscribe.v1` (a client→server EndpointControl through 3b's `server_hooks` dispatch, so no new upstream line).
  - The client subscribes only when prediction is enabled and the remote advertises `x-mosh-echo-v1`.
  - The bridge learns that from its status probe and exposes it process-locally, like SRTT.
- **Client SRTT from `LinkStatus.srtt_ms`.** It arrives per endpoint through the existing link-frame observe hook (mosh-19g.6). Without a link frame, which means no mosh, prediction stays off.

## Decision points, by section

### 1. Input accounting (chosen: count events per pane per connection)

- **Client:**
  - One input-tap hook statement at the point where `ClientShellPaneInput{pane_id, events}` is sent (client/shell_runtime.rs:805 or `push_target_event`).
  - It passes the pane id and events to the predictor, which adds `events.len()` to that pane's sent counter and creates predictions for printable text: `Key.generated_text` and `TextCommit`.
- **Server:**
  - One hook statement where the server applies `ClientShellPaneInput` (client_transport.rs:1196) adds `events.len()` to the client and pane's applied counter in `EchoTracker`.
  - The counts are per server-side client connection. A mosh resume keeps that connection (the keeper holds it), so the counts continue.
  - A new connection resets both sides: the server on client disconnect, the client on endpoint reset or a new `Welcome`.
- **Client disconnect:** one hook statement calls the `server_hooks` disconnect entry point, which removes the client's `EchoTracker` entries (root gap c).

### 2. Echo-ack emission (chosen: after the render that reflects applied input)

- One hook statement at the render send path, the full and patch senders (render.rs:963 and retained_surface.rs:566), reports `(client, surface_revision)` once a frame is enqueued.
  - If both senders cannot share one statement, it is two lines.
- For each pane whose applied count increased since the last ack, `EchoTracker` queues `x-mosh.echo-ack.v1 {pane_id, input_seq: applied, surface_revision}` on the client's control lane.
- Order on the wire does not matter, because the client waits for the revision.
- Acks are sent only to subscribed clients. Unsubscribed clients cost one map lookup.

### 3. Predictor (client, `src/mosh/predict.rs`)

- **Mode:**
  - `HERDR_MOSH_PREDICT` is read once: `off` (the default), `on`, or `adaptive`.
  - `on` shows predictions whenever the link reports SRTT.
  - `adaptive` shows them only when SRTT > 60 ms.
  - Both underline predictions when SRTT > 160 ms.
- **Placement:**
  - A prediction cell sits at the pane's cursor, and the cursor advances one column per printable character (width from `unicode-width`, already a herdr dependency if present; otherwise ASCII-only prediction).
  - Prediction stops at the pane's right edge.
  - The cursor comes from the retained surface. If the surface carries no cursor for the pane, there is no prediction.
- **Epochs (mosh rules):**
  - Printable keys extend the current epoch.
  - Any non-printable key (Enter, Backspace, arrows, control keys, mouse) ends it. Later predictions start a new epoch, which in `adaptive` mode stays hidden until one prediction in it is confirmed.
  - Paste, a pane resize or rect change, and an endpoint reset clear all predictions for the pane.
- **Validation** (each time the retained surface or the ack state changes):
  - A prediction is judged once its pane holds an ack with `input_seq` ≥ the prediction's own sequence, and the retained surface revision is ≥ that ack's `surface_revision`.
  - If the cell at the predicted position holds the predicted character, the prediction is confirmed and removed.
  - Otherwise it was wrong: every prediction for that pane is cleared, and the epoch becomes hidden.
- **Painting:**
  - The existing composition hook (composition.rs:723, today `mosh_outage::paint`) becomes the fork client facade's paint, which paints the outage banner and the predictions.
  - The existing observe hook routes `x-mosh.link.v1` to the outage state and `x-mosh.echo-ack.v1` to the predictor, through one closed match in the facade.
- **Fast-path guard:** one hook statement in `fast_path_blocker` (client/shell/surface_patch.rs:56-90) makes it decline while predictions are visible.
- **Immediate repaint:** the input-tap hook's result requests a recompose through the client's existing redraw request when a prediction was added. If the input path has no single-statement redraw request, the tap returns a bool that the hook line feeds to the existing request. This is the requirement behind the 5 ms bound.

### 4. Footprint

New upstream lines, each one tagged statement:

| Line | Count |
|---|---|
| Input tap | 1 |
| Fast-path guard | 1 |
| Server input-applied | 1 |
| Server render-sent | 1–2 |
| Client disconnect | 1 |
| `x-mosh-echo-v1` status line | 1 |

That is 6–7 lines. Running total after 3a and 3b is about 25 of 30.

### 5. Acceptance and tests

- **Unit tests:**
  - Predictor epochs: confirm, wrong, epoch hiding, paste/resize reset, right-edge stop, and judging only at or above the ack revision (an ack ahead of its surface leaves predictions pending).
  - Mode thresholds.
  - Facade routing.
  - `EchoTracker`: count, emit-after-render, subscription gating, disconnect cleanup.
- **Integration** (`tests/mosh_predict.rs`, harness link knob, 150 ms one-way, `HERDR_MOSH_PREDICT=on`):
  - Typed text appears and stays correct.
  - With an echo-suppressing program (`stty -echo; cat`), a typed character is painted, then cleared within 1 RTT plus one render after the ack.
  - With an old remote (capability stripped), there are no predictions and no subscribe.
- **Timing** (`#[ignore]`, `timing_lock()`): the median keypress-to-screen latency for 50 printable keys at 150 ms one-way is under 5 ms. It is measured from the predictor's trace timestamps (tap to the paint frame written), and cross-checked by the harness screen showing the character before 1 RTT.
- **Prediction off by default:** the existing suites run unchanged.

### 6. Out of scope

- Predicting Backspace and cursor movement.
- Predictions in alternate-screen applications beyond what content validation allows.
- Client-side RTT measurement independent of the bridge.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
