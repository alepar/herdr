//! Outage banner: the `x-mosh.link.v1` payload and per-endpoint banner state; mounted from
//! `client/mod.rs` as `client::mosh_outage`. Does not detect outages: the local bridge does.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::client::endpoint::{ClientEndpointId, EndpointRegistry};
use crate::client::frame_output::ComposedFrame;
use crate::client::shell::ClientShellState;
use crate::protocol::{
    color_to_u32, modifier_to_u16, ClientMessage, FrameData, PaneSurfaceFrame,
};
use predict::{EchoAck, Judgment, PredictMode, Predictor};

/// Server-side fork hooks (mosh-19g.5). Mounted here because this is the only fork mount the
/// server can name; costs no upstream line. Reached as `crate::client::mosh_outage::server_hooks`.
#[path = "server_hooks.rs"]
pub(crate) mod server_hooks;

/// Predictive echo core (mosh-19g.3.1): `EchoAck` codec and the `Predictor` state machine.
#[path = "predict.rs"]
pub(crate) mod predict;

pub(crate) const LINK_KIND: &str = "x-mosh.link.v1";
const STALE_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum LinkStatus {
    Resuming { last_contact_ms: u64 },
    Linked {
        /// Smoothed RTT of the link in ms; absent while unknown (and from older bridges).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        srtt_ms: Option<u64>,
        /// The far side acknowledges echo (`x-mosh-echo-v1`); absent when false and from older
        /// bridges.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        echo: bool,
    },
}

struct Resuming {
    last_contact_ms: u64,
    observed_at: Instant,
}

pub(crate) struct OutageState {
    resuming: BTreeMap<ClientEndpointId, Resuming>,
}

impl OutageState {
    #[cfg(test)]
    fn observe_at(
        &mut self,
        endpoint: &ClientEndpointId,
        kind: &str,
        data: &str,
        now: Instant,
    ) -> bool {
        if kind != LINK_KIND {
            return false;
        }
        if let Ok(status) = serde_json::from_str::<LinkStatus>(data) {
            self.apply(endpoint, status, now);
        }
        true
    }

    fn apply(&mut self, endpoint: &ClientEndpointId, status: LinkStatus, now: Instant) {
        match status {
            LinkStatus::Resuming { last_contact_ms } => {
                self.resuming.insert(
                    endpoint.clone(),
                    Resuming {
                        last_contact_ms,
                        observed_at: now,
                    },
                );
            }
            LinkStatus::Linked { .. } => {
                self.resuming.remove(endpoint);
            }
        }
    }

    fn paint_at(
        &mut self,
        frame: &mut FrameData,
        active: &ClientEndpointId,
        label: &str,
        now: Instant,
    ) {
        self.resuming
            .retain(|_, entry| now.saturating_duration_since(entry.observed_at) < STALE_AFTER);
        let Some(entry) = self.resuming.get(active) else {
            return;
        };
        let width = usize::from(frame.width);
        if width == 0 || frame.height == 0 || frame.cells.len() < width {
            return;
        }
        let text = format!(
            "⚠ {label} unreachable — last contact {}s ago, resuming…",
            entry.last_contact_ms / 1000
        );
        let reset = color_to_u32(Color::Reset);
        let modifier = modifier_to_u16(Modifier::REVERSED);
        let mut symbols: Vec<&str> = Vec::with_capacity(width);
        for grapheme in text.graphemes(true) {
            let cells = grapheme.width();
            if symbols.len() + cells > width {
                break;
            }
            symbols.push(grapheme);
            if cells == 2 {
                symbols.push("");
            }
        }
        for (column, cell) in frame.cells[..width].iter_mut().enumerate() {
            cell.symbol.clear();
            cell.symbol.push_str(symbols.get(column).copied().unwrap_or(" "));
            cell.fg = reset;
            cell.bg = reset;
            cell.modifier = modifier;
            cell.skip = false;
            cell.hyperlink = None;
        }
    }
}

/// The fork's whole client-side state behind one lock: the outage banner and the predictor.
/// Never held across a call back into the shell (`compose` re-enters `paint`).
struct Facade {
    outage: OutageState,
    predictor: Option<Predictor>,
}

static STATE: Mutex<Facade> = Mutex::new(Facade {
    outage: OutageState {
        resuming: BTreeMap::new(),
    },
    predictor: None,
});

fn facade() -> std::sync::MutexGuard<'static, Facade> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Facade {
    fn predictor(&mut self) -> &mut Predictor {
        self.predictor
            .get_or_insert_with(|| Predictor::new(PredictMode::get()))
    }

    fn observe_at(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        kind: &str,
        data: &str,
        now: Instant,
    ) -> bool {
        match kind {
            LINK_KIND => {
                if let Ok(status) = serde_json::from_str::<LinkStatus>(data) {
                    self.outage.apply(endpoint, status, now);
                    if let LinkStatus::Linked { srtt_ms, echo } = status {
                        self.predictor().on_link(endpoint, generation, srtt_ms, echo);
                    }
                }
                true
            }
            predict::ECHO_ACK_KIND => {
                if let Some(ack) = EchoAck::decode(data) {
                    self.predictor().on_echo_ack(endpoint, generation, ack);
                }
                true
            }
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_at(
        &mut self,
        frame: &mut FrameData,
        active: &ClientEndpointId,
        label: &str,
        surface: Option<&PaneSurfaceFrame>,
        surface_generation: Option<u64>,
        pane_area: Rect,
        now: Instant,
    ) {
        self.outage.paint_at(frame, active, label, now);
        let (Some(surface), Some(generation)) = (surface, surface_generation) else {
            return;
        };
        let Some(predictor) = self.predictor.as_mut() else {
            return;
        };
        let output = predictor.paint(active, generation, surface, now);
        for judgment in &output.judgments {
            trace(
                "judge",
                serde_json::json!({ "result": match judgment {
                    Judgment::Confirmed => "confirmed",
                    Judgment::Wrong => "wrong",
                    Judgment::Timeout => "timeout",
                } }),
            );
        }
        if output.cells.is_empty() {
            return;
        }
        let width = usize::from(frame.width);
        let underline = modifier_to_u16(Modifier::UNDERLINED);
        for painted in &output.cells {
            let (Some(x), Some(y)) = (
                pane_area.x.checked_add(painted.x),
                pane_area.y.checked_add(painted.y),
            ) else {
                continue;
            };
            if painted.x >= pane_area.width || painted.y >= pane_area.height || x >= frame.width {
                continue;
            }
            let index = usize::from(y) * width + usize::from(x);
            let Some(cell) = frame.cells.get_mut(index) else {
                continue;
            };
            cell.symbol.clear();
            cell.symbol.push_str(&painted.symbol);
            cell.skip = false;
            cell.hyperlink = None;
            if painted.underline {
                cell.modifier |= underline;
            }
            if painted.symbol.width() == 2 && x + 1 < frame.width {
                if let Some(next) = frame.cells.get_mut(index + 1) {
                    next.symbol.clear();
                    next.skip = false;
                    next.hyperlink = None;
                }
            }
        }
        if let (Some((x, y)), Some(cursor)) = (output.cursor, frame.cursor.as_mut()) {
            cursor.x = pane_area.x.saturating_add(x);
            cursor.y = pane_area.y.saturating_add(y);
        }
        trace(
            "paint",
            serde_json::json!({ "predictions": output.cells.len() }),
        );
    }

    /// Counts every pane-input request and predicts; true when any prediction was added.
    fn tap_at(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        requests: &[ClientMessage],
        surface: Option<&PaneSurfaceFrame>,
        now: Instant,
    ) -> bool {
        let predictor = self.predictor();
        let mut added = false;
        for request in requests {
            let ClientMessage::ClientShellPaneInput { pane_id, events } = request else {
                continue;
            };
            let predicted = predictor.tap(endpoint, generation, pane_id, events, surface, now);
            trace(
                "tap",
                serde_json::json!({ "pane": pane_id, "predicted": predicted }),
            );
            added |= predicted;
        }
        added
    }
}

/// `HERDR_MOSH_TRACE=<dir>`: JSON lines in `<dir>/predict.jsonl`, read once. Write errors are
/// ignored.
fn trace(event: &str, fields: serde_json::Value) {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let dir = std::path::PathBuf::from(std::env::var_os("HERDR_MOSH_TRACE")?);
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("predict.jsonl"))
            .ok()
            .map(Mutex::new)
    });
    let Some(file) = file else { return };
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let mut line = serde_json::json!({ "event": event, "ms": ms });
    if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    let mut file = file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writeln!(file, "{line}");
}

pub(crate) fn observe(
    endpoint_id: &ClientEndpointId,
    generation: u64,
    kind: &str,
    data: &str,
) -> bool {
    if kind != LINK_KIND && kind != predict::ECHO_ACK_KIND {
        return false;
    }
    facade().observe_at(endpoint_id, generation, kind, data, Instant::now())
}

pub(crate) fn paint(
    frame: &mut FrameData,
    active: &ClientEndpointId,
    label: &str,
    surface: Option<&PaneSurfaceFrame>,
    surface_generation: Option<u64>,
    pane_area: Rect,
) {
    facade().paint_at(
        frame,
        active,
        label,
        surface,
        surface_generation,
        pane_area,
        Instant::now(),
    );
}

/// Input tap: counts every `ClientShellPaneInput` request for the active endpoint and creates
/// predictions. Returns the recomposed frame (which `paint` draws onto) when one was added.
pub(crate) fn tap(
    eligible: bool,
    endpoints: &EndpointRegistry,
    requests: &[ClientMessage],
    shell: Option<&mut ClientShellState>,
    size: (u16, u16),
) -> Option<ComposedFrame> {
    if !eligible || PredictMode::get() == PredictMode::Off {
        return None;
    }
    let shell = shell?;
    let endpoint = endpoints.active_id();
    let generation = endpoints.connection(endpoint)?.generation;
    let added = {
        let surface = shell
            .mosh_surface()
            .filter(|(_, surface_generation)| *surface_generation == Some(generation))
            .map(|(surface, _)| surface);
        facade().tap_at(endpoint, generation, requests, surface, Instant::now())
    };
    if added {
        shell.compose(size.0, size.1)
    } else {
        None
    }
}

/// True while any prediction exists for `active` (hidden ones included): the surface-patch
/// fast path must not bypass `compose`, which is where predictions are judged and painted.
pub(crate) fn blocks_fast_path(active: &ClientEndpointId) -> bool {
    if PredictMode::get() == PredictMode::Off {
        return false;
    }
    facade()
        .predictor
        .as_ref()
        .is_some_and(|predictor| predictor.any_prediction(active))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::{decode_endpoint_control, EndpointControlMessage, ProfileId};
    use crate::protocol::{
        CellData, ClientPaneInputEvent, CursorState, PaneSurfacePane, SurfaceGraphicsScene,
        SurfaceRect,
    };

    const LINKED: &str = r#"{"state":"linked"}"#;

    fn resuming(ms: u64) -> String {
        serde_json::to_string(&LinkStatus::Resuming {
            last_contact_ms: ms,
        })
        .unwrap()
    }

    fn state() -> OutageState {
        OutageState {
            resuming: BTreeMap::new(),
        }
    }

    fn facade_with(mode: PredictMode) -> Facade {
        Facade {
            outage: state(),
            predictor: Some(Predictor::new(mode)),
        }
    }

    fn ssh() -> ClientEndpointId {
        ClientEndpointId::Ssh(ProfileId::parse("0123456789abcdef0123456789abcdef").unwrap())
    }

    fn blank_cell() -> CellData {
        CellData {
            symbol: "x".into(),
            fg: 7,
            bg: 9,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn frame(width: u16, height: u16) -> FrameData {
        FrameData {
            cells: vec![blank_cell(); usize::from(width) * usize::from(height)],
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
            graphics: Vec::new(),
        }
    }

    fn row0_text(frame: &FrameData) -> String {
        frame.cells[..usize::from(frame.width)]
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    fn untouched(frame: &FrameData, from_cell: usize) -> bool {
        frame.cells[from_cell..].iter().all(|cell| *cell == blank_cell())
    }

    #[test]
    fn link_status_json_round_trip() {
        let resuming = LinkStatus::Resuming {
            last_contact_ms: 1500,
        };
        let json = serde_json::to_string(&resuming).unwrap();
        assert_eq!(json, r#"{"state":"resuming","last_contact_ms":1500}"#);
        assert_eq!(serde_json::from_str::<LinkStatus>(&json).unwrap(), resuming);
        let linked = LinkStatus::Linked {
            srtt_ms: None,
            echo: false,
        };
        let json = serde_json::to_string(&linked).unwrap();
        assert_eq!(json, LINKED);
        assert_eq!(serde_json::from_str::<LinkStatus>(&json).unwrap(), linked);
    }

    #[test]
    fn linked_with_srtt_round_trips() {
        let linked = LinkStatus::Linked {
            srtt_ms: Some(42),
            echo: false,
        };
        let json = serde_json::to_string(&linked).unwrap();
        assert_eq!(json, r#"{"state":"linked","srtt_ms":42}"#);
        assert_eq!(serde_json::from_str::<LinkStatus>(&json).unwrap(), linked);
    }

    #[test]
    fn old_linked_json_still_parses() {
        assert_eq!(
            serde_json::from_str::<LinkStatus>(r#"{"state":"linked"}"#).unwrap(),
            LinkStatus::Linked {
            srtt_ms: None,
            echo: false,
        }
        );
    }

    #[test]
    fn older_client_ignores_srtt_ms() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        #[serde(tag = "state", rename_all = "snake_case")]
        enum OldLinkStatus {
            #[allow(dead_code)]
            Resuming { last_contact_ms: u64 },
            Linked,
        }
        assert_eq!(
            serde_json::from_str::<OldLinkStatus>(r#"{"state":"linked","srtt_ms":42}"#).unwrap(),
            OldLinkStatus::Linked
        );
    }

    #[test]
    fn linked_with_srtt_clears_the_banner_like_plain_linked() {
        let mut s = state();
        let now = Instant::now();
        assert!(s.observe_at(&ssh(), LINK_KIND, &resuming(1500), now));
        assert!(!s.resuming.is_empty());
        let linked = r#"{"state":"linked","srtt_ms":42}"#;
        assert!(s.observe_at(&ssh(), LINK_KIND, linked, now));
        assert!(s.resuming.is_empty());
    }

    #[test]
    fn observe_ignores_other_kinds() {
        let mut s = state();
        let now = Instant::now();
        assert!(!s.observe_at(&ssh(), "herdr.health.pong.v1", &resuming(1), now));
        assert!(s.resuming.is_empty());
    }

    #[test]
    fn observe_consumes_link_kind_even_when_malformed() {
        let mut s = state();
        let now = Instant::now();
        assert!(s.observe_at(&ssh(), LINK_KIND, &resuming(1), now));
        assert!(s.observe_at(&ssh(), LINK_KIND, "{", now));
        assert_eq!(s.resuming.len(), 1);
        assert_eq!(s.resuming[&ssh()].last_contact_ms, 1);
    }

    #[test]
    fn resuming_then_linked_clears() {
        let mut s = state();
        let now = Instant::now();
        assert!(s.observe_at(&ssh(), LINK_KIND, &resuming(3999), now));
        let mut f = frame(80, 2);
        s.paint_at(&mut f, &ssh(), "box", now);
        assert!(row0_text(&f).starts_with("⚠ box unreachable"));
        assert!(s.observe_at(&ssh(), LINK_KIND, LINKED, now));
        let mut f = frame(80, 2);
        s.paint_at(&mut f, &ssh(), "box", now);
        assert!(untouched(&f, 0));
    }

    #[test]
    fn stale_gap_clears() {
        let mut s = state();
        let t0 = Instant::now();
        s.observe_at(&ssh(), LINK_KIND, &resuming(0), t0);
        let mut f = frame(80, 1);
        s.paint_at(&mut f, &ssh(), "box", t0 + Duration::from_millis(1999));
        assert!(row0_text(&f).starts_with("⚠ box"));
        let mut f = frame(80, 1);
        s.paint_at(&mut f, &ssh(), "box", t0 + Duration::from_secs(2));
        assert!(untouched(&f, 0));
        assert!(s.resuming.is_empty());
    }

    #[test]
    fn paint_only_for_active_endpoint() {
        let mut s = state();
        let now = Instant::now();
        s.observe_at(&ssh(), LINK_KIND, &resuming(0), now);
        let mut f = frame(80, 2);
        s.paint_at(&mut f, &ClientEndpointId::Local, "box", now);
        assert!(untouched(&f, 0));
        assert_eq!(s.resuming.len(), 1);
    }

    #[test]
    fn paint_text_and_truncation() {
        let mut s = state();
        let now = Instant::now();
        s.observe_at(&ssh(), LINK_KIND, &resuming(3999), now);

        let mut f = frame(200, 2);
        s.paint_at(&mut f, &ssh(), "box", now);
        let text = "⚠ box unreachable — last contact 3s ago, resuming…";
        let row = row0_text(&f);
        assert!(row.starts_with(text), "{row:?}");
        assert!(row[text.len()..].chars().all(|c| c == ' '));
        let reset = color_to_u32(Color::Reset);
        for cell in &f.cells[..200] {
            assert_eq!(cell.modifier, modifier_to_u16(Modifier::REVERSED));
            assert_eq!((cell.fg, cell.bg), (reset, reset));
            assert!(!cell.skip && cell.hyperlink.is_none());
        }
        assert!(untouched(&f, 200));

        let mut f = frame(10, 2);
        s.paint_at(&mut f, &ssh(), "box", now);
        assert_eq!(row0_text(&f), "⚠ box unre");
        assert!(f.cells[..10]
            .iter()
            .all(|cell| cell.modifier == modifier_to_u16(Modifier::REVERSED)));
        assert!(untouched(&f, 10));

        let mut f = frame(0, 0);
        s.paint_at(&mut f, &ssh(), "box", now);
        assert!(f.cells.is_empty());
    }

    #[test]
    fn paint_wide_grapheme_does_not_overflow_width() {
        let mut s = state();
        let now = Instant::now();
        s.observe_at(&ssh(), LINK_KIND, &resuming(0), now);
        let mut f = frame(5, 1);
        // "⚠ " is 2 cells; the wide label char takes cells 2-3; a second one would need 4-5.
        s.paint_at(&mut f, &ssh(), "漢漢", now);
        assert_eq!(f.cells.len(), 5);
        assert_eq!(f.cells[2].symbol, "漢");
        assert_eq!(f.cells[3].symbol, "");
        assert_eq!(f.cells[4].symbol, " ");
    }

    #[test]
    fn link_kind_decodes_as_ignored_upstream() {
        assert!(matches!(
            decode_endpoint_control(LINK_KIND, LINKED),
            Ok(EndpointControlMessage::Ignored)
        ));
    }

    const GEN: u64 = 3;

    fn linked_echo(srtt_ms: u64) -> String {
        serde_json::to_string(&LinkStatus::Linked {
            srtt_ms: Some(srtt_ms),
            echo: true,
        })
        .unwrap()
    }

    /// A 10x3 pane surface covering everything, cursor at (0, 1), blank cells.
    fn pane_surface(revision: u64, cells: &[(u16, u16, &str)]) -> PaneSurfaceFrame {
        let mut data = vec![blank_cell(); 30];
        for cell in &mut data {
            cell.symbol = " ".into();
        }
        for (x, y, symbol) in cells {
            data[usize::from(*y) * 10 + usize::from(*x)].symbol = (*symbol).into();
        }
        let area = SurfaceRect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: revision,
            frame: FrameData {
                cells: data,
                width: 10,
                height: 3,
                cursor: Some(CursorState {
                    x: 0,
                    y: 1,
                    visible: true,
                    shape: 0,
                }),
                hyperlinks: Vec::new(),
                graphics: Vec::new(),
            },
            panes: vec![PaneSurfacePane {
                pane_id: "p".into(),
                content_revision: 1,
                rect: area,
                inner_rect: area,
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

    fn typed(text: &str) -> Vec<ClientMessage> {
        vec![ClientMessage::ClientShellPaneInput {
            pane_id: "p".into(),
            events: vec![ClientPaneInputEvent::TextCommit(text.into())],
        }]
    }

    fn any_prediction(facade: &Facade) -> bool {
        facade
            .predictor
            .as_ref()
            .is_some_and(|predictor| predictor.any_prediction(&ssh()))
    }

    fn area(x: u16, y: u16, width: u16, height: u16) -> Rect {
        Rect::new(x, y, width, height)
    }

    #[test]
    fn link_frame_updates_outage_and_predictor() {
        let mut f = facade_with(PredictMode::On);
        let now = Instant::now();
        assert!(f.observe_at(&ssh(), GEN, LINK_KIND, &resuming(1500), now));
        assert!(!f.outage.resuming.is_empty());
        assert!(f.observe_at(&ssh(), GEN, LINK_KIND, &linked_echo(300), now));
        assert!(f.outage.resuming.is_empty());
        // Echo was never advertised before this frame, so only the link frame can enable this.
        let surface = pane_surface(2, &[]);
        assert!(f.tap_at(&ssh(), GEN, &typed("a"), Some(&surface), now));
        assert!(any_prediction(&f));
    }

    #[test]
    fn echo_ack_routed_to_predictor() {
        let mut f = facade_with(PredictMode::On);
        let now = Instant::now();
        f.observe_at(&ssh(), GEN, LINK_KIND, &linked_echo(300), now);
        assert!(f.tap_at(&ssh(), GEN, &typed("a"), Some(&pane_surface(1, &[])), now));
        // A malformed ack is consumed and ignored.
        assert!(f.observe_at(&ssh(), GEN, predict::ECHO_ACK_KIND, "{", now));
        let ack = EchoAck {
            pane_id: "p".into(),
            input_seq: 1,
            surface_revision: 2,
        };
        assert!(f.observe_at(&ssh(), GEN, predict::ECHO_ACK_KIND, &ack.encode(), now));
        // Only a routed ack lets this surface confirm (and so retire) the prediction.
        let confirmed = pane_surface(2, &[(0, 1, "a")]);
        let mut frame = frame(10, 3);
        f.paint_at(&mut frame, &ssh(), "box", Some(&confirmed), Some(GEN), area(0, 0, 10, 3), now);
        assert!(!any_prediction(&f));
    }

    #[test]
    fn unknown_kind_returns_false() {
        let mut f = facade_with(PredictMode::On);
        assert!(!f.observe_at(&ssh(), GEN, "herdr.health.pong.v1", "{}", Instant::now()));
        assert!(!observe(&ssh(), GEN, "herdr.health.pong.v1", "{}"));
    }

    #[test]
    fn linked_echo_round_trips_and_old_json_parses() {
        let linked = LinkStatus::Linked {
            srtt_ms: Some(7),
            echo: true,
        };
        let json = serde_json::to_string(&linked).unwrap();
        assert_eq!(json, r#"{"state":"linked","srtt_ms":7,"echo":true}"#);
        assert_eq!(serde_json::from_str::<LinkStatus>(&json).unwrap(), linked);
        // Absent when false, and absent parses as false.
        let plain = LinkStatus::Linked {
            srtt_ms: None,
            echo: false,
        };
        assert_eq!(serde_json::to_string(&plain).unwrap(), LINKED);
        assert_eq!(serde_json::from_str::<LinkStatus>(LINKED).unwrap(), plain);
    }

    #[test]
    fn paint_draws_banner_then_predictions_and_moves_cursor() {
        let mut f = facade_with(PredictMode::On);
        let now = Instant::now();
        f.observe_at(&ssh(), GEN, LINK_KIND, &linked_echo(300), now);
        f.observe_at(&ssh(), GEN, LINK_KIND, &resuming(2000), now);
        let surface = pane_surface(1, &[]);
        assert!(f.tap_at(&ssh(), GEN, &typed("ab"), Some(&surface), now));

        // The pane sits at (2, 1) inside a 40x5 frame; row 0 carries the banner.
        let mut out = frame(40, 5);
        out.cursor = Some(CursorState {
            x: 2,
            y: 2,
            visible: true,
            shape: 0,
        });
        f.paint_at(&mut out, &ssh(), "box", Some(&surface), Some(GEN), area(2, 1, 10, 3), now);
        assert!(row0_text(&out).starts_with("⚠ box unreachable"));
        let cell = |x: usize, y: usize| &out.cells[y * 40 + x];
        assert_eq!((cell(2, 2).symbol.as_str(), cell(3, 2).symbol.as_str()), ("a", "b"));
        // 300 ms is above the underline threshold; untouched cells keep their colours.
        let underline = modifier_to_u16(Modifier::UNDERLINED);
        assert_eq!(cell(2, 2).modifier & underline, underline);
        assert_eq!((cell(2, 2).fg, cell(2, 2).bg), (7, 9));
        assert_eq!(cell(4, 2), &blank_cell());
        let cursor = out.cursor.unwrap();
        assert_eq!((cursor.x, cursor.y), (4, 2));
    }

    #[test]
    fn typed_key_predicts_before_any_server_response() {
        let mut f = facade_with(PredictMode::On);
        let now = Instant::now();
        f.observe_at(&ssh(), GEN, LINK_KIND, &linked_echo(300), now);
        let surface = pane_surface(1, &[]);
        assert!(f.tap_at(&ssh(), GEN, &typed("x"), Some(&surface), now));
        // The recomposed frame starts from the server's surface; no ack has arrived.
        let mut out = surface.frame.clone();
        f.paint_at(&mut out, &ssh(), "box", Some(&surface), Some(GEN), area(0, 0, 10, 3), now);
        assert_eq!(out.cells[10].symbol, "x");
        assert_eq!(out.cursor.map(|c| (c.x, c.y)), Some((1, 1)));
        assert!(any_prediction(&f));
    }

    #[test]
    fn prediction_off_by_default_paints_nothing_and_never_blocks_fast_path() {
        let mut f = facade_with(PredictMode::Off);
        let now = Instant::now();
        f.observe_at(&ssh(), GEN, LINK_KIND, &linked_echo(300), now);
        let surface = pane_surface(1, &[]);
        assert!(!f.tap_at(&ssh(), GEN, &typed("x"), Some(&surface), now));
        let mut out = surface.frame.clone();
        f.paint_at(&mut out, &ssh(), "box", Some(&surface), Some(GEN), area(0, 0, 10, 3), now);
        assert_eq!(out, surface.frame);
        assert!(!any_prediction(&f));
        // The process-global entry point, with HERDR_MOSH_PREDICT unset.
        assert!(!blocks_fast_path(&ssh()));
    }
}
