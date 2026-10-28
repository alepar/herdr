//! Predictive local echo core: the `x-mosh.echo-ack.v1` payload and the per-pane prediction
//! state machine. Pure: no I/O, time and SRTT are inputs; the only environment read is
//! `PredictMode::get`. Mounted as a `#[path]` child of `outage.rs` (no upstream line).
//!
//! Owns: `EchoAck` and the echo kind/capability strings; the prediction mode and its single
//! read point; the (endpoint, connection generation) keying of prediction state.
//! Does not own: link transport, painting onto the frame, or the server-side ack emission.

// Some codec helpers stay unused until the server side (mosh-19g.3.3) and bridge probe land.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crossterm::event::KeyModifiers;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::client::endpoint::ClientEndpointId;
use crate::protocol::{
    ClientKeyCode, ClientKeyKind, ClientPaneInputEvent, PaneSurfaceFrame, SurfaceRect,
};

pub(crate) const ECHO_ACK_KIND: &str = "x-mosh.echo-ack.v1";
pub(crate) const ECHO_SUBSCRIBE_KIND: &str = "x-mosh.echo-subscribe.v1";
pub(crate) const ECHO_CAPABILITY: &str = "x-mosh-echo-v1";

const MODE_ENV: &str = "HERDR_MOSH_PREDICT";
const ADAPTIVE_THRESHOLD: Duration = Duration::from_millis(60);
const UNDERLINE_THRESHOLD: Duration = Duration::from_millis(160);
const MIN_ACK_TIMEOUT: Duration = Duration::from_secs(1);

/// The server's report that it applied `input_seq` events for `pane_id` and rendered
/// `surface_revision` after them.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EchoAck {
    pub pane_id: String,
    pub input_seq: u64,
    pub surface_revision: u64,
}

impl EchoAck {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn decode(data: &str) -> Option<EchoAck> {
        serde_json::from_str(data).ok()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PredictMode {
    Off,
    On,
    Adaptive,
}

impl PredictMode {
    fn parse(value: Option<&str>) -> PredictMode {
        match value {
            Some("on") => PredictMode::On,
            Some("adaptive") => PredictMode::Adaptive,
            _ => PredictMode::Off,
        }
    }

    /// `HERDR_MOSH_PREDICT` (`off` default | `on` | `adaptive`), read once per process.
    pub fn get() -> PredictMode {
        static MODE: OnceLock<PredictMode> = OnceLock::new();
        *MODE.get_or_init(|| PredictMode::parse(std::env::var(MODE_ENV).ok().as_deref()))
    }

    /// Whether a prediction may be created at this smoothed RTT.
    fn allows(self, srtt: Option<Duration>) -> bool {
        match self {
            PredictMode::Off => false,
            PredictMode::On => srtt.is_some(),
            PredictMode::Adaptive => srtt.is_some_and(|srtt| srtt > ADAPTIVE_THRESHOLD),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InputClass {
    /// Text the pane will echo at its cursor.
    Printable(String),
    /// Moves the cursor or changes echo semantics in a way we do not model.
    EpochBreak,
    /// Bulk input (paste): drop every outstanding prediction.
    ClearPane,
    /// Does not affect echo (key releases, modifier-only keys, mouse).
    Ignore,
}

fn classify(event: &ClientPaneInputEvent) -> InputClass {
    match event {
        ClientPaneInputEvent::Key {
            code,
            modifiers,
            kind,
            repeat_count,
            generated_text,
            ..
        } => {
            if *kind == ClientKeyKind::Release || *code == ClientKeyCode::Null {
                return InputClass::Ignore;
            }
            let mut mods = KeyModifiers::from_bits_truncate(*modifiers);
            mods.remove(KeyModifiers::SHIFT);
            if !mods.is_empty() || *repeat_count > 1 {
                return InputClass::EpochBreak;
            }
            let text = match (generated_text.as_deref(), code) {
                (Some(text), _) if !text.is_empty() => text.to_owned(),
                (_, ClientKeyCode::Char(c)) => c.to_string(),
                _ => return InputClass::EpochBreak,
            };
            printable_or_break(text)
        }
        ClientPaneInputEvent::TextCommit(text) => printable_or_break(text.clone()),
        ClientPaneInputEvent::Paste(_) => InputClass::ClearPane,
        ClientPaneInputEvent::Mouse { .. } => InputClass::Ignore,
    }
}

fn printable_or_break(text: String) -> InputClass {
    if text.is_empty() || text.chars().any(char::is_control) {
        InputClass::EpochBreak
    } else {
        InputClass::Printable(text)
    }
}

#[derive(Debug)]
struct Prediction {
    seq: u64,
    col: u16,
    row: u16,
    text: String,
    epoch: u64,
    created_at: Instant,
}

#[derive(Debug, Default)]
struct PaneState {
    sent: u64,
    ack: Option<(u64, u64)>,
    epoch: u64,
    hidden_epochs: HashSet<u64>,
    inner_rect: Option<SurfaceRect>,
    /// Where the next predicted character lands; `None` when unknown.
    cursor: Option<(u16, u16)>,
    predictions: Vec<Prediction>,
}

impl PaneState {
    /// Drops every prediction and starts a new epoch.
    fn clear(&mut self, hide_next: bool) {
        self.predictions.clear();
        self.hidden_epochs.clear();
        self.cursor = None;
        self.epoch += 1;
        if hide_next {
            self.hidden_epochs.insert(self.epoch);
        }
    }

    fn visible(&self, prediction: &Prediction) -> bool {
        !self.hidden_epochs.contains(&prediction.epoch)
    }
}

#[derive(Debug)]
struct EndpointState {
    generation: u64,
    srtt: Option<Duration>,
    echo: bool,
    off: bool,
    panes: HashMap<String, PaneState>,
}

impl EndpointState {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            srtt: None,
            echo: false,
            off: false,
            panes: HashMap::new(),
        }
    }
}

/// One predicted cell, relative to the pane surface origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaintCell {
    pub x: u16,
    pub y: u16,
    pub symbol: String,
    pub underline: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PaintOutput {
    pub cells: Vec<PaintCell>,
    pub cursor: Option<(u16, u16)>,
    /// What judging this paint decided, for the trace.
    pub judgments: Vec<Judgment>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Judgment {
    Confirmed,
    Wrong,
    Timeout,
}

#[derive(Debug)]
pub(crate) struct Predictor {
    mode: PredictMode,
    endpoints: HashMap<ClientEndpointId, EndpointState>,
}

impl Predictor {
    pub fn new(mode: PredictMode) -> Predictor {
        Predictor {
            mode,
            endpoints: HashMap::new(),
        }
    }

    /// State for `generation`; a newer generation replaces the old state (counts reset only
    /// then), a stale one yields `None`.
    fn state(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
    ) -> Option<&mut EndpointState> {
        let state = self
            .endpoints
            .entry(endpoint.clone())
            .or_insert_with(|| EndpointState::new(generation));
        if generation > state.generation {
            *state = EndpointState::new(generation);
        }
        (generation == state.generation).then_some(state)
    }

    pub fn on_link(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        srtt_ms: Option<u64>,
        echo: bool,
    ) {
        let Some(state) = self.state(endpoint, generation) else {
            return;
        };
        state.srtt = srtt_ms.map(Duration::from_millis);
        state.echo = echo;
        if !echo {
            for pane in state.panes.values_mut() {
                pane.clear(false);
            }
        }
    }

    pub fn on_echo_ack(&mut self, endpoint: &ClientEndpointId, generation: u64, ack: EchoAck) {
        let Some(state) = self.state(endpoint, generation) else {
            return;
        };
        let pane = state.panes.entry(ack.pane_id).or_default();
        if pane.ack.is_none_or(|(seq, _)| ack.input_seq >= seq) {
            pane.ack = Some((ack.input_seq, ack.surface_revision));
        }
    }

    /// Counts every event (`sent += events.len()`) and creates predictions for printable
    /// ones when allowed. Returns true when a prediction (visible or hidden) was added.
    pub fn tap(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        pane_id: &str,
        events: &[ClientPaneInputEvent],
        surface: Option<&PaneSurfaceFrame>,
        now: Instant,
    ) -> bool {
        let mode = self.mode;
        let Some(state) = self.state(endpoint, generation) else {
            return false;
        };
        let may_predict = state.echo && !state.off && mode.allows(state.srtt);
        let pane = state.panes.entry(pane_id.to_owned()).or_default();
        let surface_pane = surface.and_then(|surface| {
            surface
                .panes
                .iter()
                .find(|candidate| candidate.pane_id == pane_id)
                .map(|candidate| (surface, candidate.inner_rect))
        });
        if let Some((_, rect)) = surface_pane {
            if pane.inner_rect.is_some_and(|known| known != rect) {
                pane.clear(mode == PredictMode::Adaptive);
            }
        }
        let mut added = false;
        for event in events {
            let before = pane.sent;
            pane.sent += 1;
            match classify(event) {
                InputClass::Ignore => {}
                InputClass::ClearPane => pane.clear(mode == PredictMode::Adaptive),
                InputClass::EpochBreak => {
                    pane.epoch += 1;
                    if mode == PredictMode::Adaptive {
                        pane.hidden_epochs.insert(pane.epoch);
                    }
                    pane.cursor = None;
                }
                InputClass::Printable(text) => {
                    let Some((surface, rect)) = surface_pane else {
                        continue;
                    };
                    if !may_predict {
                        continue;
                    }
                    added |= predict_text(pane, surface, rect, &text, before, now);
                }
            }
        }
        added
    }

    /// Judges predictions against `surface` (the only place judging happens), applies the
    /// no-ack timeout, and returns the cells and cursor to paint.
    pub fn paint(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        surface: &PaneSurfaceFrame,
        now: Instant,
    ) -> PaintOutput {
        let mode = self.mode;
        let Some(state) = self.state(endpoint, generation) else {
            return PaintOutput::default();
        };
        let underline = state.srtt.is_some_and(|srtt| srtt > UNDERLINE_THRESHOLD);
        let timeout = state
            .srtt
            .map_or(MIN_ACK_TIMEOUT, |srtt| (srtt * 4).max(MIN_ACK_TIMEOUT));
        let mut timed_out = false;
        let mut output = PaintOutput::default();
        let mut cursor_focused = false;
        for (pane_id, pane) in &mut state.panes {
            if pane.predictions.is_empty() {
                continue;
            }
            let Some(surface_pane) = surface.panes.iter().find(|p| &p.pane_id == pane_id) else {
                pane.clear(mode == PredictMode::Adaptive);
                continue;
            };
            if pane.inner_rect != Some(surface_pane.inner_rect) {
                pane.clear(mode == PredictMode::Adaptive);
                continue;
            }
            let outstanding = pane.predictions.len();
            if !judge(pane, surface) {
                pane.clear(true);
                output.judgments.push(Judgment::Wrong);
                continue;
            }
            if pane.predictions.len() < outstanding {
                output.judgments.push(Judgment::Confirmed);
            }
            if pane
                .predictions
                .iter()
                .any(|p| now.saturating_duration_since(p.created_at) > timeout)
            {
                pane.clear(false);
                timed_out = true;
                continue;
            }
            let mut visible = false;
            for prediction in &pane.predictions {
                if pane.visible(prediction) {
                    visible = true;
                    output.cells.push(PaintCell {
                        x: prediction.col,
                        y: prediction.row,
                        symbol: prediction.text.clone(),
                        underline,
                    });
                }
            }
            if visible && (!cursor_focused || surface_pane.focused) {
                cursor_focused = surface_pane.focused;
                let right = surface_pane
                    .inner_rect
                    .x
                    .saturating_add(surface_pane.inner_rect.width.saturating_sub(1));
                output.cursor = pane.cursor.map(|(x, y)| (x.min(right), y));
            }
        }
        if timed_out {
            state.off = true;
            for pane in state.panes.values_mut() {
                pane.clear(false);
            }
            return PaintOutput {
                judgments: vec![Judgment::Timeout],
                ..PaintOutput::default()
            };
        }
        output
    }

    pub fn any_prediction(&self, endpoint: &ClientEndpointId) -> bool {
        self.endpoints.get(endpoint).is_some_and(|state| {
            state
                .panes
                .values()
                .any(|pane| !pane.predictions.is_empty())
        })
    }
}

/// Adds one prediction per grapheme of `text` at the pane's (predicted) cursor. `before` is
/// the pane's sent count before this event; the event's own seq is `before + 1`.
fn predict_text(
    pane: &mut PaneState,
    surface: &PaneSurfaceFrame,
    rect: SurfaceRect,
    text: &str,
    before: u64,
    now: Instant,
) -> bool {
    let start = if pane.predictions.is_empty() {
        // Nothing outstanding: trust the real cursor only once every earlier event is
        // acked and rendered, or it may not reflect them yet.
        let settled = match pane.ack {
            Some((seq, revision)) => seq >= before && surface.surface_revision >= revision,
            None => before == 0,
        };
        let cursor = surface.frame.cursor.as_ref().filter(|cursor| cursor.visible);
        match cursor {
            Some(cursor) if settled => Some((cursor.x, cursor.y)),
            _ => None,
        }
    } else {
        pane.cursor
    };
    pane.inner_rect = Some(rect);
    let Some((mut col, row)) = start else {
        pane.cursor = None;
        return false;
    };
    let right = u32::from(rect.x) + u32::from(rect.width);
    let bottom = u32::from(rect.y) + u32::from(rect.height);
    let inside = col >= rect.x && row >= rect.y && u32::from(row) < bottom;
    let mut added = false;
    if inside {
        for grapheme in text.graphemes(true) {
            let width = u16::try_from(UnicodeWidthStr::width(grapheme)).unwrap_or(0);
            if width == 0 || u32::from(col) + u32::from(width) > right {
                // Zero-width cannot be placed; the right edge stops prediction (the real
                // terminal wraps, which we do not model).
                pane.cursor = None;
                return added;
            }
            pane.predictions.push(Prediction {
                seq: before + 1,
                col,
                row,
                text: grapheme.to_owned(),
                epoch: pane.epoch,
                created_at: now,
            });
            col += width;
            added = true;
        }
    }
    pane.cursor = inside.then_some((col, row));
    added
}

/// Judges every prediction an ack covers. Returns false when one is wrong.
fn judge(pane: &mut PaneState, surface: &PaneSurfaceFrame) -> bool {
    let Some((ack_seq, ack_revision)) = pane.ack else {
        return true;
    };
    if surface.surface_revision < ack_revision {
        return true;
    }
    let width = usize::from(surface.frame.width);
    let mut confirmed_epochs = Vec::new();
    let mut ok = true;
    pane.predictions.retain(|prediction| {
        if !ok || prediction.seq > ack_seq {
            return true;
        }
        let held = width > 0
            && prediction.col < surface.frame.width
            && surface
                .frame
                .cells
                .get(usize::from(prediction.row) * width + usize::from(prediction.col))
                .is_some_and(|cell| cell.symbol == prediction.text);
        if held {
            confirmed_epochs.push(prediction.epoch);
        } else {
            ok = false;
        }
        !held
    });
    for epoch in confirmed_epochs {
        pane.hidden_epochs.remove(&epoch);
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        CellData, ClientMouseKind, ClientMousePosition, CursorState, FrameData,
        SurfaceGraphicsScene,
    };

    const GEN: u64 = 7;
    const W: u16 = 10;

    fn ep() -> ClientEndpointId {
        ClientEndpointId::Local
    }

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.to_owned(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn rect(x: u16, y: u16, width: u16, height: u16) -> SurfaceRect {
        SurfaceRect {
            x,
            y,
            width,
            height,
        }
    }

    /// One pane "p" covering the whole 10x3 surface unless `inner` says otherwise.
    fn surface_with(
        revision: u64,
        cursor: Option<(u16, u16)>,
        inner: SurfaceRect,
        cells: &[(u16, u16, &str)],
    ) -> PaneSurfaceFrame {
        let mut data = vec![cell(" "); usize::from(W) * 3];
        for (x, y, symbol) in cells {
            data[usize::from(*y) * usize::from(W) + usize::from(*x)] = cell(symbol);
        }
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: revision,
            frame: FrameData {
                cells: data,
                width: W,
                height: 3,
                cursor: cursor.map(|(x, y)| CursorState {
                    x,
                    y,
                    visible: true,
                    shape: 0,
                }),
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: vec![crate::protocol::PaneSurfacePane {
                pane_id: "p".into(),
                content_revision: 1,
                rect: inner,
                inner_rect: inner,
                scrollbar_rect: None,
                scroll: None,
                focused: true,
                mouse_reporting: false,
                sgr_pixel_mouse: false,
                alternate_screen_active: false,
                pixel_width: 0,
                pixel_height: 0,
            }],
            splits: Vec::new(),
            popup: None,
            graphics: SurfaceGraphicsScene::default(),
        }
    }

    fn surface(revision: u64, cursor: (u16, u16), cells: &[(u16, u16, &str)]) -> PaneSurfaceFrame {
        surface_with(revision, Some(cursor), rect(0, 0, W, 3), cells)
    }

    fn key_with(code: ClientKeyCode, modifiers: u8, kind: ClientKeyKind) -> ClientPaneInputEvent {
        ClientPaneInputEvent::Key {
            code,
            modifiers,
            kind,
            repeat_count: 1,
            shifted_codepoint: None,
            generated_text: None,
            tracks_release: false,
            physical_key_id: None,
            windows_record: None,
        }
    }

    fn ch(c: char) -> ClientPaneInputEvent {
        key_with(ClientKeyCode::Char(c), 0, ClientKeyKind::Press)
    }

    fn enter() -> ClientPaneInputEvent {
        key_with(ClientKeyCode::Enter, 0, ClientKeyKind::Press)
    }

    fn text(s: &str) -> ClientPaneInputEvent {
        ClientPaneInputEvent::TextCommit(s.to_owned())
    }

    fn ack(seq: u64, revision: u64) -> EchoAck {
        EchoAck {
            pane_id: "p".into(),
            input_seq: seq,
            surface_revision: revision,
        }
    }

    /// A predictor with echo on and the given SRTT.
    fn predictor(mode: PredictMode, srtt_ms: Option<u64>) -> Predictor {
        let mut predictor = Predictor::new(mode);
        predictor.on_link(&ep(), GEN, srtt_ms, true);
        predictor
    }

    fn type_events(
        predictor: &mut Predictor,
        events: &[ClientPaneInputEvent],
        surface: &PaneSurfaceFrame,
        now: Instant,
    ) -> bool {
        predictor.tap(&ep(), GEN, "p", events, Some(surface), now)
    }

    fn painted(output: &PaintOutput) -> Vec<(u16, u16, &str)> {
        output
            .cells
            .iter()
            .map(|c| (c.x, c.y, c.symbol.as_str()))
            .collect()
    }

    #[test]
    fn input_classes() {
        assert_eq!(classify(&ch('a')), InputClass::Printable("a".into()));
        let mut shifted = key_with(ClientKeyCode::Char('a'), 1, ClientKeyKind::Press);
        if let ClientPaneInputEvent::Key { generated_text, .. } = &mut shifted {
            *generated_text = Some("A".into());
        }
        assert_eq!(classify(&shifted), InputClass::Printable("A".into()));
        assert_eq!(classify(&text("hi")), InputClass::Printable("hi".into()));
        assert_eq!(
            classify(&ClientPaneInputEvent::Paste("x".into())),
            InputClass::ClearPane
        );
        // A release must not predict a second character.
        assert_eq!(
            classify(&key_with(
                ClientKeyCode::Char('a'),
                0,
                ClientKeyKind::Release
            )),
            InputClass::Ignore
        );
        assert_eq!(
            classify(&key_with(ClientKeyCode::Null, 0, ClientKeyKind::Press)),
            InputClass::Ignore
        );
        assert_eq!(classify(&enter()), InputClass::EpochBreak);
        assert_eq!(
            classify(&key_with(ClientKeyCode::Left, 0, ClientKeyKind::Press)),
            InputClass::EpochBreak
        );
        // Ctrl+C is not text.
        assert_eq!(
            classify(&key_with(ClientKeyCode::Char('c'), 4, ClientKeyKind::Press)),
            InputClass::EpochBreak
        );
        assert_eq!(
            classify(&text("a\nb")),
            InputClass::EpochBreak,
            "control characters are not echoed text"
        );
        assert_eq!(
            classify(&ClientPaneInputEvent::Mouse {
                kind: ClientMouseKind::Moved,
                position: ClientMousePosition::Cell { column: 0, row: 0 },
                geometry: None,
                modifiers: 0,
                lines: 0,
            }),
            InputClass::Ignore
        );
    }

    #[test]
    fn typing_predicts_then_ack_and_surface_confirm() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        assert!(type_events(&mut p, &[ch('a'), ch('b')], &surface(1, (0, 0), &[]), now));
        let out = p.paint(&ep(), GEN, &surface(1, (0, 0), &[]), now);
        assert_eq!(painted(&out), vec![(0, 0, "a"), (1, 0, "b")]);
        assert_eq!(out.cursor, Some((2, 0)));
        assert!(p.any_prediction(&ep()));
        p.on_echo_ack(&ep(), GEN, ack(2, 2));
        let out = p.paint(&ep(), GEN, &surface(2, (2, 0), &[(0, 0, "a"), (1, 0, "b")]), now);
        assert!(out.cells.is_empty());
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn epoch_break_and_hidden_epoch_then_confirm_reveals() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::Adaptive, Some(100));
        // The first epoch is visible.
        type_events(&mut p, &[ch('a')], &surface(1, (0, 0), &[]), now);
        assert_eq!(painted(&p.paint(&ep(), GEN, &surface(1, (0, 0), &[]), now)).len(), 1);
        p.on_echo_ack(&ep(), GEN, ack(1, 2));
        p.paint(&ep(), GEN, &surface(2, (1, 0), &[(0, 0, "a")]), now);
        // Enter breaks the epoch; the next one is hidden until one prediction confirms.
        type_events(&mut p, &[enter()], &surface(2, (1, 0), &[(0, 0, "a")]), now);
        p.on_echo_ack(&ep(), GEN, ack(2, 3));
        let s3 = surface(3, (0, 1), &[(0, 0, "a")]);
        assert!(type_events(&mut p, &[ch('b')], &s3, now), "hidden still counts as added");
        assert!(p.any_prediction(&ep()));
        assert!(p.paint(&ep(), GEN, &s3, now).cells.is_empty(), "hidden epoch paints nothing");
        p.on_echo_ack(&ep(), GEN, ack(3, 4));
        let s4 = surface(4, (1, 1), &[(0, 0, "a"), (0, 1, "b")]);
        assert!(p.paint(&ep(), GEN, &s4, now).cells.is_empty());
        assert!(!p.any_prediction(&ep()), "confirmed");
        // Confirmation revealed the epoch: the next prediction in it is painted.
        type_events(&mut p, &[ch('c')], &s4, now);
        assert_eq!(painted(&p.paint(&ep(), GEN, &s4, now)), vec![(1, 1, "c")]);
    }

    #[test]
    fn on_mode_does_not_hide_epochs() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        type_events(&mut p, &[enter()], &surface(1, (0, 0), &[]), now);
        p.on_echo_ack(&ep(), GEN, ack(1, 1));
        let s = surface(1, (0, 1), &[]);
        type_events(&mut p, &[ch('b')], &s, now);
        assert_eq!(painted(&p.paint(&ep(), GEN, &s, now)), vec![(0, 1, "b")]);
    }

    #[test]
    fn unacked_unpredicted_event_blocks_prediction_from_stale_cursor() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        // Enter is in flight, so the surface cursor does not reflect it yet.
        assert!(!type_events(&mut p, &[enter(), ch('x')], &s, now));
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn paste_clears_pane() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a')], &s, now);
        assert!(p.any_prediction(&ep()));
        type_events(&mut p, &[ClientPaneInputEvent::Paste("long".into())], &s, now);
        assert!(!p.any_prediction(&ep()));
        assert!(p.paint(&ep(), GEN, &s, now).cells.is_empty());
    }

    #[test]
    fn rect_change_clears_pane() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a')], &s, now);
        let resized = surface_with(1, Some((0, 0)), rect(0, 0, 8, 3), &[]);
        let out = p.paint(&ep(), GEN, &resized, now);
        assert!(out.cells.is_empty());
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn right_edge_stops_prediction() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (W - 2, 0), &[]);
        type_events(&mut p, &[ch('a'), ch('b'), ch('c')], &s, now);
        let out = p.paint(&ep(), GEN, &s, now);
        assert_eq!(painted(&out), vec![(W - 2, 0, "a"), (W - 1, 0, "b")]);
        assert_eq!(out.cursor, None, "past the edge the real cursor is left alone");
    }

    #[test]
    fn wide_characters_advance_two() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[text("世"), ch('a')], &s, now);
        let out = p.paint(&ep(), GEN, &s, now);
        assert_eq!(painted(&out), vec![(0, 0, "世"), (2, 0, "a")]);
        assert_eq!(out.cursor, Some((3, 0)));
        // A wide character that does not fit stops at the edge.
        let mut p = predictor(PredictMode::On, Some(100));
        let edge = surface(1, (W - 1, 0), &[]);
        assert!(!type_events(&mut p, &[text("世")], &edge, now));
    }

    #[test]
    fn ack_ahead_of_surface_leaves_prediction_pending() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s1 = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a')], &s1, now);
        p.on_echo_ack(&ep(), GEN, ack(1, 5));
        // The surface predates the ack's render: not judged, still painted, not wrong.
        let out = p.paint(&ep(), GEN, &surface(4, (0, 0), &[]), now);
        assert_eq!(painted(&out), vec![(0, 0, "a")]);
        assert!(p.any_prediction(&ep()));
        // Once the surface catches up and holds the text, it confirms.
        let out = p.paint(&ep(), GEN, &surface(5, (1, 0), &[(0, 0, "a")]), now);
        assert!(out.cells.is_empty());
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn wrong_prediction_clears_pane() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s1 = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a'), ch('b')], &s1, now);
        p.on_echo_ack(&ep(), GEN, ack(1, 2));
        // The pane echoed 'z', not 'a': everything is dropped, including 'b'.
        let out = p.paint(&ep(), GEN, &surface(2, (1, 0), &[(0, 0, "z")]), now);
        assert!(out.cells.is_empty());
        assert!(!p.any_prediction(&ep()));
        // The epoch after a wrong prediction is hidden until confirmed.
        p.on_echo_ack(&ep(), GEN, ack(2, 3));
        let s3 = surface(3, (2, 0), &[(0, 0, "z"), (1, 0, "b")]);
        type_events(&mut p, &[ch('c')], &s3, now);
        assert!(p.any_prediction(&ep()));
        assert!(p.paint(&ep(), GEN, &s3, now).cells.is_empty());
    }

    #[test]
    fn uncovered_prediction_times_out_and_turns_off() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a')], &s, now);
        // 100 ms SRTT: the deadline is the 1 s floor, not 4 x SRTT.
        let out = p.paint(&ep(), GEN, &s, now + Duration::from_millis(900));
        assert_eq!(out.cells.len(), 1);
        let out = p.paint(&ep(), GEN, &s, now + Duration::from_millis(1100));
        assert!(out.cells.is_empty());
        assert!(!p.any_prediction(&ep()));
        // Off for the generation, even though echo and SRTT still allow it.
        assert!(!type_events(&mut p, &[ch('b')], &s, now + Duration::from_secs(2)));
        // A new generation turns it back on.
        p.on_link(&ep(), GEN + 1, Some(100), true);
        assert!(p.tap(&ep(), GEN + 1, "p", &[ch('c')], Some(&s), now));
    }

    #[test]
    fn timeout_scales_with_srtt() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(500));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a')], &s, now);
        // 4 x 500 ms = 2 s.
        assert_eq!(p.paint(&ep(), GEN, &s, now + Duration::from_millis(1900)).cells.len(), 1);
        assert!(p.paint(&ep(), GEN, &s, now + Duration::from_millis(2100)).cells.is_empty());
    }

    #[test]
    fn generation_change_resets_counts() {
        let now = Instant::now();
        let mut p = predictor(PredictMode::On, Some(100));
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[enter()], &s, now);
        // Enter is unacked, so 'x' is not predicted on generation GEN.
        assert!(!type_events(&mut p, &[ch('x')], &s, now));
        // A new generation starts from zero sent and no ack: prediction works again.
        p.on_link(&ep(), GEN + 1, Some(100), true);
        assert!(p.tap(&ep(), GEN + 1, "p", &[ch('x')], Some(&s), now));
        // Events from the superseded generation are ignored.
        assert!(!type_events(&mut p, &[ch('y')], &s, now));
        assert_eq!(painted(&p.paint(&ep(), GEN + 1, &s, now)), vec![(0, 0, "x")]);
    }

    #[test]
    fn mode_thresholds() {
        let now = Instant::now();
        let s = surface(1, (0, 0), &[]);
        let predicts = |mode, srtt| {
            let mut p = predictor(mode, srtt);
            type_events(&mut p, &[ch('a')], &s, now)
        };
        assert!(!predicts(PredictMode::Off, Some(500)));
        assert!(predicts(PredictMode::On, Some(10)));
        assert!(!predicts(PredictMode::On, None), "unknown SRTT");
        assert!(!predicts(PredictMode::Adaptive, Some(50)));
        assert!(!predicts(PredictMode::Adaptive, Some(60)), "strictly above 60 ms");
        assert!(predicts(PredictMode::Adaptive, Some(70)));
        let underlined = |srtt| {
            let mut p = predictor(PredictMode::On, Some(srtt));
            type_events(&mut p, &[ch('a')], &s, now);
            p.paint(&ep(), GEN, &s, now).cells[0].underline
        };
        assert!(!underlined(150));
        assert!(!underlined(160), "strictly above 160 ms");
        assert!(underlined(170));
    }

    #[test]
    fn mode_parse() {
        assert_eq!(PredictMode::parse(None), PredictMode::Off);
        assert_eq!(PredictMode::parse(Some("on")), PredictMode::On);
        assert_eq!(PredictMode::parse(Some("adaptive")), PredictMode::Adaptive);
        assert_eq!(PredictMode::parse(Some("off")), PredictMode::Off);
        assert_eq!(PredictMode::parse(Some("ON")), PredictMode::Off);
        assert_eq!(PredictMode::parse(Some("")), PredictMode::Off);
    }

    #[test]
    fn echo_ack_round_trips() {
        let original = ack(42, 9);
        let json = original.encode();
        assert_eq!(
            json,
            r#"{"pane_id":"p","input_seq":42,"surface_revision":9}"#
        );
        assert_eq!(EchoAck::decode(&json), Some(original));
    }

    #[test]
    fn malformed_echo_ack_is_none() {
        assert_eq!(EchoAck::decode(""), None);
        assert_eq!(EchoAck::decode("not json"), None);
        assert_eq!(EchoAck::decode(r#"{"pane_id":"p","input_seq":1}"#), None);
        assert_eq!(
            EchoAck::decode(r#"{"pane_id":"p","input_seq":-1,"surface_revision":1}"#),
            None
        );
    }

    #[test]
    fn no_predictions_without_echo() {
        let now = Instant::now();
        let mut p = Predictor::new(PredictMode::On);
        p.on_link(&ep(), GEN, Some(100), false);
        let s = surface(1, (0, 0), &[]);
        assert!(!type_events(&mut p, &[ch('a')], &s, now));
        assert!(!p.any_prediction(&ep()));
        // Turning echo off drops what exists.
        p.on_link(&ep(), GEN, Some(100), true);
        p.on_echo_ack(&ep(), GEN, ack(1, 1));
        assert!(type_events(&mut p, &[ch('a')], &s, now));
        p.on_link(&ep(), GEN, Some(100), false);
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn counting_runs_without_prediction() {
        let now = Instant::now();
        // Echo off: events still count, so a later prediction carries the right seq.
        let mut p = Predictor::new(PredictMode::On);
        p.on_link(&ep(), GEN, Some(100), false);
        let s = surface(1, (0, 0), &[]);
        type_events(&mut p, &[ch('a'), ch('b'), ch('c')], &s, now);
        p.on_link(&ep(), GEN, Some(100), true);
        p.on_echo_ack(&ep(), GEN, ack(3, 1));
        assert!(type_events(&mut p, &[ch('d')], &s, now));
        // An ack for 3 events must not cover the 4th event's prediction.
        let out = p.paint(&ep(), GEN, &surface(1, (0, 0), &[]), now);
        assert_eq!(painted(&out), vec![(0, 0, "d")]);
        p.on_echo_ack(&ep(), GEN, ack(4, 2));
        p.paint(&ep(), GEN, &surface(2, (1, 0), &[(0, 0, "d")]), now);
        assert!(!p.any_prediction(&ep()));
    }

    #[test]
    fn echo_constants_are_stable() {
        assert_eq!(ECHO_ACK_KIND, "x-mosh.echo-ack.v1");
        assert_eq!(ECHO_SUBSCRIBE_KIND, "x-mosh.echo-subscribe.v1");
        assert_eq!(ECHO_CAPABILITY, "x-mosh-echo-v1");
    }
}
