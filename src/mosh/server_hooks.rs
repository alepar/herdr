//! Server-side fork hooks. Mounted as a child of the client facade
//! (`crate::client::mosh_outage::server_hooks`) because that is the only fork mount the server
//! can name; every upstream call site is one statement tagged `// mosh-fork hook`.

// The entry points are inert until the 3b resync dispatch and the Phase 4 echo hooks add their
// call sites; remove this allow when the last one lands.
#![allow(dead_code)]

use crate::server::client_transport::ServerEvent;
use crate::server::clients::ClientConnection;

/// Client -> server request for a full `PaneSurface` (3b §5).
pub(crate) const SURFACE_RESYNC_KIND: &str = "x-mosh.surface-resync.v1";

/// Server-side echo bookkeeping and resync marking for mosh clients.
///
/// Owns: per (client connection, pane) received-event counts for subscribed clients
/// (created on the first `x-mosh.echo-subscribe.v1`, idempotent on repeats, removed on
/// client disconnect); the deadline-driven `x-mosh.echo-ack.v1` emission on the control
/// lane; the server's handling of fork client `EndpointControl` kinds
/// (`x-mosh.surface-resync.v1`).
/// Does not own: the client transport, the render path, `Client` state, or any wire
/// format other than the fork's own `x-mosh.*` payloads; it never puts an echo-ack in the
/// latest-wins render slot.
/// New server-side fork behaviour goes behind the entry points in this module; upstream
/// call sites stay one statement tagged `// mosh-fork hook` and never touch this state.
#[derive(Debug, Default)]
pub(crate) struct EchoTracker {}

/// Client `EndpointControl` dispatch for fork kinds (client_transport.rs, the generic
/// `EndpointControl` arm). Fork kinds fall through to the upstream handler afterwards, which
/// answers `None` for them, so unknown `x-mosh.*` kinds stay ignored.
pub(crate) fn on_client_endpoint_control(
    client_id: u64,
    kind: &str,
    data: &str,
    events: &tokio::sync::mpsc::Sender<ServerEvent>,
) {
    let _ = data;
    // Unknown x-mosh.* kinds stay ignored; the echo-subscribe arm lands with Phase 4.
    if kind == SURFACE_RESYNC_KIND {
        let _ = events.blocking_send(ServerEvent::MoshSurfaceResync { client_id });
    }
}

/// Resync (3b §5): the client's next frame is a full `PaneSurface`. Drops the render baseline
/// and any queued render, then requests a repaint. No projection bump, foreground or geometry
/// work; `surface_revision` stays server-owned and is not reset, so the full surface carries a
/// revision above every earlier one. Returns whether a render is needed.
pub(crate) fn surface_resync(client: Option<&mut ClientConnection>) -> bool {
    let Some(client) = client else {
        return false;
    };
    client.render_state.reset_baseline();
    if let Some(writer) = &client.writer {
        writer.discard_pending_render();
    }
    client.request_repaint();
    true
}

/// Pane input received for `pane_id` (anchor: client_transport.rs ~:1196,
/// `ClientShellPaneInput`); `events` is `events.len()`. Inert.
pub(crate) fn on_pane_input(client_id: u64, pane_id: &str, events: usize) {
    let _ = (client_id, pane_id, events);
}

/// A render carrying `surface_revision` was committed for the client. Inert.
pub(crate) fn on_render_sent(client_id: u64, surface_revision: u64) {
    let _ = (client_id, surface_revision);
}

/// The client is gone (anchor: headless.rs `remove_client`, ~:946). Inert.
pub(crate) fn on_client_disconnected(client_id: u64) {
    let _ = client_id;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FrameData, PaneSurfaceFrame, RenderEncoding, ServerMessage};
    use crate::server::client_transport::ClientWriter;

    fn surface(projection_revision: u64) -> PaneSurfaceFrame {
        let buffer = ratatui::buffer::Buffer::with_lines(["pane"]);
        PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision,
            surface_revision: 0,
            frame: FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
            panes: Vec::new(),
            splits: Vec::new(),
            popup: None,
            graphics: crate::protocol::SurfaceGraphicsScene::default(),
        }
    }

    fn client() -> ClientConnection {
        ClientConnection::new(
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            0,
            RenderEncoding::SemanticFrame,
            Some(ClientWriter::test_paused()),
        )
    }

    #[test]
    fn resync_kind_maps_to_server_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        on_client_endpoint_control(7, SURFACE_RESYNC_KIND, "", &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(ServerEvent::MoshSurfaceResync { client_id: 7 })
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_fork_kinds_are_ignored() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        on_client_endpoint_control(7, "x-mosh.bogus.v1", "{}", &tx);
        on_client_endpoint_control(7, "x-mosh.echo-subscribe.v1", "{}", &tx);
        on_client_endpoint_control(7, "health.ping", "", &tx);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resync_body_forces_a_full_surface() {
        let mut client = client();
        let first = client
            .render_state
            .prepare_pane_surface(surface(1))
            .unwrap();
        client.render_state.commit_sent_frame(first);
        assert!(client.render_state.last_pane_surface().is_some());
        let writer = client.writer.clone().unwrap();
        writer.test_fill_render(vec![1, 2, 3]);

        assert!(surface_resync(Some(&mut client)));

        assert!(client.render_state.last_pane_surface().is_none());
        assert!(
            writer.test_drain().is_empty(),
            "a queued render must be discarded"
        );
        // Identical content would normally be skipped; after a resync it is a full PaneSurface.
        let next = client
            .render_state
            .prepare_pane_surface(surface(1))
            .unwrap();
        assert!(matches!(next.message(), ServerMessage::PaneSurface(_)));
    }

    #[test]
    fn resync_for_a_vanished_client_needs_no_render() {
        assert!(!surface_resync(None));
    }

    #[test]
    fn resync_keeps_surface_revisions_monotonic() {
        let mut client = client();
        let mut last = 0;
        for projection in 1..=3 {
            let prepared = client
                .render_state
                .prepare_pane_surface(surface(projection))
                .unwrap();
            let ServerMessage::PaneSurface(frame) = prepared.message() else {
                panic!("expected a full surface");
            };
            assert!(frame.surface_revision > last);
            last = frame.surface_revision;
            client.render_state.commit_sent_frame(prepared);
        }
        assert!(surface_resync(Some(&mut client)));
        let prepared = client
            .render_state
            .prepare_pane_surface(surface(3))
            .unwrap();
        let ServerMessage::PaneSurface(frame) = prepared.message() else {
            panic!("expected a full surface after resync");
        };
        assert!(frame.surface_revision > last, "{} <= {last}", frame.surface_revision);
    }

    #[test]
    fn remaining_entry_points_are_inert() {
        on_pane_input(7, "pane-1", 3);
        on_render_sent(7, 42);
        on_client_disconnected(7);
    }
}
