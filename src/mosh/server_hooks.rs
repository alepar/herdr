//! Server-side fork hooks. Mounted as a child of the client facade
//! (`crate::client::mosh_outage::server_hooks`) because that is the only fork mount the server
//! can name; every upstream call site is one statement tagged `// mosh-fork hook`.

// The entry points are inert until the 3b resync dispatch and the Phase 4 echo hooks add their
// call sites; remove this allow when the last one lands.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::predict::{EchoAck, ECHO_ACK_KIND, ECHO_SUBSCRIBE_KIND};
use crate::protocol::ServerMessage;
use crate::server::client_transport::{ClientWriter, ServerEvent};
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
pub(crate) struct EchoTracker {
    clients: HashMap<u64, ClientEcho>,
}

/// An input is acked this long after it was received (3.3 / spec §2).
const ECHO_DELAY: Duration = Duration::from_millis(50);
/// Extra wait when no render has been seen to commit since the input was first observed, so an
/// echo render that the loop has not produced yet (pane dirty, throttled) is not outrun by its
/// ack. Input that causes no render (`stty -echo`) is acked once this elapses.
const SETTLE_GRACE: Duration = Duration::from_millis(50);
/// Re-check interval while an ack is due but withheld.
const RETRY: Duration = Duration::from_millis(10);

static ECHO: Mutex<Option<EchoTracker>> = Mutex::new(None);

fn with_tracker<R>(f: impl FnOnce(&mut EchoTracker) -> R) -> R {
    let mut guard = ECHO.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(EchoTracker::default))
}

#[derive(Debug, Default)]
struct ClientEcho {
    panes: HashMap<String, PaneEcho>,
}

#[derive(Debug, Default)]
struct PaneEcho {
    received: u64,
    acked: u64,
    fifo: VecDeque<Received>,
}

#[derive(Debug)]
struct Received {
    /// Cumulative events received for the pane once this batch arrived.
    count: u64,
    at: Instant,
    /// Committed `surface_revision` when an echo pass first saw this entry.
    revision_seen: Option<u64>,
}

impl EchoTracker {
    fn subscribe(&mut self, client_id: u64) {
        self.clients.entry(client_id).or_default();
    }

    fn count(&mut self, client_id: u64, pane_id: &str, events: usize, now: Instant) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };
        if events == 0 {
            return;
        }
        let pane = client.panes.entry(pane_id.to_owned()).or_default();
        pane.received = pane.received.saturating_add(events as u64);
        pane.fifo.push_back(Received {
            count: pane.received,
            at: now,
            revision_seen: None,
        });
    }

    /// Acks due at `now`, with the writer to send each on. Takes no locks of its own and
    /// sends nothing: the caller sends after releasing the tracker mutex.
    fn collect_due(
        &mut self,
        clients: &HashMap<u64, ClientConnection>,
        now: Instant,
    ) -> Vec<(ClientWriter, EchoAck)> {
        let mut due = Vec::new();
        for (client_id, echo) in &mut self.clients {
            let Some(client) = clients.get(client_id) else {
                continue;
            };
            let Some(writer) = &client.writer else {
                continue;
            };
            let Some(revision) = client
                .render_state
                .last_pane_surface()
                .map(|surface| surface.surface_revision)
            else {
                continue;
            };
            for pane in echo.panes.values_mut() {
                for entry in &mut pane.fifo {
                    entry.revision_seen.get_or_insert(revision);
                }
            }
            // A deferred or recomputing render means a newer surface is on its way.
            if client.render_pending || client.render_state.requires_recompute() {
                continue;
            }
            for (pane_id, pane) in &mut echo.panes {
                let newest_due = pane
                    .fifo
                    .iter()
                    .rev()
                    .find(|entry| entry.at + ECHO_DELAY <= now);
                let Some(entry) = newest_due else {
                    continue;
                };
                let rendered_since = entry.revision_seen != Some(revision);
                if !rendered_since && now < entry.at + ECHO_DELAY + SETTLE_GRACE {
                    continue;
                }
                let input_seq = entry.count;
                pane.acked = input_seq;
                pane.fifo.retain(|entry| entry.count > input_seq);
                due.push((
                    writer.clone(),
                    EchoAck {
                        pane_id: pane_id.clone(),
                        input_seq,
                        surface_revision: revision,
                    },
                ));
            }
        }
        due
    }

    fn deadline(&self, next: Option<Instant>, now: Instant) -> Option<Instant> {
        let mut earliest = next;
        for echo in self.clients.values() {
            for pane in echo.panes.values() {
                let first_pending = pane.fifo.iter().find(|entry| entry.at + ECHO_DELAY > now);
                let newest_due = pane
                    .fifo
                    .iter()
                    .rev()
                    .find(|entry| entry.at + ECHO_DELAY <= now);
                let candidates = first_pending
                    .map(|entry| entry.at + ECHO_DELAY)
                    .into_iter()
                    .chain(newest_due.map(|entry| entry.at + ECHO_DELAY + SETTLE_GRACE));
                for candidate in candidates {
                    // A past candidate means the ack is withheld; poll instead of spinning.
                    let candidate = if candidate <= now { now + RETRY } else { candidate };
                    earliest = Some(earliest.map_or(candidate, |current| current.min(candidate)));
                }
            }
        }
        earliest
    }
}

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
    // Unknown x-mosh.* kinds stay ignored.
    if kind == SURFACE_RESYNC_KIND {
        let _ = events.blocking_send(ServerEvent::MoshSurfaceResync { client_id });
    } else if kind == ECHO_SUBSCRIBE_KIND {
        with_tracker(|tracker| tracker.subscribe(client_id));
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
/// `ClientShellPaneInput`); `events` is `events.len()`. Counted only for subscribed clients.
pub(crate) fn on_pane_input(client_id: u64, pane_id: &str, events: usize) {
    let now = Instant::now();
    with_tracker(|tracker| tracker.count(client_id, pane_id, events, now));
}

/// Queues the echo-acks due at `now` on each client's control lane (anchor: headless.rs, before
/// step 8). The ack carries the client's last committed `surface_revision`; it waits while a
/// render is deferred or recomputing, and for a settle grace when no render has committed since
/// the input was first observed.
pub(crate) fn echo_due(clients: &HashMap<u64, ClientConnection>, now: Instant) {
    let due = with_tracker(|tracker| tracker.collect_due(clients, now));
    send_acks(due);
}

fn send_acks(due: Vec<(ClientWriter, EchoAck)>) {
    for (writer, ack) in due {
        let message = ServerMessage::EndpointControl {
            kind: ECHO_ACK_KIND.into(),
            data: ack.encode(),
        };
        let mut framed = Vec::new();
        if crate::protocol::write_message(&mut framed, &message).is_ok() {
            let _ = writer.control.send(framed);
        }
    }
}

/// Folds the next echo-ack time into the loop's `next_deadline` (anchor: headless.rs, step 8).
pub(crate) fn echo_deadline(next_deadline: Option<Instant>) -> Option<Instant> {
    let now = Instant::now();
    with_tracker(|tracker| tracker.deadline(next_deadline, now))
}

/// A render carrying `surface_revision` was committed for the client. Inert: echo acks read the
/// committed revision from the client's render state instead.
pub(crate) fn on_render_sent(client_id: u64, surface_revision: u64) {
    let _ = (client_id, surface_revision);
}

/// The client is gone (anchor: headless.rs `remove_client`, ~:946). Drops its echo entry.
pub(crate) fn on_client_disconnected(client_id: u64) {
    with_tracker(|tracker| {
        tracker.clients.remove(&client_id);
    });
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

    fn commit(client: &mut ClientConnection, projection: u64) -> u64 {
        let prepared = client
            .render_state
            .prepare_pane_surface(surface(projection))
            .unwrap();
        let ServerMessage::PaneSurface(frame) = prepared.message() else {
            panic!("expected a full surface");
        };
        let revision = frame.surface_revision;
        client.render_state.commit_sent_frame(prepared);
        revision
    }

    fn acks(client: &ClientConnection) -> Vec<EchoAck> {
        let writer = client.writer.as_ref().unwrap();
        writer
            .test_drain()
            .into_iter()
            .map(|frame| {
                let message: ServerMessage =
                    crate::protocol::read_message(&mut frame.as_slice(), crate::protocol::MAX_FRAME_SIZE)
                        .unwrap();
                let ServerMessage::EndpointControl { kind, data } = message else {
                    panic!("expected an endpoint control frame");
                };
                assert_eq!(kind, ECHO_ACK_KIND);
                EchoAck::decode(&data).unwrap()
            })
            .collect()
    }

    /// Runs a due pass on `tracker` for the one client `id` and sends what it collected.
    fn pass(
        tracker: &mut EchoTracker,
        id: u64,
        client: ClientConnection,
        now: Instant,
    ) -> ClientConnection {
        let mut map = HashMap::from([(id, client)]);
        send_acks(tracker.collect_due(&map, now));
        map.remove(&id).unwrap()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn unsubscribed_clients_are_not_counted() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.count(1, "p", 3, t0);
        assert!(tracker.clients.is_empty());
        let mut c = client();
        commit(&mut c, 1);
        let c = pass(&mut tracker, 1, c, t0 + ms(500));
        assert!(acks(&c).is_empty());
        assert_eq!(tracker.deadline(None, t0), None);
    }

    #[test]
    fn ack_only_after_50ms() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        let mut c = client();
        commit(&mut c, 1);
        tracker.count(1, "p", 2, t0);
        let c = pass(&mut tracker, 1, c, t0 + ms(49));
        assert!(acks(&c).is_empty());
        assert_eq!(tracker.deadline(None, t0 + ms(10)), Some(t0 + ms(50)));
        // A render commits after the first look, so the ack is not held for the settle grace.
        let mut c = c;
        commit(&mut c, 2);
        let c = pass(&mut tracker, 1, c, t0 + ms(50));
        let got = acks(&c);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].pane_id.as_str(), got[0].input_seq), ("p", 2));
    }

    #[test]
    fn ack_carries_last_committed_revision() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        let mut c = client();
        commit(&mut c, 1);
        tracker.count(1, "p", 1, t0);
        let mut c = pass(&mut tracker, 1, c, t0); // first look stamps revision_seen
        assert!(acks(&c).is_empty());
        let r1 = commit(&mut c, 2);
        let r2 = commit(&mut c, 3);
        assert!(r2 > r1);
        let c = pass(&mut tracker, 1, c, t0 + ms(60));
        let got = acks(&c);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].surface_revision, r2);
    }

    #[test]
    fn no_baseline_no_ack_until_one_exists() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        tracker.count(1, "p", 1, t0);
        let c = pass(&mut tracker, 1, client(), t0 + ms(500));
        assert!(acks(&c).is_empty());
        let mut c = c;
        let r = commit(&mut c, 1);
        let c = pass(&mut tracker, 1, c, t0 + ms(501));
        let got = acks(&c);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].surface_revision, r);
    }

    #[test]
    fn input_without_render_is_still_acked() {
        // stty -echo: no render follows the input, so the revision never moves.
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        let mut c = client();
        let r = commit(&mut c, 1);
        tracker.count(1, "p", 1, t0);
        let c = pass(&mut tracker, 1, c, t0 + ms(60));
        assert!(acks(&c).is_empty(), "held for the settle grace");
        assert_eq!(tracker.deadline(None, t0 + ms(60)), Some(t0 + ms(100)));
        let c = pass(&mut tracker, 1, c, t0 + ms(100));
        let got = acks(&c);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].input_seq, got[0].surface_revision), (1, r));
        // Nothing left to ack: the entry is consumed.
        assert_eq!(tracker.deadline(None, t0 + ms(100)), None);
    }

    #[test]
    fn remove_client_drops_entry() {
        let id = 9_001;
        on_client_endpoint_control(id, ECHO_SUBSCRIBE_KIND, "", &tokio::sync::mpsc::channel(1).0);
        assert!(with_tracker(|t| t.clients.contains_key(&id)));
        on_client_disconnected(id);
        assert!(!with_tracker(|t| t.clients.contains_key(&id)));
        on_pane_input(id, "p", 1);
        assert!(!with_tracker(|t| t.clients.contains_key(&id)));
    }

    #[test]
    fn second_subscribe_keeps_counts() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        tracker.count(1, "p", 4, t0);
        tracker.subscribe(1);
        let pane = &tracker.clients[&1].panes["p"];
        assert_eq!((pane.received, pane.acked, pane.fifo.len()), (4, 0, 1));
    }

    #[test]
    fn render_pending_at_deadline_withholds_then_acks_new_revision() {
        let mut tracker = EchoTracker::default();
        let t0 = Instant::now();
        tracker.subscribe(1);
        let mut c = client();
        let old = commit(&mut c, 1);
        tracker.count(1, "p", 1, t0);
        c.render_pending = true;
        let mut c = pass(&mut tracker, 1, c, t0 + ms(80)); // stamps `old`, then withholds
        assert!(acks(&c).is_empty());
        let new = commit(&mut c, 2);
        assert!(new > old);
        c.render_pending = false;
        let c = pass(&mut tracker, 1, c, t0 + ms(85));
        let got = acks(&c);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].surface_revision, new);
    }

    #[test]
    fn subscribed_client_typing_gets_acks_with_input_seq_equal_to_events_sent() {
        let id = 9_002;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        on_client_endpoint_control(id, ECHO_SUBSCRIBE_KIND, "", &tx);
        let mut c = client();
        commit(&mut c, 1);
        let mut clients = HashMap::from([(id, c)]);
        let t0 = Instant::now();
        // First look stamps the baseline revision, then the echo render commits.
        echo_due(&clients, t0);
        for batch in [3usize, 1, 2] {
            on_pane_input(id, "pane-1", batch);
        }
        echo_due(&clients, t0);
        commit(clients.get_mut(&id).unwrap(), 2);
        echo_due(&clients, t0 + Duration::from_secs(1));
        let got = acks(&clients[&id]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].pane_id.as_str(), got[0].input_seq), ("pane-1", 6));
        on_client_disconnected(id);
    }

    #[test]
    fn unsubscribed_client_receives_no_acks() {
        let id = 9_003;
        let mut c = client();
        commit(&mut c, 1);
        let clients = HashMap::from([(id, c)]);
        on_pane_input(id, "pane-1", 5);
        echo_due(&clients, Instant::now() + Duration::from_secs(1));
        assert!(acks(&clients[&id]).is_empty());
    }
}
