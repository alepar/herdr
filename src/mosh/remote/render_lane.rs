//! The keeper's render lane: the queue handle and the per-leg stream sender.
//!
//! Owns: the bounded, clearable queue of render frames between the keeper's `server_reader`
//! thread (producer) and a leg's lane task on the runtime (consumer), the offer semantics
//! that implement supersede, the budget that tells the producer to stop reading the server
//! (unsettled bytes against the leg's congestion window), and `RenderLane`, the consumer that
//! sends each frame on its own QUIC uni stream (twice, on a lossy leg), numbers them per leg
//! and resets superseded streams.
//! Hides: the lock and the wake-up of the waiting consumer, quinn stream handling.
//! Must not: classify frames (the caller passes the `FrameClass`), or know about sessions.
//! Lifecycle: the queue is created with a QUIC leg, shared by `KeeperLeg` and its `LegSlot`,
//! closed when the leg is dropped; the consumer drains what is queued and then sees `None`.
//! `RenderLane::spawn` runs one task per leg that ends with the queue or the connection;
//! each frame's body is written by a task of its own that outlives nothing but its stream.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::sync::{oneshot, Notify};

use crate::remote::attach::mosh::link_frames::FrameClass;
use crate::remote::attach::mosh::quic::RenderHeader;

/// Frames a lane holds before chained frames are turned away.
// The producer (the keeper's `server_reader`, mosh-19g.2.3) is the first non-test user.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) const LANE_CAPACITY: usize = 64;

/// The least the budget is, whatever the congestion window: one big patch must fit.
pub(super) const LANE_MIN_BUDGET: u64 = 64 * 1024;
/// The most the budget is, however large the congestion window grows.
pub(super) const LANE_MAX_BUDGET: u64 = 1024 * 1024;
/// Frames outstanding at which the producer pauses: with two copies each, well under the
/// peer's limit of concurrent streams.
pub(super) const LANE_BUDGET_FRAMES: u64 = 24;

/// What `offer` did with a frame.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Offer {
    Accepted,
    Rejected,
}

pub(super) struct State {
    pub(super) frames: VecDeque<(FrameClass, Vec<u8>)>,
    closed: bool,
    /// Queued frames a self-contained offer cleared, over the leg's life.
    cleared: u64,
    /// Chained offers turned away at capacity, over the leg's life.
    rejected: u64,
    /// Bytes of frames offered and not yet settled (queued, or streaming and unacknowledged).
    outstanding: u64,
    /// Frames counted in `outstanding`.
    outstanding_frames: u64,
    /// The leg's congestion window as last read by the lane task (0 until the first read).
    window: u64,
}

/// Where lane trace lines go (`HERDR_MOSH_TRACE`); `None` records nothing.
pub(super) type LaneTrace = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

struct Inner {
    state: Mutex<State>,
    notify: Notify,
    trace: Option<LaneTrace>,
}

/// Producer side: the keeper's `server_reader` thread. Consumer side: the lane's runtime task.
#[derive(Clone)]
pub(super) struct LaneQueue {
    inner: Arc<Inner>,
}

impl LaneQueue {
    #[cfg(test)]
    pub(super) fn new() -> LaneQueue {
        LaneQueue::with_trace(None)
    }

    pub(super) fn with_trace(trace: Option<LaneTrace>) -> LaneQueue {
        LaneQueue {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    frames: VecDeque::new(),
                    closed: false,
                    cleared: 0,
                    rejected: 0,
                    outstanding: 0,
                    outstanding_frames: 0,
                    window: 0,
                }),
                notify: Notify::new(),
                trace,
            }),
        }
    }

    fn trace(&self, event: &str, fields: serde_json::Value) {
        if let Some(trace) = &self.inner.trace {
            trace(event, fields);
        }
    }

    #[cfg(test)]
    pub(super) fn lock_for_test(&self) -> MutexGuard<'_, State> {
        self.lock()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `SelfContained` clears the queue (everything queued is superseded), pushes and is
    /// accepted. `Chained` is rejected when `LANE_CAPACITY` frames are queued, else pushed.
    /// Either is rejected once the lane is closed. `Reliable` frames never travel on a lane.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn offer(&self, class: FrameClass, bytes: Vec<u8>) -> Offer {
        debug_assert_ne!(class, FrameClass::Reliable, "reliable frames are not lane frames");
        {
            let mut state = self.lock();
            if state.closed {
                return Offer::Rejected;
            }
            match class {
                FrameClass::SelfContained => {
                    let queued = state.frames.len() as u64;
                    let bytes: u64 = state.frames.iter().map(|(_, b)| b.len() as u64).sum();
                    state.frames.clear();
                    state.cleared += queued;
                    state.outstanding -= bytes;
                    state.outstanding_frames -= queued;
                }
                FrameClass::Chained | FrameClass::Reliable => {
                    if state.frames.len() >= LANE_CAPACITY {
                        state.rejected += 1;
                        return Offer::Rejected;
                    }
                }
            }
            state.outstanding += bytes.len() as u64;
            state.outstanding_frames += 1;
            state.frames.push_back((class, bytes));
        }
        self.inner.notify.notify_one();
        Offer::Accepted
    }

    /// Enough is outstanding that the producer should stop feeding the lane: the server then
    /// backs up and coalesces its own renders instead of the keeper queueing frames the link
    /// cannot carry (and a newer self-contained frame would only throw away).
    ///
    /// The budget follows the congestion window: unacknowledged data beyond what the window
    /// lets out would only wait in quinn's send buffer, in front of everything newer.
    pub(super) fn saturated(&self) -> bool {
        let state = self.lock();
        let budget = state.window.clamp(LANE_MIN_BUDGET, LANE_MAX_BUDGET);
        state.outstanding >= budget || state.outstanding_frames >= LANE_BUDGET_FRAMES
    }

    fn set_window(&self, window: u64) {
        self.lock().window = window;
    }

    /// A popped frame of `len` bytes is finished with (acknowledged, reset or abandoned).
    pub(super) fn settle(&self, len: usize) {
        let mut state = self.lock();
        state.outstanding = state.outstanding.saturating_sub(len as u64);
        state.outstanding_frames = state.outstanding_frames.saturating_sub(1);
    }

    /// Waits for the next frame; `None` once the lane is closed and drained. One consumer.
    pub(super) async fn pop(&self) -> Option<(FrameClass, Vec<u8>)> {
        loop {
            {
                let mut state = self.lock();
                if let Some(frame) = state.frames.pop_front() {
                    return Some(frame);
                }
                if state.closed {
                    return None;
                }
            }
            // `notify_one` stores a permit when nobody waits, so an offer between the unlock
            // above and this await still wakes it.
            self.inner.notify.notified().await;
        }
    }

    /// The leg ended: wake the consumer and end it once the queue drains.
    pub(super) fn close(&self) {
        self.lock().closed = true;
        self.inner.notify.notify_one();
    }
}

const HEADER_WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(1);
const HEADER_WAIT_POLLS: u32 = 5;

/// Streams still being sent (or awaiting the peer's acknowledgement), by sequence number and
/// copy; the sender cancels one to reset its stream.
type InFlight = Arc<Mutex<BTreeMap<(u64, u8), oneshot::Sender<()>>>>;

/// Keeper-side render sender for one QUIC leg (runs on the QuicAcceptor runtime).
pub(super) struct RenderLane;

impl RenderLane {
    /// Spawns the consumer task: pops `queue`, one uni stream per frame, sequence from 0.
    /// Ends when the queue is closed and drained or the connection is gone.
    pub(super) fn spawn(handle: &Handle, conn: quinn::Connection, queue: LaneQueue) {
        handle.spawn(run(conn, queue));
    }
}

/// Oldest-first: the priority falls as the sequence rises (monotone non-increasing, and
/// strictly decreasing for the first `i32::MAX` frames of a leg), so a large frame is not
/// overtaken by the small chained frames that follow it. A self-contained frame takes the
/// top: everything older is about to be reset, so its data must not queue behind theirs, and
/// its header must reach the peer before the resets do.
fn priority(seq: u64, class: FrameClass) -> i32 {
    match class {
        FrameClass::SelfContained => i32::MAX,
        FrameClass::Chained | FrameClass::Reliable => {
            i32::MAX - i32::try_from(seq).unwrap_or(i32::MAX)
        }
    }
}

/// Chained frames go out twice while at least this share of the leg's packets are being lost
/// (and once again when the share falls to `SINGLE_BELOW_PERCENT`).
///
/// Frames are delivered in order, so the one frame that needs a third or fourth round of
/// retransmission holds up every frame behind it: at 10 % loss, 150 ms each way and 45-packet
/// frames that was one second without a screen change roughly once in 20 s. The two copies
/// ride separate streams and lose packets independently, so the later of the two is far less
/// likely to be late; the bridge keeps whichever completes first.
const DOUBLE_ABOVE_PERCENT: u64 = 3;
const SINGLE_BELOW_PERCENT: u64 = 1;
/// Fewer packets than this in the last second say nothing about the loss rate.
const MIN_LOSS_SAMPLE: u64 = 20;

/// Whether chained frames are sent twice, given the packets sent and lost in the last second.
fn lossy(sent: u64, lost: u64, was_lossy: bool) -> bool {
    if sent < MIN_LOSS_SAMPLE {
        was_lossy
    } else if lost * 100 >= sent * DOUBLE_ABOVE_PERCENT {
        true
    } else if lost * 100 <= sent * SINGLE_BELOW_PERCENT {
        false
    } else {
        was_lossy
    }
}

async fn run(conn: quinn::Connection, queue: LaneQueue) {
    let in_flight: InFlight = Arc::default();
    let mut seq = 0u64;
    let mut tick = tokio::time::interval(TICK);
    let mut ticks = 0u32;
    let mut double = false;
    let mut counted = (0u64, 0u64);
    loop {
        let frame = tokio::select! {
            frame = queue.pop() => frame,
            _ = conn.closed() => None,
            _ = tick.tick() => {
                let stats = conn.stats();
                queue.set_window(stats.path.cwnd);
                ticks += 1;
                if ticks.is_multiple_of(TICKS_PER_SECOND) {
                    let (sent, lost) = (stats.path.sent_packets, stats.path.lost_packets);
                    double = lossy(sent - counted.0, lost - counted.1, double);
                    counted = (sent, lost);
                    if queue.inner.trace.is_some() {
                        trace_stats(&conn, &queue, &in_flight, double);
                    }
                }
                continue;
            }
        };
        let Some((class, bytes)) = frame else { break };
        let settle = Settle(queue.clone(), bytes.len());
        let popped = Instant::now();
        let this = seq;
        seq += 1;
        let header = RenderHeader {
            seq: this,
            class: class.wire_byte(),
        };
        let first = tokio::select! {
            opened = open_stream(&conn, this, class, &header) => opened,
            _ = conn.closed() => None,
        };
        let Some(first) = first else { break };
        if class == FrameClass::SelfContained {
            // The header is on its way; now everything older is superseded.
            let resets = supersede(&in_flight);
            queue.trace(
                "lane_supersede",
                serde_json::json!({"seq": this, "resets": resets}),
            );
        }
        let copies = if double && class == FrameClass::Chained { 2u8 } else { 1 };
        queue.trace(
            "lane_frame",
            serde_json::json!({
                "seq": this,
                "class": format!("{class:?}"),
                "bytes": bytes.len(),
                "open_ms": popped.elapsed().as_millis() as u64,
                "copies": copies,
            }),
        );
        let mut settle = Some(settle);
        let mut first = Some(first);
        for copy in 0..copies {
            let mut send = match first.take() {
                Some(stream) => stream,
                None => {
                    let second = tokio::select! {
                        opened = open_stream(&conn, this, class, &header) => opened,
                        _ = conn.closed() => None,
                    };
                    match second {
                        Some(stream) => stream,
                        None => break,
                    }
                }
            };
            let (cancel_tx, cancel_rx) = oneshot::channel();
            in_flight
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((this, copy), cancel_tx);
            let in_flight = Arc::clone(&in_flight);
            let queue = queue.clone();
            let bytes = bytes.clone();
            // The frame is settled when its first copy is: the second one adds no latency.
            let settle = settle.take();
            tokio::spawn(async move {
                let _settle = settle;
                let len = bytes.len();
                let outcome = tokio::select! {
                    acked = send_body(&mut send, bytes) => if acked { "acked" } else { "error" },
                    _ = cancel_rx => {
                        let _ = send.reset(0u32.into());
                        "reset"
                    }
                };
                in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(this, copy));
                queue.trace(
                    "lane_stream",
                    serde_json::json!({
                        "seq": this,
                        "copy": copy,
                        "outcome": outcome,
                        "bytes": len,
                        "ms_open": popped.elapsed().as_millis() as u64,
                    }),
                );
            });
        }
    }
}

/// Opens a uni stream for frame `seq` and writes its header. Before any data is written: a
/// priority change does not move data already queued. A self-contained frame's header is
/// then waited onto the wire, ahead of the resets it precedes.
async fn open_stream(
    conn: &quinn::Connection,
    seq: u64,
    class: FrameClass,
    header: &RenderHeader,
) -> Option<quinn::SendStream> {
    let mut send = conn.open_uni().await.ok()?;
    let _ = send.set_priority(priority(seq, class));
    let sent_before = conn.stats().udp_tx.datagrams;
    send.write_all(&header.encode()).await.ok()?;
    if class == FrameClass::SelfContained {
        header_on_the_wire(conn, sent_before).await;
    }
    Some(send)
}

/// Cancels every stream in flight; returns how many.
fn supersede(in_flight: &InFlight) -> usize {
    let streams = std::mem::take(&mut *in_flight.lock().unwrap_or_else(|e| e.into_inner()));
    let resets = streams.len();
    for (_, cancel) in streams {
        let _ = cancel.send(());
    }
    resets
}

/// Settles a frame's bytes in the queue's budget when its stream task (or an early exit) ends.
struct Settle(LaneQueue, usize);

impl Drop for Settle {
    fn drop(&mut self) {
        self.0.settle(self.1);
    }
}

/// How often the lane reads the congestion window for its budget and the loss rate.
const TICK: Duration = Duration::from_millis(100);
const TICKS_PER_SECOND: u32 = 10;

fn trace_stats(conn: &quinn::Connection, queue: &LaneQueue, in_flight: &InFlight, double: bool) {
    let stats = conn.stats();
    let (queued, cleared, rejected, outstanding) = {
        let state = queue.lock();
        (state.frames.len(), state.cleared, state.rejected, state.outstanding)
    };
    let streams = in_flight.lock().unwrap_or_else(|e| e.into_inner()).len();
    queue.trace(
        "lane_stats",
        serde_json::json!({
            "cwnd": stats.path.cwnd,
            "rtt_ms": stats.path.rtt.as_millis() as u64,
            "sent_packets": stats.path.sent_packets,
            "lost_packets": stats.path.lost_packets,
            "congestion_events": stats.path.congestion_events,
            "udp_tx": stats.udp_tx.datagrams,
            "udp_rx": stats.udp_rx.datagrams,
            "queued": queued,
            "cleared": cleared,
            "rejected": rejected,
            "streams": streams,
            "outstanding": outstanding,
            "double": double,
        }),
    );
}

/// Waits (bounded) until the connection has sent a datagram since `sent_before`, so the
/// header just written is on the wire before the resets it precedes are queued. QUIC gives no
/// stronger hook than this: a congested link times the wait out and the resets go anyway,
/// because stopping the superseded data matters more than the order of the two signals.
async fn header_on_the_wire(conn: &quinn::Connection, sent_before: u64) {
    for _ in 0..HEADER_WAIT_POLLS {
        if conn.stats().udp_tx.datagrams > sent_before {
            return;
        }
        tokio::time::sleep(HEADER_WAIT_POLL).await;
    }
}

/// Writes the body, finishes the stream, then waits for the peer to acknowledge it all, so
/// the stream stays resettable while any of it is unacknowledged on a lossy link.
async fn send_body(send: &mut quinn::SendStream, bytes: Vec<u8>) -> bool {
    if send.write_all(&bytes).await.is_err() || send.finish().is_err() {
        return false;
    }
    send.stopped().await.is_ok()
}

/// A loopback QUIC connection pair for lane tests: the server side with a lane spawned on
/// it, and the client side that accepts the lane's uni streams.
#[cfg(test)]
pub(super) mod loopback {
    use std::io;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::remote::attach::mosh::quic::{
        self, QuicRuntime, ServerIdentity, KEEPER_IDLE_TIMEOUT, SERVER_NAME,
    };

    pub(in crate::remote::attach::mosh::remote) struct Pair {
        pub rt: Arc<QuicRuntime>,
        pub server: quinn::Connection,
        pub client: quinn::Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
    }

    pub(in crate::remote::attach::mosh::remote) fn pair() -> Pair {
        let rt = QuicRuntime::new("lane-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server_endpoint = quic::server_endpoint(
            &rt,
            quic::server_config(&identity, KEEPER_IDLE_TIMEOUT).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        let client_endpoint = quic::client_endpoint(&rt, false).expect("client endpoint");
        let config =
            quic::client_config(identity.fingerprint, Duration::from_secs(30)).expect("config");
        let accepting = server_endpoint.clone();
        let connecting = client_endpoint.clone();
        let (server, client) = rt
            .block_on(async move {
                let accept = async move {
                    let incoming = accepting
                        .accept()
                        .await
                        .ok_or_else(|| io::Error::other("no incoming"))?;
                    incoming.await.map_err(io::Error::other)
                };
                let connect = async move {
                    connecting
                        .connect_with(config, addr, SERVER_NAME)
                        .map_err(io::Error::other)?
                        .await
                        .map_err(io::Error::other)
                };
                let (server, client) = tokio::join!(accept, connect);
                io::Result::Ok((server?, client?))
            })
            .expect("block_on")
            .expect("connect");
        Pair {
            rt,
            server,
            client,
            _endpoints: (server_endpoint, client_endpoint),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    fn fill_chained(queue: &LaneQueue) {
        for n in 0..LANE_CAPACITY {
            assert_eq!(queue.offer(FrameClass::Chained, vec![n as u8]), Offer::Accepted);
        }
    }

    #[test]
    fn capacity_is_64() {
        assert_eq!(LANE_CAPACITY, 64);
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.lock().frames.len(), 64);
    }

    #[test]
    fn chained_rejected_when_64_queued() {
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.offer(FrameClass::Chained, vec![0xff]), Offer::Rejected);
        assert_eq!(queue.lock().frames.len(), 64);
        // Rejected means not queued: the 64th frame is still the last one.
        assert_eq!(queue.lock().frames.back().unwrap().1, vec![63]);
    }

    #[test]
    fn self_contained_clears_and_is_accepted_when_full() {
        let queue = LaneQueue::new();
        fill_chained(&queue);
        assert_eq!(queue.offer(FrameClass::SelfContained, vec![0xaa]), Offer::Accepted);
        let state = queue.lock();
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.frames[0], (FrameClass::SelfContained, vec![0xaa]));
        drop(state);
        // Room again for chained frames.
        assert_eq!(queue.offer(FrameClass::Chained, vec![1]), Offer::Accepted);
    }

    #[test]
    fn pop_returns_frames_in_order() {
        let queue = LaneQueue::new();
        queue.offer(FrameClass::SelfContained, vec![1]);
        queue.offer(FrameClass::Chained, vec![2]);
        let rt = runtime();
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::SelfContained, vec![1])));
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::Chained, vec![2])));
    }

    #[test]
    fn pop_wakes_on_offer() {
        let queue = LaneQueue::new();
        let producer = queue.clone();
        let start = Instant::now();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            producer.offer(FrameClass::Chained, vec![7]);
        });
        let frame = runtime().block_on(queue.pop());
        assert_eq!(frame, Some((FrameClass::Chained, vec![7])));
        assert!(start.elapsed() >= Duration::from_millis(50), "pop returned before the offer");
        handle.join().unwrap();
    }

    #[test]
    fn close_ends_pop() {
        let queue = LaneQueue::new();
        let closer = queue.clone();
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            closer.close();
        });
        assert_eq!(runtime().block_on(queue.pop()), None);
        handle.join().unwrap();
    }

    #[test]
    fn close_drains_queued_frames_then_ends() {
        let queue = LaneQueue::new();
        queue.offer(FrameClass::Chained, vec![1]);
        queue.close();
        let rt = runtime();
        assert_eq!(rt.block_on(queue.pop()), Some((FrameClass::Chained, vec![1])));
        assert_eq!(rt.block_on(queue.pop()), None);
    }

    #[test]
    fn closed_lane_rejects_offers() {
        let queue = LaneQueue::new();
        queue.close();
        assert_eq!(queue.offer(FrameClass::SelfContained, vec![1]), Offer::Rejected);
        assert_eq!(queue.offer(FrameClass::Chained, vec![1]), Offer::Rejected);
    }

    #[test]
    fn frame_budget_saturates_at_24_frames() {
        let queue = LaneQueue::new();
        for _ in 0..LANE_BUDGET_FRAMES - 1 {
            queue.offer(FrameClass::Chained, vec![1]);
        }
        assert!(!queue.saturated());
        queue.offer(FrameClass::Chained, vec![1]);
        assert!(queue.saturated());
    }

    #[test]
    fn byte_budget_is_the_window_between_its_bounds() {
        let queue = LaneQueue::new();
        // No window read yet: the floor.
        queue.offer(FrameClass::Chained, vec![0; LANE_MIN_BUDGET as usize - 1]);
        assert!(!queue.saturated());
        queue.offer(FrameClass::Chained, vec![0]);
        assert!(queue.saturated(), "{LANE_MIN_BUDGET} bytes fill the floor");
        // A window of 300 kB lets more in ...
        queue.set_window(300_000);
        assert!(!queue.saturated());
        queue.offer(FrameClass::Chained, vec![0; 235_000]);
        assert!(queue.saturated());
        // ... and a window past the ceiling does not.
        queue.set_window(u64::MAX);
        queue.offer(FrameClass::Chained, vec![0; LANE_MAX_BUDGET as usize]);
        assert!(queue.saturated());
    }

    #[test]
    fn settling_frees_the_budget_and_a_self_contained_offer_resets_it() {
        let queue = LaneQueue::new();
        queue.offer(FrameClass::Chained, vec![0; LANE_MIN_BUDGET as usize]);
        assert!(queue.saturated());
        let rt = runtime();
        let (_, frame) = rt.block_on(queue.pop()).unwrap();
        // Popped is not settled: the bytes are on their way and unacknowledged.
        assert!(queue.saturated());
        queue.settle(frame.len());
        assert!(!queue.saturated());
        // Queued frames a self-contained one clears leave the count with it.
        for _ in 0..LANE_BUDGET_FRAMES {
            queue.offer(FrameClass::Chained, vec![0]);
        }
        assert!(queue.saturated());
        queue.offer(FrameClass::SelfContained, vec![0]);
        assert!(!queue.saturated());
        assert_eq!(queue.lock().outstanding_frames, 1);
        assert_eq!(queue.lock().outstanding, 1);
    }

    #[test]
    fn loss_decides_when_frames_go_twice() {
        // 10 % lost: double. Under 1 %: single. In between: whatever it was.
        assert!(lossy(100, 10, false));
        assert!(!lossy(100, 0, true));
        assert!(!lossy(1000, 10, true));
        assert!(lossy(100, 2, true));
        assert!(!lossy(100, 2, false));
        assert!(lossy(100, 3, false), "3 % is the threshold");
        // Too few packets to tell: keep the previous answer.
        assert!(lossy(MIN_LOSS_SAMPLE - 1, 19, true));
        assert!(!lossy(MIN_LOSS_SAMPLE - 1, 19, false));
    }

    mod lane {
        use super::super::loopback;
        use super::*;
        use std::time::Duration;

        const CHAINED_BODY: usize = 16 << 20;
        /// Under the stream window and the loopback socket buffer, so only priority decides the order.
        const LARGE_FRAME: usize = 100_000;

        async fn next_stream(conn: &quinn::Connection) -> (quinn::RecvStream, RenderHeader) {
            let mut recv = conn.accept_uni().await.expect("accept_uni");
            let mut header = [0u8; RenderHeader::LEN];
            recv.read_exact(&mut header).await.expect("header");
            (recv, RenderHeader::decode(&header).expect("decode"))
        }

        fn started(pair: &loopback::Pair) -> LaneQueue {
            let queue = LaneQueue::new();
            RenderLane::spawn(&pair.rt.handle(), pair.server.clone(), queue.clone());
            queue
        }

        #[test]
        fn priority_falls_as_the_sequence_rises() {
            let chained = FrameClass::Chained;
            assert_eq!(priority(0, chained), i32::MAX);
            assert!(priority(1, chained) < priority(0, chained));
            assert!(priority(1000, chained) < priority(999, chained));
            assert_eq!(priority(u64::MAX, chained), 0);
            // A self-contained frame outranks every older chained frame.
            assert!(priority(5, FrameClass::SelfContained) > priority(4, chained));
        }

        #[test]
        fn frames_arrive_on_uni_streams_with_headers() {
            let pair = loopback::pair();
            let queue = started(&pair);
            let frames = [
                (FrameClass::SelfContained, vec![1u8; 10]),
                (FrameClass::Chained, vec![2u8; 3000]),
                (FrameClass::Chained, vec![3u8; 1]),
                (FrameClass::Chained, vec![4u8; 70_000]),
            ];
            let client = pair.client.clone();
            let got = pair
                .rt
                .block_on(async move {
                    let mut got = Vec::new();
                    for (class, bytes) in &frames {
                        assert_eq!(queue.offer(*class, bytes.clone()), Offer::Accepted);
                        let (mut recv, header) = next_stream(&client).await;
                        let body = recv.read_to_end(1 << 20).await.expect("body");
                        got.push((header, body));
                    }
                    (frames, got)
                })
                .expect("block_on");
            let (frames, got) = got;
            for (n, ((class, bytes), (header, body))) in frames.iter().zip(&got).enumerate() {
                assert_eq!(header.seq, n as u64);
                assert_eq!(header.class, class.wire_byte());
                assert_eq!(body, bytes);
            }
        }

        #[test]
        fn self_contained_resets_older_open_streams_after_its_header() {
            let pair = loopback::pair();
            let queue = started(&pair);
            let client = pair.client.clone();
            let events = pair
                .rt
                .block_on(async move {
                    let log = Arc::new(Mutex::new(Vec::<String>::new()));
                    assert_eq!(
                        queue.offer(FrameClass::SelfContained, vec![0u8; CHAINED_BODY]),
                        Offer::Accepted
                    );
                    // Seq 0 is self-contained; 1 and 2 chain on it. The client reads headers only,
                    // so flow control keeps all three bodies in flight.
                    for _ in 0..2 {
                        assert_eq!(
                            queue.offer(FrameClass::Chained, vec![0u8; CHAINED_BODY]),
                            Offer::Accepted
                        );
                    }
                    let mut watchers = Vec::new();
                    for _ in 0..3 {
                        let (mut recv, header) = next_stream(&client).await;
                        let log = Arc::clone(&log);
                        watchers.push(tokio::spawn(async move {
                            let reset = recv.received_reset().await;
                            if matches!(reset, Ok(Some(_))) {
                                log.lock().unwrap().push(format!("reset {}", header.seq));
                            }
                        }));
                    }
                    // Let the older streams run into flow control and the link go quiet, so the
                    // client observes the header and the resets in the order they arrive.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    assert_eq!(
                        queue.offer(FrameClass::SelfContained, vec![9u8; 100]),
                        Offer::Accepted
                    );
                    let (mut recv, header) = next_stream(&client).await;
                    log.lock().unwrap().push(format!("header {}", header.seq));
                    assert_eq!(header.class, FrameClass::SelfContained.wire_byte());
                    assert_eq!(recv.read_to_end(1000).await.expect("body"), vec![9u8; 100]);
                    for watcher in watchers {
                        tokio::time::timeout(Duration::from_secs(10), watcher)
                            .await
                            .expect("an older stream was never reset")
                            .expect("watcher");
                    }
                    let events = log.lock().unwrap().clone();
                    events
                })
                .expect("block_on");
            assert_eq!(events[0], "header 3", "{events:?}");
            let mut resets = events[1..].to_vec();
            resets.sort();
            assert_eq!(resets, ["reset 0", "reset 1", "reset 2"], "{events:?}");
        }

        #[test]
        fn large_self_contained_completes_before_later_small_chained() {
            let pair = loopback::pair();
            let queue = started(&pair);
            let client = pair.client.clone();
            let order = pair
                .rt
                .block_on(async move {
                    let big = LARGE_FRAME;
                    assert_eq!(
                        queue.offer(FrameClass::SelfContained, vec![5u8; big]),
                        Offer::Accepted
                    );
                    for n in 0..3u8 {
                        assert_eq!(queue.offer(FrameClass::Chained, vec![n; 2000]), Offer::Accepted);
                    }
                    let done = Arc::new(Mutex::new(Vec::<u64>::new()));
                    let mut readers = Vec::new();
                    for _ in 0..4 {
                        let (mut recv, header) = next_stream(&client).await;
                        let done = Arc::clone(&done);
                        readers.push(tokio::spawn(async move {
                            let body = recv.read_to_end(2 << 20).await.expect("body");
                            done.lock().unwrap().push(header.seq);
                            body.len()
                        }));
                    }
                    let mut lens = Vec::new();
                    for reader in readers {
                        lens.push(reader.await.expect("reader"));
                    }
                    assert_eq!(lens, [big, 2000, 2000, 2000]);
                    let order = done.lock().unwrap().clone();
                    order
                })
                .expect("block_on");
            assert_eq!(order[0], 0, "completion order {order:?}");
        }

        #[test]
        fn lane_ends_when_the_queue_closes() {
            let pair = loopback::pair();
            let queue = started(&pair);
            queue.offer(FrameClass::SelfContained, vec![1]);
            queue.close();
            let client = pair.client.clone();
            pair.rt
                .block_on(async move {
                    let (mut recv, header) = next_stream(&client).await;
                    assert_eq!(header.seq, 0);
                    assert_eq!(recv.read_to_end(10).await.expect("body"), vec![1]);
                })
                .expect("block_on");
        }
    }
}
