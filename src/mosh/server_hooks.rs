//! Server-side fork hooks. Mounted as a child of the client facade
//! (`crate::client::mosh_outage::server_hooks`) because that is the only fork mount the server
//! can name; every upstream call site is one statement tagged `// mosh-fork hook`.

// The entry points are inert until the 3b resync dispatch and the Phase 4 echo hooks add their
// call sites; remove this allow when the last one lands.
#![allow(dead_code)]

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

/// Client `EndpointControl` dispatch for fork kinds (anchor: client_transport.rs ~:1349,
/// the unknown-kind arm). `true` means the fork consumed the message. Inert: always `false`.
pub(crate) fn on_client_endpoint_control(client_id: u64, kind: &str, data: &str) -> bool {
    let _ = (client_id, kind, data);
    false
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

    #[test]
    fn entry_points_are_inert() {
        assert!(!on_client_endpoint_control(7, "x-mosh.surface-resync.v1", "{}"));
        assert!(!on_client_endpoint_control(7, "health.ping", ""));
        on_pane_input(7, "pane-1", 3);
        on_render_sent(7, 42);
        on_client_disconnected(7);
        assert!(!on_client_endpoint_control(7, "x-mosh.echo-subscribe.v1", "{}"));
    }
}
