# Phase 4: predictive local echo with server echo-ack

Parent spec: `2026-10-28-mosh-phases-3-4-quic-prediction-design.md` (§3, §4, §6 4; gaps a–c in its Key abstractions).
Bead: mosh-19g.3
Builds on: 3a (`--3a-quic-leg-design.md`), 3b (`--3b-supersede-design.md`; the `server_hooks` client EndpointControl dispatch and render lanes), and root seams mosh-19g.5 (`server_hooks`, budget 30) and mosh-19g.6 (SRTT, `LinkStatus.srtt_ms`).
Research brief: `inputs/seam-brief.md` §3–§4 (client compose, fast path, input path).

## Goal

On a slow link, typed printable characters appear immediately. At 150 ms one-way delay with prediction on, the median time from keypress to the character on screen is under 5 ms. A wrong prediction disappears within 1 RTT + 50 ms (mosh's echo delay) + one render. On a fast link, or when the remote lacks support, nothing changes.

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
- **Opt-in subscription, injected by the bridge.** The server sends echo-acks only to a client subscribed with `x-mosh.echo-subscribe.v1` (a client→server EndpointControl through 3b's `server_hooks` dispatch, so no new upstream line).
  - The local bridge injects it right after Hello when the probe shows `x-mosh-echo-v1` and prediction is not `off`, then sets `LinkStatus.echo` so the client knows per endpoint.
- **Client SRTT from `LinkStatus.srtt_ms`.** It arrives per endpoint through the existing link-frame observe hook (mosh-19g.6). Without a link frame, which means no mosh, prediction stays off.

## Decision points, by section

### 1. Input accounting (chosen: count events per pane per connection)

- **Unit:** one element of `ClientShellPaneInput.events`, counted at the client's input tap and at the
  server's receipt (client_transport.rs:1196). Counts are per (server client connection, pane).
- **Client:**
  - One input-tap hook statement at the point where `ClientShellPaneInput{pane_id, events}` is sent (client/shell_runtime.rs:805 or `push_target_event`).
  - It passes the pane id and events to the predictor, which adds `events.len()` to that pane's sent counter and creates predictions for printable text: `Key.generated_text` and `TextCommit`.
- **Server:**
  - One hook statement where the server applies `ClientShellPaneInput` (client_transport.rs:1196) adds `events.len()` to the client and pane's applied counter in `EchoTracker`.
  - The counts are per server-side client connection. A mosh resume keeps that connection (the keeper holds it), so the counts continue.
  - The server creates the client's `EchoTracker` entry on the first `x-mosh.echo-subscribe.v1`. A repeated subscribe (the replay on a new leg) is idempotent and does not reset counts.
  - The client resets on a new connection generation (Key abstractions, Predictor keying), which is exactly when the server side is a new client connection; the server's old entry went with the old connection's disconnect.
- **Client disconnect:** one hook statement calls the `server_hooks` disconnect entry point, which removes the client's `EchoTracker` entries (root gap c).

### 2. Echo-ack emission (chosen: deadline-driven, as mosh; see Key abstractions)

- Emission runs from the main-loop deadline (`echo_due`, Key abstractions), not from the render send path; there is no render-sent hook.
- For each pane whose applied count increased since the last ack, and whose input was received at least 50 ms earlier, `EchoTracker` queues `x-mosh.echo-ack.v1 {pane_id, input_seq: applied, surface_revision}` on the client's control lane.
- **Pending renders hold the ack.** At the deadline, if a render for that client is pending or deferred (the pending render slot is occupied, the pane is dirty, or a render was held back by backpressure, `TrySendError::Full` at render.rs:963-990), the ack waits for a later tick. It uses the last committed `surface_revision` only when nothing is pending for that client, so the revision it names already contains the echo. The check runs inside the existing `echo_due` statement; no new hook line.
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
  - Any non-printable key (Enter, Backspace, arrows, control keys, mouse) ends it. Later predictions start a new epoch, which stays hidden, in every mode, until one prediction in it is confirmed.
  - Paste, a pane resize or rect change, an endpoint reset, echo loss, a wrong prediction and the no-ack timeout clear all predictions for the pane; the epoch after a clear is hidden the same way.
  - Why every mode, not only `adaptive`: the key that ends an epoch is often the one that opens a no-echo prompt (Enter after `sudo`, `ssh`, `gpg` or `stty -echo`). A visible new epoch would paint the password typed there in cleartext on the local screen until the first echo-ack judged it wrong, about one RTT plus the 50 ms ack deadline plus one render, and a screen recording or share would capture it. Upstream mosh hides tentative epochs in every display mode except Experimental; `on` differs from `adaptive` only in the SRTT threshold.
  - The cost: the first character after Enter shows at its confirmation, one RTT late; the rest of that epoch paints at once. The first epoch of a connection is visible (nothing breaks before it), so a client that attaches or reconnects while a pane already waits at a no-echo prompt still paints that text until it is judged wrong.
  - The `stty -echo; cat` integration test types the command and Enter through the client and asserts the text typed into `cat` is never painted (this supersedes "painted, then cleared" in §5 for text typed after Enter; the R8 timing test still covers clearing a first-epoch key).
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
| Server input count (receipt, client_transport.rs:1196) | 1 |
| `echo_due` in the headless main loop | 1 |
| `echo_deadline` folded into `next_deadline` | 1 |
| Client disconnect (`remove_client`) | 1 |
| `x-mosh-echo-v1` status line | 1 |

That is 7 added lines (the observe and paint lines are edits; the render-sent hook was dropped for deadline-driven emission). Running total: 12 today + 3a 1 (status) + 3b 4 (status, dispatch, ServerEvent variant, headless arm) + Phase 4 7 + the mosh-19g.5 mount line ≈ 25 of 30. The -G parse and start_ui_bridge changes live in src/mosh and cost no upstream line.

### 5. Acceptance and tests

- **Unit tests:**
  - Predictor epochs: confirm, wrong, epoch hiding, paste/resize reset, right-edge stop, and judging only at or above the ack revision (an ack ahead of its surface leaves predictions pending).
  - Mode thresholds.
  - Facade routing.
  - `EchoTracker`: count, emit-after-render, hold while a render is pending or deferred, subscription gating, disconnect cleanup.
- **Integration** (`tests/mosh_predict.rs`, harness link knob, 150 ms one-way, `HERDR_MOSH_PREDICT=on`):
  - Typed text appears and stays correct.
  - With an echo-suppressing program (`stty -echo; cat`), a typed character is painted, then cleared within 1 RTT plus one render after the ack.
  - With an old remote (capability stripped), there are no predictions and no subscribe.
  - Flood plus typing: a dense flood in one pane keeps the render slot backpressured while printable keys are typed into a shell; correct predictions are never cleared as wrong (no ack names a pre-echo revision).
- **Timing** (`#[ignore]`, `timing_lock()`): the median keypress-to-screen latency for 50 printable keys at 150 ms one-way is under 5 ms. It is measured from the predictor's trace timestamps (tap to the paint frame written), and cross-checked by the harness screen showing the character before 1 RTT.
- **Prediction off by default:** the existing suites run unchanged.

### 6. Out of scope

- Predicting Backspace and cursor movement.
- Predictions in alternate-screen applications beyond what content validation allows.
- Client-side RTT measurement independent of the bridge.

## Key abstractions

**EchoAck** — new value type in `predict.rs`: the one definition of the `x-mosh.echo-ack.v1` and `x-mosh.echo-subscribe.v1` kind strings and of the ack JSON `{pane_id, input_seq, surface_revision}`.
- It lives next to its consumer, as `LinkStatus` lives in `outage.rs`.
- `server_hooks` encodes it, `Predictor` decodes it, and `link_frames` builds the subscribe frame from it.
- No I/O. A malformed payload decodes to none.

**Client fork facade (`mosh_outage`, outage.rs)** — extended (root-defined).
- Mounting: still mounted only from client/mod.rs:33. `predict.rs` is mounted as its child (`#[path]`), costing no upstream line.
- One process-global mutex holds both `OutageState` and `Predictor`, so there is no lock order. All callers are on the client's current_thread runtime.
- `observe` is one closed `match kind`:
  - `LINK_KIND`: parse `LinkStatus` once. The outage state gets its state; `Predictor` gets `srtt_ms` and `echo`.
  - echo-ack kind: `Predictor`.
  - anything else: false.
  The observe hook line (client/mod.rs:2095) is edited, not added, to also pass the `generation` in scope.
- `paint` paints the banner, then the predictions. The hook line (composition.rs:723) is edited to also pass the retained surface, `pane_surface_generation` and `layout.pane_surface`.
- New entry points, each the body of one hook statement: `tap` and `blocks_fast_path`.

**Predictor** — extended (root-defined).
- Keying: state is keyed by `(ClientEndpointId, connection generation)`, then by pane. A new generation replaces the entry; this is the client-side reset, needing no `Welcome` hook. A mosh resume keeps the generation.
- Input classes: one closed match over `ClientPaneInputEvent`:
  - Key with `generated_text`, or TextCommit → Printable;
  - Paste → ClearPane;
  - key releases and modifier-only keys → Ignore, so report-all keyboard modes do not end an epoch on every key;
  - everything else → EpochBreak.
  Every event counts toward `sent`.
- Creation: predictions are created only when echo is on and the mode threshold is met; counting always runs. `adaptive` on a fast link therefore holds no predictions and keeps the fast path.
- Judging happens only in `paint`, where the surface and ack state are read together:
  - a prediction is judged once an ack with `input_seq` ≥ its seq exists and the surface revision is ≥ that ack's revision;
  - a pane whose `inner_rect` differs from the one recorded with its predictions is cleared (resize, with no hook).
- `paint` also moves `frame.cursor` past the last visible prediction.
- Failure: a prediction uncovered by any ack after max(1 s, 4×SRTT) clears the pane and turns prediction off for that generation. This covers an old still-running server under a new binary's capability line (status.rs:302 lists constants), and a lost subscribe.

**EchoTracker / server_hooks** — extended (root-defined, mosh-19g.5).
- A process-global mutex keyed by client id, holding per pane `received`, `acked` and a FIFO of `(count, received_at)`. It is never held across a channel send.
- Subscribe: an arm in 3b's dispatch entry (client_transport.rs:1349) creates the client's entry on the first subscribe and returns no `ServerEvent`; this costs no line. A repeated subscribe on the same connection is a no-op and keeps the counts.
- Counting: on receipt, on the reader thread (client_transport.rs:1196), for subscribed clients only. Receipt also covers events the main loop later drops (headless.rs:2365-2386), so the counts match the client's.
- Emission, on mosh's deadline:
  - An ack covers input received at least `ECHO_DELAY` (50 ms) earlier and carries the client's last committed `surface_revision`, via `render_state.last_pane_surface()`.
  - While a render for that client is pending or deferred (pending render slot occupied, pane dirty, or render held back by backpressure), the ack waits for a later tick; the last committed revision is used only when nothing is pending (§2).
  - With no baseline (just after a resync), the ack waits for a later tick. Acks go on the control lane.
  - It is driven by two main-loop statements in headless.rs: `echo_due` before step 8 (~:608), and `echo_deadline` folded into `next_deadline` (the loop otherwise wakes every 250 ms, :167).
- Disconnect: one statement in `remove_client` (headless.rs:946), which both detach and disconnect pass through.

**LinkStatus** — extended: `Linked` gains `echo: bool` (`serde(default)`, skipped when false). The bridge sets it once it has injected the subscribe. This is the per-endpoint path for "the remote advertises x-mosh-echo-v1".

**LocalBridge / link_frames** — extended: `encode_echo_subscribe_frame` sits beside `encode_resync_frame`. When the probe shows `x-mosh-echo-v1` and the predict mode is not `off`, the bridge injects the subscribe once per session through 3b's upstream `FrameSplitter` path, right after the first client frame (Hello), as offset-bearing DATA so it is replayed on a new leg.

**status.rs** — one tagged line, `x-mosh-echo-v1`.

**Variation:**
- Fork kinds on the client → the facade's closed match.
- Kinds on the server → 3b's dispatch match.
- Input classes → one match in `Predictor`.
- Mode → a closed enum, read once.
- `EchoTracker` is concrete.

**Walk-through** (mode `on`, 150 ms one-way):
1. The probe shows echo, and the bridge injects subscribe after Hello. The server marks the client subscribed before any pane input arrives, so both sides count from zero.
2. The first SRTT sample brings `Linked{srtt_ms:300, echo:true}`.
3. A keypress: the tap statement in the input loop counts it, places "a" at the cursor, recomposes, and returns the frame; the loop's final present writes it, well under 5 ms.
4. The server counts on receipt. 50 ms later, after the echo has rendered as revision R, a main-loop tick queues the ack `{pane, n, R}`.
5. An ack that overtakes R leaves the prediction pending. R arrives as a patch; the guard declines the fast path, compose runs, and paint confirms "a".
6. With `stty -echo`, R is the old surface, so the prediction is judged wrong and cleared.
- Future change, Backspace prediction: one class arm plus its placement rule inside `Predictor`. No hook, server or wire change.

**Corrections to the spec as first written** (applied in §§1–3 and the Goal):
- The input tap goes in the client input loop (shell_runtime.rs ~:811, before `for request in outcome.requests`, gated on `active_endpoint_online && pending_activation.is_none()`) and returns a recomposed frame.
  - `request_repaint` (client/state.rs:149) composes nothing.
  - Tapping in `push_target_event` would count requests that are later dropped (shell_runtime.rs:843-850).
- Echo-acks are deadline-driven (above), not sent after a render. Applying input is not echoing it, and input that causes no render would never be acked. A wrong prediction therefore clears within 1 RTT + 50 ms + one render.
- The bridge injects the subscribe; the client never sends it. The client learns `echo` only after the first SRTT sample, too late for counts to match. This keeps the root's "Predictor must not send anything".
- The fast-path guard declines while any prediction exists for the active endpoint, hidden ones included, so the deciding revision is always composed and judged.
- `unicode-width` is already a dependency (Cargo.toml:53).
- To verify during implementation: `pane_surface_generation` is numbered the same way as the registry's `connection.generation`.

Digest:
- EchoAck: owns the echo-ack and echo-subscribe kind strings and the ack JSON;
  new echo fields go here; no caller formats or parses these kinds itself.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*

### 2026-10-28: pane_surface_generation matches connection.generation (mosh-19g.3.2)

Verified read-only: `ClientShellState::pane_surface_generation` is copied from
`active_snapshot_generation` when a surface is installed, and that value is the
`connection.generation` that `install_client_shell_snapshot` passes to
`set_endpoint_snapshot_for_generation` (registry numbering, `Option<u64>`). The generation `observe`
receives at `client/mod.rs` (the `EndpointControl` arm) is the same registry value. So the
predictor is keyed by one number everywhere: `observe` and `tap` use the registry's
`connection.generation`, `paint` uses `pane_surface_generation`, and `paint` skips predictions
when the retained surface's generation is `None` or stale. No translation layer needed.

Hook deviation: the tap needs the retained surface, and `ClientShellState` has no accessor outside
`client::shell`. One extra tagged accessor line (`mosh_surface`, `shell/state.rs`) was added, so
this task adds three upstream lines (accessor, tap, fast-path guard) plus two edited hook lines;
the script reports 15 of 30.

### 2026-10-28: echo_due cannot read all render-pending state (mosh-19g.3.3)

Verify-first result. From the single `echo_due(&self.clients, now)` hook, `server_hooks` can read
`ClientConnection::render_pending` (deferred render under backpressure) and
`ClientRenderState::requires_recompute()`, plus `last_pane_surface()`. It cannot read the writer's
render slot occupancy (`ClientWriterQueue` is private to `client_transport.rs`) or the headless
loop's local `needs_render` (pane dirty / throttled). Reading them would take new upstream hook
lines, so the design-roast r2 rule is applied in part: the ack is withheld while `render_pending` or
a recompute is set. A queued render in the slot is harmless, because its revision is already the
committed one and the client holds the ack until its surface catches up (3.1). For the unreadable
dirty/throttled case, the fallback is used with a bound so input that renders nothing
(`stty -echo`) is still acked: an entry records the committed `surface_revision` when an echo pass
first sees it, and the ack is sent once the revision has advanced past that value, or 50 ms after
the 50 ms deadline if it has not. Worst-case ack latency without a render is 100 ms.

### 2026-10-28: echo-ack is judged per pane (mosh-19g.3.10)

Supersedes the 3.3 note's client-wide `revision_seen` fallback (that note is left as written).

Cause (confirmed with a temporary server-side trace and the client `predict.jsonl`, flood in pane 1,
typing in pane 2, 150 ms one-way, 1 MiB/s). "Echo rendered" was judged on the client-wide
`surface_revision`, which the flood bumps every frame. In the failing runs the wrong judgment's ack
was sent by the settle path (`rendered_since == false`, or a revision bumped by pane 1 only) with the
typed-into pane's `content_revision` unchanged since receipt: the committed surface trailed pane 2's
runtime because the render was throttled or backpressured (`render_pending` flipped true/false
around 100-250 ms stalls), and pane 2's echo reached a committed surface 40-150 ms after the 100 ms
settle bound. The pane id spaces match (`ClientShellPaneInput.pane_id` and `PaneSurfacePane.pane_id`
are both `w1:p2`-style ids).

Rule (`EchoTracker`, `src/mosh/server_hooks.rs`; no upstream line added):

- Each input entry records the typed-into pane's committed `content_revision` at the first echo pass
  that sees it (`pane_before`). A pass follows the input on the loop thread before any echo can exist,
  so this baseline cannot contain the echo. Stamping lazily, rather than from a per-client cache
  refreshed by earlier passes, avoids a stale cache (an idle client has no passes) attributing an
  earlier unrelated pane change to the new input.
- Rendered path: the oldest due entry is acked once the committed pane revision differs from
  `pane_before`; the ack names the committed client revision, which contains that change. Remaining
  entries are rebased to the current pane revision, so one echo cannot ack a later input; a later
  input whose echo landed in the same render is acked by the settle path. `render_pending` and recompute
  do not hold this path.
- Settle path (the pane never changed, `stty -echo`), after `ECHO_DELAY + SETTLE_GRACE`: when no other
  pane changed within 500 ms the entry is acked at once (still held while `render_pending` or a
  recompute is set), which keeps the 100 ms bound. Otherwise another pane is rendering and the
  committed surface may trail the runtimes, so the entry records the committed revision at the first
  such pass and is acked only after a revision past it (rendered after the grace pass on the loop
  thread, so it read the pane at least 100 ms after the input).
- A pass acks the newest entry it can justify, one ack per pane per pass.

No client change: every ack the server now sends is safe to judge as-is, and a client-side "a settle
ack cannot confirm" rule would stop `stty -echo` predictions from being refuted without a pane render.

Residual: a shell that takes over 100 ms to echo under a flood and where no other pane has changed in
500 ms is still refuted on the idle settle path; a flowing flood with an echo later than the first
post-grace render can still be refuted.

Results: `flood_plus_typing_never_clears_correct_predictions` passes 4/4 at 150 ms one-way and
1 MiB/s (16 of 16 predictions confirmed each run, `evidence/mosh-19g.3.10/flood-1mib.log`; before:
a wrong judgment in each traced run). The R8 stty-echo test clears within its bound (404-405 ms,
bound 500 ms; `stty.log`).

At 256 KiB/s the no-ack timeout still fires (`flood-256kib.log`). The server's ack leaves 55 ms
after the first input, but reaches the client 1.8-1.9 s after the tap: the rate-limited link holds
about 2 s of flood frames ahead of it, and the client's timeout is max(4 x srtt, 1 s) with srtt
300-465 ms (about 1.6 s). That delay is queueing in the link and render lane, not the echo rule.
Per the acceptance amendment, a timeout that disables prediction for the generation is acceptable
there; the 256 KiB/s test asserts 0 wrong judgments. Loaded-path latency (render-lane queue,
heartbeat SRTT understating the loaded path) is parked as a follow-up.
