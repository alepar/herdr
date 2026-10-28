//! Outage banner: the `x-mosh.link.v1` payload and per-endpoint banner state; mounted from
//! `client/mod.rs` as `client::mosh_outage`. Does not detect outages: the local bridge does.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ratatui::style::{Color, Modifier};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::client::endpoint::ClientEndpointId;
use crate::protocol::{color_to_u32, modifier_to_u16, FrameData};

pub(crate) const LINK_KIND: &str = "x-mosh.link.v1";
const STALE_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum LinkStatus {
    Resuming { last_contact_ms: u64 },
    Linked,
}

struct Resuming {
    last_contact_ms: u64,
    observed_at: Instant,
}

pub(crate) struct OutageState {
    resuming: BTreeMap<ClientEndpointId, Resuming>,
}

impl OutageState {
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
        match serde_json::from_str::<LinkStatus>(data) {
            Ok(LinkStatus::Resuming { last_contact_ms }) => {
                self.resuming.insert(
                    endpoint.clone(),
                    Resuming {
                        last_contact_ms,
                        observed_at: now,
                    },
                );
            }
            Ok(LinkStatus::Linked) => {
                self.resuming.remove(endpoint);
            }
            Err(_) => {}
        }
        true
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

static STATE: Mutex<OutageState> = Mutex::new(OutageState {
    resuming: BTreeMap::new(),
});

pub(crate) fn observe(endpoint_id: &ClientEndpointId, kind: &str, data: &str) -> bool {
    if kind != LINK_KIND {
        return false;
    }
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .observe_at(endpoint_id, kind, data, Instant::now())
}

pub(crate) fn paint(frame: &mut FrameData, active: &ClientEndpointId, label: &str) {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .paint_at(frame, active, label, Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::{decode_endpoint_control, EndpointControlMessage, ProfileId};
    use crate::protocol::CellData;

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
        let json = serde_json::to_string(&LinkStatus::Linked).unwrap();
        assert_eq!(json, LINKED);
        assert_eq!(
            serde_json::from_str::<LinkStatus>(&json).unwrap(),
            LinkStatus::Linked
        );
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
}
