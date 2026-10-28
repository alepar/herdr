//! The user-facing socket (accept loop), the client/session pump, whole-frame hold-back,
//! injection rules, the give-up deadline, session-end triggers and the process-global
//! ghost-eviction token map. Returns a working `SshStdioBridge` value. Does not own ssh
//! argument details.
//!
//! Threads per connection: the bridge thread is the leg I/O thread (it owns the
//! `SessionEndpoint`, the `FrameSplitter` and the state machine and never blocks on the
//! network or the client socket); per leg a reader and a writer thread; a client-reader (started
//! only after leg 0's `WELCOME new`, the pre-WELCOME gate) and a client-writer that owns every
//! blocking client-socket write.

use super::super::{bridge_connection, ManagedSshOptions, SshStdioBridge};
use super::link_frames::{
    encode_echo_subscribe_frame, encode_link_frame, encode_resync_frame, FrameClass, FrameSplitter,
};
use super::render_reorder::{GapCause, RenderArrival, RenderReorder, ReorderOutput};
use super::session::{
    self, end_reason, Decoder, LinkVerdict, Role, SessionEndpoint, SessionEvent, SessionMsg, Token,
    WelcomeStatus, MAX_DATA_CHUNK,
};
use super::ring::RingOverflow;
use super::ssh_leg::{Leg, LegDialer, LegEndReport, LegError, LegKind, LEG_REAP_BUDGET};
use crate::client::mosh_outage::LinkStatus;
use crate::ipc::{self, LocalStream};
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::TryClone as _;
use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const LOCAL_RING_CAP: usize = 8 * 1024 * 1024;
/// Upper bound on how long the pump sleeps; also the `should_stop` poll period (spec: <= 100 ms).
const TICK: Duration = Duration::from_millis(50);
const RESUMING_INJECT_EVERY: Duration = Duration::from_secs(1);
/// The one deadline of a closing connection: how long the END/ACK has to reach the keeper (leg
/// write completed and leg EOF), and how long the client writer has to finish behind its frames.
const CLOSE_BUDGET: Duration = Duration::from_millis(500);
/// Receive timeout while closing, so flush, EOF and deadline checks are not quantised to `TICK`.
const CLOSING_TICK: Duration = Duration::from_millis(5);
/// After a ring overflow the client is already closed; the keeper is told on the next leg, but
/// only for this long.
const OVERFLOW_END_LINGER: Duration = Duration::from_secs(5);
const BRIDGE_ACCEPT_POLL: Duration = Duration::from_millis(50);
const BRIDGE_SOCKET_PERMISSION_MODE: u32 = 0o600;

/// Last session token per bridge local-socket path. Process-global (not per bridge instance) so
/// a saved machine's reconnect, which builds a new bridge for the same path, can name the
/// session it replaces as `previous_token` (keeper-side ghost eviction).
static LAST_TOKEN: Mutex<BTreeMap<PathBuf, Token>> = Mutex::new(BTreeMap::new());

fn last_token(path: &Path) -> Option<Token> {
    LAST_TOKEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .copied()
}

fn store_token(path: &Path, token: Token) {
    LAST_TOKEN
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf(), token);
}

/// What serves a connection whose leg 0 fails on the remote side before `WELCOME`.
pub(super) enum PlainFallback {
    /// Upstream `bridge_connection` with today's leg-0 options.
    Ssh {
        target: String,
        remote_command: String,
        ssh_options: Option<ManagedSshOptions>,
        noninteractive: bool,
    },
    #[cfg(test)]
    Test(Box<dyn Fn(LocalStream) -> io::Result<()> + Send>),
}

impl PlainFallback {
    fn run(&self, stream: LocalStream, stop: &Arc<AtomicBool>) -> io::Result<()> {
        match self {
            PlainFallback::Ssh {
                target,
                remote_command,
                ssh_options,
                noninteractive,
            } => bridge_connection(
                stream,
                target,
                remote_command,
                ssh_options.as_ref(),
                *noninteractive,
                stop,
            ),
            #[cfg(test)]
            PlainFallback::Test(serve) => serve(stream),
        }
    }
}

pub(super) struct BridgeParams {
    pub local_socket: PathBuf,
    pub grace: Duration,
    pub noninteractive: bool,
    pub plain: PlainFallback,
    /// `HERDR_MOSH_TRACE`: directory receiving `local.jsonl`.
    pub trace_dir: Option<PathBuf>,
    /// The remote advertised `x-mosh-supersede-v1`: HELLO asks for the render split and QUIC
    /// legs carrying a render lane host a `RenderReorder`.
    pub render_split: bool,
    /// The remote advertised `x-mosh-echo-v1` and prediction is enabled: the bridge subscribes
    /// to echo acknowledgements once per session and reports `echo` in `Linked`.
    pub echo: bool,
}

/// Bind the user-facing socket exactly like `SshStdioBridge::start_command` and serve one
/// connection at a time with the resumable pump.
pub(super) fn start(
    params: BridgeParams,
    dialer: Box<dyn LegDialer>,
    should_stop: Arc<AtomicBool>,
) -> io::Result<SshStdioBridge> {
    let local_socket = params.local_socket.clone();
    ipc::prepare_socket_path(&local_socket, |path| {
        format!("remote bridge is already listening at {}", path.display())
    })?;
    let listener = ipc::bind_private_local_listener(&local_socket)?;
    let socket_identity = ipc::socket_file_identity(&local_socket)?;
    if let Err(err) = ipc::restrict_socket_permissions(&local_socket, BRIDGE_SOCKET_PERMISSION_MODE)
    {
        let _ = ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
        return Err(err);
    }
    if let Err(err) = listener.set_nonblocking(ListenerNonblockingMode::Accept) {
        let _ = ipc::remove_socket_file_if_owned(&local_socket, &socket_identity);
        return Err(err);
    }

    let thread_stop = Arc::clone(&should_stop);
    let (failure_tx, failure_rx) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        let trace = Trace::new(params.trace_dir.as_deref());
        let dialer = Arc::new(Mutex::new(dialer));
        while !thread_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok(stream) => {
                    if let Err(err) = serve_connection(stream, &params, &dialer, &thread_stop, &trace)
                    {
                        let _ = failure_tx.try_send(io::Error::new(err.kind(), err.to_string()));
                        if params.noninteractive {
                            tracing::warn!(error = %err, "saved SSH endpoint bridge failed");
                        } else {
                            eprintln!("herdr: remote bridge failed: {err}");
                        }
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(BRIDGE_ACCEPT_POLL);
                }
                Err(err) => {
                    if params.noninteractive {
                        tracing::warn!(error = %err, "saved SSH endpoint listener failed");
                    } else {
                        eprintln!("herdr: remote bridge listener failed: {err}");
                    }
                    break;
                }
            }
        }
    });

    Ok(SshStdioBridge {
        local_socket,
        socket_identity,
        should_stop,
        failure_rx,
        thread: Some(thread),
    })
}

/// `HERDR_MOSH_TRACE=<dir>`: JSON lines in `<dir>/local.jsonl`.
struct Trace {
    file: Option<Mutex<File>>,
    origin: Instant,
}

impl Trace {
    fn new(dir: Option<&std::path::Path>) -> Trace {
        let file = dir.and_then(|dir| {
            std::fs::create_dir_all(dir).ok()?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("local.jsonl"))
                .ok()
                .map(Mutex::new)
        });
        Trace {
            file,
            origin: Instant::now(),
        }
    }

    fn emit(&self, event: &str, fields: serde_json::Value) {
        let Some(file) = &self.file else { return };
        let mut line = serde_json::json!({
            "t_ms": self.origin.elapsed().as_millis() as u64,
            "event": event,
        });
        if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), fields) {
            line.extend(fields);
        }
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{line}");
    }
}

type SharedDialer = Arc<Mutex<Box<dyn LegDialer>>>;

fn lock_dialer(dialer: &SharedDialer) -> std::sync::MutexGuard<'_, Box<dyn LegDialer>> {
    dialer.lock().unwrap_or_else(PoisonError::into_inner)
}

fn serve_connection(
    stream: LocalStream,
    params: &BridgeParams,
    dialer: &SharedDialer,
    stop: &Arc<AtomicBool>,
    trace: &Trace,
) -> io::Result<()> {
    let token = Token::generate()?;
    let mut conn = Conn::new(params, dialer, stop, trace, token, Some(stream));
    conn.run();
    conn.finish()
}

enum Event {
    Leg(u64, LegIn),
    Dialed(Result<Leg, LegError>),
    Client(Vec<u8>),
    ClientEof,
    /// A render-lane arrival of leg generation `.0`.
    Lane(u64, RenderArrival),
}

enum LegIn {
    Bytes(Vec<u8>),
    Eof,
}

/// Where `Conn::inject_upstream` may write an injected client->server frame.
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum InjectAt {
    /// At the next client frame boundary (at once when already at one).
    NextBoundary,
    /// At the first boundary after the client's first frame (its HELLO) has gone through.
    AfterFirstFrame,
}

/// What `Conn::inject_upstream` did with a frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Injection {
    /// Pushed to the session stream.
    Written,
    /// Waiting for the client frame in flight to complete.
    Queued,
    /// Refused or lost.
    Dropped,
}

enum ClientOut {
    Stream { bytes: Vec<u8>, end_offset: u64 },
    Injected(Vec<u8>),
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// A dial is in flight (`Conn::kind` says which leg).
    Dialing,
    /// A leg is up and HELLO was written; waiting for WELCOME.
    Welcome,
    Linked,
    Backoff(Instant),
}

/// Re-inject a changed SRTT only when it moved by more than this fraction of the last reported
/// value (1/5) or by more than `SRTT_REPORT_ABS`.
const SRTT_REPORT_ABS: Duration = Duration::from_millis(20);

/// The last SRTT reported to the client and the re-injection threshold.
#[derive(Debug, Default)]
struct SrttReporter {
    last: Option<Duration>,
}

impl SrttReporter {
    /// `Some(ms)` when `current` should be injected: the first known value, or a change of
    /// more than 20 % or more than 20 ms from the last reported one. Unknown never injects.
    fn update(&mut self, current: Option<Duration>) -> Option<u64> {
        let current = current?;
        let inject = match self.last {
            None => true,
            Some(last) => {
                let delta = current.abs_diff(last);
                delta * 5 > last || delta > SRTT_REPORT_ABS
            }
        };
        inject.then(|| {
            self.last = Some(current);
            as_ms(current)
        })
    }

    /// A `Linked` was injected for another reason carrying `reported`; remember it.
    fn reported(&mut self, reported: Option<Duration>) {
        self.last = reported;
    }
}

fn as_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

struct Down {
    give_up_at: Instant,
    last_inject: Option<Instant>,
    resuming_injected: bool,
}

/// How the run loop ended.
enum Done {
    /// Serve this connection through the upstream plain bridge with the untouched stream.
    Plain,
    /// The Closing phase finished; `finish` only releases resources, bounded by `reap_by`.
    Closed {
        error: Option<io::Error>,
        /// Kill the ssh child at once: no END/ACK was owed to the keeper.
        kill: bool,
        reap_by: Instant,
    },
}

/// Why the connection is closing.
enum CloseWhy {
    /// `should_stop`: `END{client-gone}` goes out now (ahead of unsent ring bytes) and the client
    /// is shut at once.
    Stopped,
    /// The session-final END the endpoint queued (client EOF / ring overflow) was handed to the
    /// leg writer.
    EndSent,
    /// `END{server-eof}` received: deliver the client queue, ack what the client actually wrote,
    /// then close behind the frames.
    ServerEof,
    /// Any other end (give-up, fatal reconnect, session-gone, oversized frame, linger expiry, leg-0
    /// failure, client setup): close the client behind its queued frames; no END or ACK owed.
    Ended {
        reason: String,
        error: Option<io::Error>,
    },
}

impl CloseWhy {
    fn reason(&self) -> &str {
        match self {
            CloseWhy::Stopped => "stopped",
            CloseWhy::EndSent => end_reason::CLIENT_GONE,
            CloseWhy::ServerEof => end_reason::SERVER_EOF,
            CloseWhy::Ended { reason, .. } => reason,
        }
    }
}

/// The one teardown phase (spec §Post-Implementation Note, code redesign cycle 1). The run loop
/// keeps running while it is open: heartbeats, acks, leg EOF and `should_stop` are still
/// observed, and nothing here blocks on the client socket.
struct Closing {
    why: CloseWhy,
    /// An END or final ACK is owed to the leg: wait for its write and the leg's EOF.
    leg_owed: bool,
    /// `LegIo::sent` count that must be flushed: the END and/or the final ACK. `None` until it
    /// is handed to the leg writer.
    leg_target: Option<u64>,
    /// Bounds the END/ACK leg write, leg EOF and the ssh reap.
    deadline: Instant,
    /// ServerEof only: keep delivering the client queue until this instant (the remaining
    /// grace) before the ACK and the client close.
    drain_until: Option<Instant>,
    /// Bounds the client writer's own sub-wait after `ClientOut::Close`; `None` until the close
    /// is sent (ServerEof sends it after the leg settles, so the ACK goes first).
    client_deadline: Option<Instant>,
}

struct LegIo {
    gen: u64,
    leg: Leg,
    tx: mpsc::SyncSender<Vec<u8>>,
    flushed: Arc<AtomicU64>,
    /// Messages handed to the leg writer so far.
    sent: u64,
    /// A message the writer queue had no room for.
    pending: Option<Vec<u8>>,
    /// The leg reader thread has exited (the far end closed, or the child died).
    reader_done: Arc<AtomicBool>,
    /// The leg writer thread has exited (a write failed, or its sender was dropped).
    writer_done: Arc<AtomicBool>,
    /// Present on a lane-bearing leg of a render-split session: orders the lane's frames.
    reorder: Option<RenderReorder>,
}

impl LegIo {
    /// A thread that makes this leg's progress has exited, so waiting on it cannot succeed.
    fn worker_exited(&self) -> bool {
        self.reader_done.load(Ordering::Acquire) || self.writer_done.load(Ordering::Acquire)
    }
}

struct ClientIo {
    go_tx: mpsc::Sender<usize>,
    out_tx: mpsc::Sender<ClientOut>,
    written: Arc<AtomicU64>,
    /// A handle on the socket used only to shut it down.
    shutdown: LocalStream,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

fn shutdown_client(stream: &LocalStream) {
    let LocalStream::UdSocket(stream) = stream;
    let _ = stream.inner().shutdown(std::net::Shutdown::Both);
}

struct Conn<'a> {
    params: &'a BridgeParams,
    dialer: &'a SharedDialer,
    stop: &'a Arc<AtomicBool>,
    trace: &'a Trace,
    /// Per-connection stop flag handed to dials; set when `should_stop` is seen or the
    /// connection ends.
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    endpoint: SessionEndpoint,
    /// What the client was last told about the link's SRTT.
    srtt_report: SrttReporter,
    splitter: FrameSplitter,
    /// Client->server frame boundary tracker: sees the same bytes as the endpoint's source and
    /// its yielded frames are discarded.
    upstream: FrameSplitter,
    /// The tracker saw an oversized prefix and is unusable: injections are refused.
    upstream_untracked: bool,
    /// Frames waiting for the next client frame boundary (see `inject_upstream`).
    pending_inject: VecDeque<Vec<u8>>,
    /// The echo subscribe was requested this session (once).
    echo_requested: bool,
    /// The echo subscribe frame was written to the session stream: `Linked` carries `echo`.
    echo_injected: bool,
    /// A `Linked` carrying `echo` is owed to the client.
    echo_report_due: bool,
    /// When the last gap resync was requested and has not yet been answered by a full surface.
    resync_at: Option<Instant>,
    /// When the transport counters were last traced.
    last_stats: Instant,
    /// WELCOMEs received this connection (the resume rule fires on every one after the first).
    welcomes: u32,
    /// The client stream until the client threads take it (and for the leg-0 fallback).
    stream: Option<LocalStream>,
    client: Option<ClientIo>,
    leg: Option<LegIo>,
    /// An ssh leg held in reserve while a QUIC leg 0 is tried: switched to if that leg ends
    /// before `WELCOME`, dropped at `WELCOME`.
    standby: Option<Leg>,
    /// The token of an abandoned leg-0 attempt, named as `previous_token` in the next HELLO only
    /// (the keeper may already have admitted that session).
    ghost: Option<Token>,
    leg_gen: u64,
    stage: Stage,
    kind: LegKind,
    attempt: u32,
    down: Option<Down>,
    /// Stream bytes of the complete frames handed to the client writer.
    frames_end: u64,
    /// The client side is finished (EOF, error or overflow): nothing more is forwarded.
    client_closed: bool,
    /// ClientEof arrived while Closing: the client will read nothing more.
    client_eof_seen: bool,
    /// The client reader waits for a go token.
    client_idle: bool,
    end_deadline: Option<Instant>,
    closing: Option<Closing>,
    /// The client socket was shut down (stop); nothing waits on its writer.
    client_shut: bool,
    /// Pre-WELCOME copy of the inbound bytes, only to learn WELCOME's `recv_offset` for tracing.
    peek: Decoder,
    peer_recv_offset: Option<u64>,
    dial_thread: Option<JoinHandle<()>>,
    done: Option<Done>,
}

impl<'a> Conn<'a> {
    fn new(
        params: &'a BridgeParams,
        dialer: &'a SharedDialer,
        stop: &'a Arc<AtomicBool>,
        trace: &'a Trace,
        token: Token,
        stream: Option<LocalStream>,
    ) -> Conn<'a> {
        let (tx, rx) = mpsc::channel();
        Conn {
            params,
            dialer,
            stop,
            trace,
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            endpoint: SessionEndpoint::new(Role::Local, token, LOCAL_RING_CAP),
            srtt_report: SrttReporter::default(),
            splitter: FrameSplitter::new(),
            upstream: FrameSplitter::new(),
            upstream_untracked: false,
            pending_inject: VecDeque::new(),
            echo_requested: false,
            echo_injected: false,
            echo_report_due: false,
            resync_at: None,
            last_stats: Instant::now(),
            welcomes: 0,
            stream,
            client: None,
            leg: None,
            standby: None,
            ghost: None,
            leg_gen: 0,
            stage: Stage::Dialing,
            kind: LegKind::Initial,
            attempt: 0,
            down: None,
            frames_end: 0,
            client_closed: false,
            client_eof_seen: false,
            client_idle: false,
            end_deadline: None,
            closing: None,
            client_shut: false,
            peek: Decoder::default(),
            peer_recv_offset: None,
            dial_thread: None,
            done: None,
        }
    }
}

impl Conn<'_> {
    fn run(&mut self) {
        self.start_dial(LegKind::Initial, None);
        while self.done.is_none() {
            let wait = if self.closing.is_some() { CLOSING_TICK } else { TICK };
            let wait = self.reorder_wait(wait);
            match self.rx.recv_timeout(wait) {
                Ok(event) => self.on_event(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            for _ in 0..256 {
                if self.done.is_some() {
                    break;
                }
                match self.rx.try_recv() {
                    Ok(event) => self.on_event(event),
                    Err(_) => break,
                }
            }
            if self.done.is_none() {
                self.tick();
            }
        }
    }

    /// Enter Closing for any end that owes the keeper nothing: close the client behind its
    /// queued frames (bounded sub-wait). The first reason wins.
    fn end_session(&mut self, reason: &str, error: Option<io::Error>) {
        if self.closing.is_some() {
            return;
        }
        let by = Instant::now() + CLOSE_BUDGET;
        self.client_send(ClientOut::Close);
        self.closing = Some(Closing {
            why: CloseWhy::Ended {
                reason: reason.to_owned(),
                error,
            },
            leg_owed: false,
            leg_target: None,
            deadline: by,
            drain_until: None,
            client_deadline: Some(by),
        });
    }

    // ---- dialing and leg setup ------------------------------------------------------------

    fn start_dial(&mut self, kind: LegKind, setup_deadline: Option<Instant>) {
        self.kind = kind;
        self.stage = Stage::Dialing;
        let dialer = Arc::clone(self.dialer);
        let cancel = Arc::clone(&self.cancel);
        let tx = self.tx.clone();
        self.dial_thread = Some(thread::spawn(move || {
            let result = lock_dialer(&dialer).dial(kind, setup_deadline, &cancel);
            let _ = tx.send(Event::Dialed(result));
        }));
    }

    fn on_dialed(&mut self, result: Result<Leg, LegError>) {
        if let Some(handle) = self.dial_thread.take() {
            let _ = handle.join();
        }
        let stopped = self.stop.load(Ordering::Acquire);
        if stopped || self.closing.is_some() {
            if let Ok(leg) = result {
                reap_in_background(leg);
            }
            // A dial finishing while merely Closing leaves the graceful close and its reason alone.
            if stopped {
                self.on_stop(Instant::now());
            }
            return;
        }
        match result {
            Ok(leg) => self.begin_leg(leg),
            Err(err) => self.dial_failed(err),
        }
    }

    /// A dial, or a leg that ended before `WELCOME`, failed with `err`.
    fn dial_failed(&mut self, err: LegError) {
        match (self.kind, err) {
            (LegKind::Initial, LegError::Unsupported(err)) => {
                tracing::debug!(%err, "mosh leg 0 unsupported; plain bridge");
                self.done = Some(Done::Plain);
            }
            (LegKind::Initial, LegError::Fatal(err)) => self.end_session("leg0-failed", Some(err)),
            // Only a stop makes leg 0 retryable.
            (LegKind::Initial, LegError::Retryable(_)) => self.on_stop(Instant::now()),
            (LegKind::Reconnect, LegError::Retryable(err)) => {
                tracing::debug!(%err, attempt = self.attempt, "mosh reconnect attempt failed");
                self.attempt += 1;
                self.schedule_backoff();
            }
            (LegKind::Reconnect, LegError::Fatal(err) | LegError::Unsupported(err)) => {
                self.end_session("reconnect-fatal", Some(err));
            }
        }
    }

    fn schedule_backoff(&mut self) {
        let wait = lock_dialer(self.dialer).backoff(self.attempt);
        self.kind = LegKind::Reconnect;
        self.stage = Stage::Backoff(Instant::now() + wait);
    }

    fn begin_leg(&mut self, mut leg: Leg) {
        let (Some(mut reader), Some(mut writer)) = (leg.take_reader(), leg.take_writer()) else {
            self.end_session(
                "leg-io",
                Some(io::Error::other("leg has no byte stream halves")),
            );
            return;
        };
        // The token and silence limit belong to the leg; the bridge only passes them on.
        self.endpoint
            .set_quic_token(leg.admission_token().map(str::to_owned));
        self.endpoint.set_silence_timeout(leg.silence_timeout());
        self.endpoint.set_render_split(self.params.render_split);
        let lane = leg.take_render_lane();
        if let Some(standby) = leg.take_standby() {
            self.standby = Some(standby);
        }
        self.leg_gen += 1;
        let gen = self.leg_gen;
        let events = self.tx.clone();
        let reader_done = Arc::new(AtomicBool::new(false));
        let reader_flag = Arc::clone(&reader_done);
        thread::spawn(move || {
            let mut buf = vec![0u8; MAX_DATA_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = events.send(Event::Leg(gen, LegIn::Eof));
                        break;
                    }
                    Ok(n) => {
                        if events
                            .send(Event::Leg(gen, LegIn::Bytes(buf[..n].to_vec())))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        let _ = events.send(Event::Leg(gen, LegIn::Eof));
                        break;
                    }
                }
            }
            reader_flag.store(true, Ordering::Release);
        });
        let reorder = match lane {
            Some(lane) if self.params.render_split => {
                let lane_events = self.tx.clone();
                thread::spawn(move || {
                    for arrival in lane {
                        if lane_events.send(Event::Lane(gen, arrival)).is_err() {
                            break;
                        }
                    }
                });
                Some(RenderReorder::new())
            }
            _ => None,
        };
        let (leg_tx, leg_rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let flushed = Arc::new(AtomicU64::new(0));
        let writer_flushed = Arc::clone(&flushed);
        let writer_done = Arc::new(AtomicBool::new(false));
        let writer_flag = Arc::clone(&writer_done);
        thread::spawn(move || {
            for message in leg_rx {
                if writer.write_all(&message).and_then(|()| writer.flush()).is_err() {
                    break;
                }
                writer_flushed.fetch_add(1, Ordering::Release);
            }
            writer_flag.store(true, Ordering::Release);
        });

        let now = Instant::now();
        let previous = self.ghost.take().or_else(|| match self.kind {
            LegKind::Initial => last_token(&self.params.local_socket),
            LegKind::Reconnect => None,
        });
        let grace_ms = u32::try_from(self.params.grace.as_millis()).unwrap_or(u32::MAX);
        let hello = self.endpoint.hello(now, grace_ms, previous);
        let _ = leg_tx.try_send(hello);
        self.peek = Decoder::default();
        self.peer_recv_offset = None;
        self.leg = Some(LegIo {
            gen,
            leg,
            tx: leg_tx,
            flushed,
            sent: 1,
            pending: None,
            reader_done,
            writer_done,
            reorder,
        });
        self.stage = Stage::Welcome;
    }

    /// Tear the current leg down and return its end report, bounded by `LEG_REAP_BUDGET`.
    fn kill_leg(&mut self, kill: bool) -> Option<LegEndReport> {
        let LegIo { leg, tx, .. } = self.leg.take()?;
        drop(tx);
        Some(leg.finish(kill, Instant::now() + LEG_REAP_BUDGET))
    }

    /// Drop the current leg without waiting for it: a reaper thread kills and reaps it, so a
    /// dying leg never blocks the Resuming injection or the give-up timer.
    fn reap_leg(&mut self) {
        if let Some(LegIo { leg, tx, .. }) = self.leg.take() {
            drop(tx);
            reap_in_background(leg);
        }
    }

    /// The leg ended before `WELCOME` (explicit END, EOF, protocol error or the bound).
    fn pre_welcome_failed(&mut self, kill: bool) {
        let Some(report) = self.kill_leg(kill) else {
            return;
        };
        if let Some(standby) = self.standby.take() {
            self.switch_to_standby(standby);
            return;
        }
        self.endpoint.leg_down();
        let err = lock_dialer(self.dialer).classify(self.kind, &report);
        self.dial_failed(err);
    }

    /// Leg 0 (QUIC) ended before `WELCOME`: send a leg-0-style HELLO on the ssh standby, with a
    /// fresh session token and the abandoned attempt's token as `previous_token`, so the keeper's
    /// ghost eviction removes a session it may already have admitted. Nothing was read from the
    /// client yet (the pre-`WELCOME` gate), so a fresh endpoint loses nothing. Phase 1's leg-0
    /// table applies only if the standby also ends before `WELCOME`.
    fn switch_to_standby(&mut self, standby: Leg) {
        let token = match Token::generate() {
            Ok(token) => token,
            Err(err) => {
                self.end_session("leg0-failed", Some(err));
                return;
            }
        };
        tracing::debug!("mosh leg 0 ended before WELCOME; switching to the ssh standby");
        self.ghost = Some(self.endpoint.token());
        self.endpoint = SessionEndpoint::new(Role::Local, token, LOCAL_RING_CAP);
        self.begin_leg(standby);
    }

    // ---- inbound leg bytes ----------------------------------------------------------------

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Dialed(result) => self.on_dialed(result),
            Event::Leg(gen, input) => {
                if self.leg.as_ref().map(|io| io.gen) != Some(gen) {
                    return; // a leg already torn down
                }
                match input {
                    LegIn::Bytes(bytes) => self.on_leg_bytes(&bytes),
                    LegIn::Eof => self.leg_ended("eof"),
                }
            }
            Event::Client(bytes) => self.on_client(&bytes),
            Event::ClientEof => self.on_client_eof(),
            Event::Lane(gen, arrival) => self.on_lane(gen, arrival),
        }
    }

    fn leg_ended(&mut self, reason: &str) {
        if let Some(closing) = &self.closing {
            // Closing polls the leg itself, except while draining a server-eof, where a dead
            // leg is dropped and never resumed.
            if closing.drain_until.is_some() {
                self.reap_leg();
                self.endpoint.leg_down();
            }
            return;
        }
        match self.stage {
            Stage::Welcome => self.pre_welcome_failed(reason != "eof"),
            Stage::Linked => self.leg_dead(reason),
            _ => {}
        }
    }

    /// Whether leg input and the send pump are live: always, except while Closing for anything
    /// but a server-eof drain.
    fn pumping(&self) -> bool {
        self.closing.as_ref().is_none_or(|c| c.drain_until.is_some())
    }

    fn on_leg_bytes(&mut self, bytes: &[u8]) {
        if !self.pumping() {
            return;
        }
        let now = Instant::now();
        if self.stage == Stage::Welcome {
            self.peek.push(bytes);
            while let Ok(Some(message)) = self.peek.next() {
                if let SessionMsg::Welcome(welcome) = message {
                    self.peer_recv_offset = Some(welcome.recv_offset);
                }
            }
        }
        let (events, error) = match self.endpoint.on_leg_bytes(now, bytes) {
            Ok(events) => (events, None),
            Err(err) => (err.delivered, Some(err.error)),
        };
        // Bytes decoded before a protocol error are already counted in recv_offset and will
        // not be resent: deliver them before the leg is dropped.
        for event in events {
            if self.leg.is_none() || !self.pumping() {
                break;
            }
            self.on_session_event(event);
        }
        if let Some(err) = error {
            if self.leg.is_some() && self.pumping() {
                self.leg_ended(&format!("protocol error: {err}"));
            }
        }
        if self.stage == Stage::Linked && self.pumping() {
            self.pump_sends(Instant::now());
            self.feed_client();
        }
    }

    fn on_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Deliver(bytes) => self.deliver(&bytes),
            SessionEvent::Welcomed(status) => self.on_welcomed(status),
            SessionEvent::ResumeRefused => {
                self.trace.emit("welcome", serde_json::json!({"status": "refused"}));
                self.end_session("session-gone", None);
            }
            SessionEvent::PeerEnd(reason) => {
                if self.stage == Stage::Welcome {
                    if let Some(io) = &mut self.leg {
                        io.leg.record_pre_welcome_end(reason);
                    }
                    self.pre_welcome_failed(false);
                } else if reason == end_reason::SERVER_EOF {
                    self.begin_server_eof();
                } else {
                    self.end_session(&reason, None);
                }
            }
            SessionEvent::Hello(_) => self.leg_ended("unexpected HELLO"),
        }
    }

    fn deliver(&mut self, bytes: &[u8]) {
        if self.client_closed {
            return;
        }
        // On a lane-bearing leg render-class frames come on the lane; any on the session stream
        // are ring replays from an earlier leg, superseded by the resume resync. They still
        // count toward the ack point.
        let lane_leg = self.leg.as_ref().is_some_and(|io| io.reorder.is_some());
        match self.splitter.push(bytes) {
            Ok(frames) => {
                for frame in frames {
                    self.frames_end += frame.len() as u64;
                    let end_offset = self.frames_end;
                    let bytes = if lane_leg && FrameClass::of(&frame) != FrameClass::Reliable {
                        Vec::new()
                    } else {
                        frame
                    };
                    self.client_send(ClientOut::Stream { bytes, end_offset });
                }
            }
            Err(oversized) => self.end_session(
                "oversized-frame",
                Some(io::Error::other(format!(
                    "remote sent a frame of {} bytes (max {})",
                    oversized.claimed, oversized.max
                ))),
            ),
        }
    }

    fn client_send(&self, out: ClientOut) {
        if let Some(client) = &self.client {
            let _ = client.out_tx.send(out);
        }
    }

    fn on_welcomed(&mut self, status: WelcomeStatus) {
        if let Some(io) = &mut self.leg {
            io.leg.mark_welcome();
        }
        if let Some(standby) = self.standby.take() {
            reap_in_background(standby);
        }
        let status_name = match status {
            WelcomeStatus::New => "new",
            WelcomeStatus::Resumed => "resumed",
            WelcomeStatus::Unknown => "unknown",
        };
        self.trace
            .emit("welcome", serde_json::json!({ "status": status_name }));
        match status {
            WelcomeStatus::New => {
                store_token(&self.params.local_socket, self.endpoint.token());
                if let Err(err) = self.start_client() {
                    self.end_session("client-setup", Some(err));
                    return;
                }
            }
            WelcomeStatus::Resumed => {
                let replayed_up = self
                    .peer_recv_offset
                    .map(|offset| self.endpoint.outbound_end().saturating_sub(offset));
                // The local end cannot see how much the keeper replays downstream.
                self.trace.emit(
                    "resume",
                    serde_json::json!({"replayed_up": replayed_up, "replayed_down": null}),
                );
                if let Some(down) = self.down.take() {
                    if down.resuming_injected && !self.client_closed {
                        // The new leg has no samples yet: tell the client it is unknown.
                        let srtt = self.srtt();
                        self.inject(LinkStatus::Linked {
                            srtt_ms: srtt.map(as_ms),
                            echo: self.echo_injected,
                        });
                        self.srtt_report.reported(srtt);
                    }
                }
                self.attempt = 0;
            }
            WelcomeStatus::Unknown => {}
        }
        self.stage = Stage::Linked;
        self.trace.emit("leg_up", serde_json::json!({}));
        self.welcomes += 1;
        if self.welcomes > 1 && status != WelcomeStatus::Unknown {
            self.request_resync("resume");
        }
    }

    // ---- the client side ------------------------------------------------------------------

    /// Start the client threads. Only called after leg 0's `WELCOME new`: until then nothing
    /// has been read from the client socket.
    fn start_client(&mut self) -> io::Result<()> {
        let stream = self
            .stream
            .take()
            .ok_or_else(|| io::Error::other("client stream already taken"))?;
        let read_half = stream.try_clone()?;
        let shutdown = stream.try_clone()?;
        let written = Arc::new(AtomicU64::new(0));

        let (go_tx, go_rx) = mpsc::channel::<usize>();
        let events = self.tx.clone();
        let reader = thread::spawn(move || {
            let mut read_half = read_half;
            let mut buf = vec![0u8; MAX_DATA_CHUNK];
            while let Ok(size) = go_rx.recv() {
                loop {
                    match read_half.read(&mut buf[..size.min(MAX_DATA_CHUNK)]) {
                        Ok(0) => {
                            let _ = events.send(Event::ClientEof);
                            return;
                        }
                        Ok(n) => {
                            let _ = events.send(Event::Client(buf[..n].to_vec()));
                            break;
                        }
                        Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => {
                            let _ = events.send(Event::ClientEof);
                            return;
                        }
                    }
                }
            }
        });

        let (out_tx, out_rx) = mpsc::channel::<ClientOut>();
        let writer_written = Arc::clone(&written);
        let writer = thread::spawn(move || {
            let mut stream = stream;
            for out in out_rx {
                let result = match out {
                    ClientOut::Stream { bytes, end_offset } => {
                        let result = stream.write_all(&bytes);
                        if result.is_ok() {
                            writer_written.store(end_offset, Ordering::Release);
                        }
                        result
                    }
                    ClientOut::Injected(bytes) => stream.write_all(&bytes),
                    ClientOut::Close => break,
                };
                if result.is_err() {
                    break;
                }
            }
            shutdown_client(&stream);
        });

        let _ = go_tx.send(MAX_DATA_CHUNK);
        self.client = Some(ClientIo {
            go_tx,
            out_tx,
            written,
            shutdown,
            reader,
            writer,
        });
        self.client_idle = false;
        self.request_echo_subscribe();
        Ok(())
    }

    fn on_client(&mut self, bytes: &[u8]) {
        self.client_idle = true;
        if self.client_closed || self.closing.is_some() {
            return;
        }
        if self.push_client_bytes(bytes).is_err() {
            // Only possible while no leg is up: the session ends here.
            self.trace.emit(
                "session_end",
                serde_json::json!({"reason": end_reason::RING_OVERFLOW}),
            );
            self.endpoint.queue_final_end(end_reason::RING_OVERFLOW);
            self.client_gone();
            self.end_deadline = Some(Instant::now() + OVERFLOW_END_LINGER);
        } else {
            if self.echo_requested
                && !self.echo_injected
                && !self.upstream_untracked
                && self.pending_inject.is_empty()
            {
                // The push completed the client's HELLO and flushed the queued subscribe.
                self.echo_subscribed();
            }
            if self.stage == Stage::Linked {
                self.pump_sends(Instant::now());
            }
            self.feed_client();
        }
    }

    fn on_client_eof(&mut self) {
        if self.client_closed {
            return;
        }
        if self.closing.is_some() {
            self.client_eof_seen = true;
            return;
        }
        self.endpoint.queue_final_end(end_reason::CLIENT_GONE);
        self.client_gone();
    }

    /// Close the client socket behind the frames already queued; the keeper is still told.
    fn client_gone(&mut self) {
        self.client_closed = true;
        self.client_send(ClientOut::Close);
    }

    // ---- upstream injection and the render lane ------------------------------------------------

    /// Push client bytes to the endpoint's source and track client frame boundaries. A frame
    /// queued by `inject_upstream` is pushed at the first boundary after it, inside the chunk
    /// when the chunk completes the frame, so it never lands within a client frame.
    fn push_client_bytes(&mut self, bytes: &[u8]) -> Result<(), RingOverflow> {
        if self.upstream_untracked {
            return self.endpoint.push_source(bytes);
        }
        let mut held = self.upstream.held_back();
        let frames = match self.upstream.push(bytes) {
            Ok(frames) => frames,
            Err(_) => {
                self.upstream_untracked = true;
                self.pending_inject.clear();
                return self.endpoint.push_source(bytes);
            }
        };
        let mut pos = 0;
        for frame in &frames {
            let end = pos + frame.len() - held;
            held = 0;
            self.endpoint.push_source(&bytes[pos..end])?;
            pos = end;
            while let Some(injected) = self.pending_inject.pop_front() {
                self.endpoint.push_source(&injected)?;
            }
        }
        self.endpoint.push_source(&bytes[pos..])
    }

    /// Make `frame` offset-bearing session DATA toward the keeper (so a new leg replays it),
    /// written only at a client frame boundary. Pushes to the endpoint at once when the
    /// tracker is at a boundary (and, for `AfterFirstFrame`, a client frame was seen);
    /// otherwise the frame waits for the push that completes the client frame in flight.
    fn inject_upstream(&mut self, frame: Vec<u8>, at: InjectAt) -> Injection {
        if self.upstream_untracked || self.client_closed || self.closing.is_some() {
            return Injection::Dropped;
        }
        let ready = self.pending_inject.is_empty()
            && self.upstream.at_boundary()
            && (at == InjectAt::NextBoundary || self.upstream.frames_seen() >= 1);
        if !ready {
            self.pending_inject.push_back(frame);
            Injection::Queued
        } else if self.endpoint.push_source(&frame).is_err() {
            // Only possible while no leg is up and the ring is full; the next client write
            // ends the session on overflow.
            tracing::debug!("mosh injected frame dropped: ring full");
            Injection::Dropped
        } else {
            Injection::Written
        }
    }

    /// Subscribe to echo acknowledgements, once per session, right after the client's HELLO.
    /// Called when the client starts; the frame waits for HELLO to go through.
    fn request_echo_subscribe(&mut self) {
        if !self.params.echo || self.echo_requested {
            return;
        }
        self.echo_requested = true;
        match self.inject_upstream(encode_echo_subscribe_frame(), InjectAt::AfterFirstFrame) {
            Injection::Written => self.echo_subscribed(),
            Injection::Queued | Injection::Dropped => {}
        }
    }

    /// The subscribe frame is on the session stream (and so replayed on every new leg).
    fn echo_subscribed(&mut self) {
        self.echo_injected = true;
        self.echo_report_due = true;
        self.trace.emit("echo_subscribe", serde_json::json!({}));
    }

    /// The one resync path: a gap on the lane, or a resume. Only render-split sessions have a
    /// server that answers it. After the client's first frame, so it never precedes the
    /// client's HELLO.
    fn request_resync(&mut self, reason: &str) {
        if !self.params.render_split {
            return;
        }
        self.trace.emit("resync", serde_json::json!({ "reason": reason }));
        self.resync_at = Some(Instant::now());
        self.inject_upstream(encode_resync_frame(), InjectAt::AfterFirstFrame);
    }

    fn on_lane(&mut self, gen: u64, arrival: RenderArrival) {
        if !self.pumping() {
            return;
        }
        let now = Instant::now();
        let srtt = self.srtt();
        let Some(reorder) = self
            .leg
            .as_mut()
            .filter(|io| io.gen == gen)
            .and_then(|io| io.reorder.as_mut())
        else {
            return;
        };
        let (seq, kind, class, bytes) = match &arrival {
            RenderArrival::Header { seq, class } => (*seq, "header", Some(*class), 0),
            RenderArrival::Complete { seq, class, bytes } => {
                (*seq, "complete", Some(*class), bytes.len())
            }
            RenderArrival::Reset { seq } => (*seq, "reset", None, 0),
        };
        let next_before = reorder.next_seq();
        let out = reorder.on_arrival(arrival, now, srtt);
        if kind != "header" || class == Some(FrameClass::SelfContained) {
            self.trace.emit(
                "lane_in",
                serde_json::json!({
                    "seq": seq, "kind": kind, "class": class.map(|c| format!("{c:?}")),
                    "bytes": bytes, "next": next_before,
                }),
            );
        }
        self.apply_reorder(out);
    }

    fn poll_reorder(&mut self, now: Instant) {
        let srtt = self.srtt();
        let Some(reorder) = self.leg.as_mut().and_then(|io| io.reorder.as_mut()) else {
            return;
        };
        let out = reorder.poll(now, srtt);
        self.apply_reorder(out);
    }

    /// Frames from the lane go out between whole session-stream frames (never counted toward
    /// the ack point: they carry no stream offset).
    fn apply_reorder(&mut self, out: ReorderOutput) {
        if let Some(cause) = out.gap {
            let cause = match cause {
                GapCause::Reset => "reset",
                GapCause::Overdue => "overdue",
                GapCause::Supersede => "supersede",
            };
            let missing = self
                .leg
                .as_ref()
                .and_then(|io| io.reorder.as_ref())
                .map(|r| (r.next_seq(), r.header_seen(r.next_seq())));
            self.trace.emit(
                "render_gap",
                serde_json::json!({
                    "cause": cause,
                    "missing_seq": missing.map(|m| m.0),
                    "header_seen": missing.map(|m| m.1),
                }),
            );
        }
        if !self.client_closed {
            for frame in out.deliver {
                if FrameClass::of(&frame) == FrameClass::SelfContained {
                    let since = self.resync_at.take().map(|at| at.elapsed().as_millis() as u64);
                    self.trace.emit(
                        "surface_delivered",
                        serde_json::json!({ "bytes": frame.len(), "ms_since_resync": since }),
                    );
                }
                self.client_send(ClientOut::Injected(frame));
            }
        }
        if out.request_resync {
            self.request_resync("gap");
        }
        if self.stage == Stage::Linked && self.pumping() {
            self.pump_sends(Instant::now());
        }
    }

    /// How long the run loop may sleep before the lane's next overdue deadline.
    fn reorder_wait(&self, wait: Duration) -> Duration {
        let deadline = self
            .leg
            .as_ref()
            .and_then(|io| io.reorder.as_ref())
            .and_then(|reorder| reorder.next_deadline(self.srtt()));
        match deadline {
            Some(at) => wait.min(at.saturating_duration_since(Instant::now())),
            None => wait,
        }
    }

    fn inject(&mut self, status: LinkStatus) {
        let state = match status {
            LinkStatus::Resuming { .. } => "resuming",
            LinkStatus::Linked { .. } => "linked",
        };
        let srtt_ms = match status {
            LinkStatus::Linked { srtt_ms, .. } => srtt_ms,
            LinkStatus::Resuming { .. } => None,
        };
        self.trace
            .emit("inject", serde_json::json!({ "state": state, "srtt_ms": srtt_ms }));
        self.client_send(ClientOut::Injected(encode_link_frame(&status)));
    }

    // ---- pump and timers ------------------------------------------------------------------

    fn tick(&mut self) {
        let now = Instant::now();
        if self.stop.load(Ordering::Acquire) {
            self.cancel.store(true, Ordering::Release);
            if !matches!(&self.closing, Some(c) if matches!(c.why, CloseWhy::Stopped)) {
                self.on_stop(now);
            }
        } else if self.closing.is_none() && self.end_deadline.is_some_and(|by| now >= by) {
            self.end_session("end-linger-expired", None);
        }
        if self.closing.is_some() {
            self.tick_closing(now);
            return;
        }
        self.tick_outage(now);
        if self.closing.is_some() {
            return;
        }
        self.feed_client();
        self.poll_reorder(now);
        self.trace_stats(now);
        match self.stage {
            Stage::Dialing => {}
            Stage::Welcome => {
                if self.endpoint.poll_timers(now) == Some(LinkVerdict::PreWelcomeBound) {
                    self.pre_welcome_failed(true);
                }
            }
            Stage::Backoff(until) => {
                if now >= until {
                    let give_up_at = self.down.as_ref().map_or(now, |down| down.give_up_at);
                    self.start_dial(
                        LegKind::Reconnect,
                        Some(session::setup_deadline(now, give_up_at)),
                    );
                }
            }
            Stage::Linked => self.tick_linked(now),
        }
    }

    /// Let the client reader take its next chunk. While a leg is up the endpoint's window rule
    /// gates it (back-pressure on the client); while no leg is up the client is read until the
    /// ring is full, so a write past the cap deterministically ends the session.
    fn feed_client(&mut self) {
        if self.client_closed || !self.client_idle {
            return;
        }
        if !self.endpoint.may_read_source() && self.endpoint.is_up() {
            return;
        }
        // Down with a full ring: still read a whole chunk, so a client writing past the cap
        // overflows and ends the session (the ring-cap rule).
        let size = match self.endpoint.source_read_size() {
            0 => MAX_DATA_CHUNK,
            n => n,
        };
        if let Some(client) = &self.client {
            if client.go_tx.send(size).is_ok() {
                self.client_idle = false;
            }
        }
    }

    /// Once a second: the transport's counters, for diagnosing render recovery.
    fn trace_stats(&mut self, now: Instant) {
        if self.trace.file.is_none() || now.duration_since(self.last_stats) < Duration::from_secs(1)
        {
            return;
        }
        self.last_stats = now;
        if let Some(stats) = self.leg.as_ref().and_then(|io| io.leg.stats()) {
            self.trace.emit("quic_stats", stats);
        }
    }

    fn tick_outage(&mut self, now: Instant) {
        let Some((give_up_at, last_inject)) = self
            .down
            .as_ref()
            .map(|down| (down.give_up_at, down.last_inject))
        else {
            return;
        };
        if now >= give_up_at {
            self.end_session("give-up", None);
            return;
        }
        let due = last_inject.is_none_or(|at| now.duration_since(at) >= RESUMING_INJECT_EVERY);
        if due && self.splitter.frames_seen() >= 1 && !self.client_closed {
            self.inject_resuming(now);
        }
    }

    fn inject_resuming(&mut self, now: Instant) {
        let last_contact_ms = self
            .endpoint
            .last_inbound()
            .map_or(0, |at| now.saturating_duration_since(at).as_millis() as u64);
        self.inject(LinkStatus::Resuming { last_contact_ms });
        if let Some(down) = &mut self.down {
            down.last_inject = Some(now);
            down.resuming_injected = true;
        }
    }

    fn tick_linked(&mut self, now: Instant) {
        if self.endpoint.poll_timers(now) == Some(LinkVerdict::Silence) {
            self.leg_dead("silence");
            return;
        }
        self.update_delivered(now);
        self.pump_sends(now);
        // The client's handshake reads the first server frame as the endpoint welcome, so no
        // link frame may precede it. A leg with its own RTT probe (QUIC) knows its SRTT at
        // once, before the server has answered.
        if self.client.is_some() && !self.client_closed && self.frames_end > 0 {
            if self.echo_report_due {
                // The subscribe is on the stream: tell the client now, SRTT known or not.
                self.echo_report_due = false;
                let srtt = self.srtt();
                self.inject(LinkStatus::Linked {
                    srtt_ms: srtt.map(as_ms),
                    echo: true,
                });
                self.srtt_report.reported(srtt);
            } else if let Some(ms) = self.srtt_report.update(self.srtt()) {
                self.inject(LinkStatus::Linked {
                    srtt_ms: Some(ms),
                    echo: self.echo_injected,
                });
            }
        }
    }

    /// Current SRTT of this connection's link, or unknown: the leg's own RTT probe when it
    /// has one (QUIC), else the session's EWMA. The only SRTT source for bridge code.
    fn srtt(&self) -> Option<Duration> {
        self.leg
            .as_ref()
            .and_then(|io| io.leg.rtt())
            .or_else(|| self.endpoint.srtt())
    }

    /// The ack rule: held-back partial bytes count as delivered, queued bytes do not.
    fn update_delivered(&mut self, now: Instant) {
        let received = self.endpoint.recv_offset();
        let delivered = if self.client_closed {
            received
        } else if let Some(client) = &self.client {
            let written = client.written.load(Ordering::Acquire);
            if written >= self.frames_end {
                received
            } else {
                written
            }
        } else {
            return;
        };
        self.endpoint.delivered(now, delivered);
    }

    /// Hand every message the endpoint has due to the leg writer without blocking.
    fn pump_sends(&mut self, now: Instant) {
        loop {
            let Some(io) = &mut self.leg else { return };
            let message = match io.pending.take() {
                Some(message) => message,
                None => match self.endpoint.poll_send(now) {
                    Some(message) => message,
                    None => return,
                },
            };
            let is_end =
                self.closing.is_none() && self.client_closed && is_end_message(&message);
            match io.tx.try_send(message) {
                Ok(()) => {
                    io.sent += 1;
                    if is_end {
                        // The session-final END reached the leg writer: Closing waits for its
                        // write to complete and for leg EOF. A leg that dies first is not the
                        // close edge (it resumes and the END is re-sent). If EOF is observed
                        // just before the writer's `flushed` increment lands the bridge resumes
                        // needlessly and the keeper answers `unknown`; rare and benign.
                        let by = now + CLOSE_BUDGET;
                        self.closing = Some(Closing {
                            why: CloseWhy::EndSent,
                            leg_owed: true,
                            leg_target: Some(io.sent),
                            deadline: by,
                            drain_until: None,
                            client_deadline: Some(by),
                        });
                        return;
                    }
                }
                Err(TrySendError::Full(message)) => {
                    io.pending = Some(message);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    if self.closing.is_some() {
                        // Draining a server-eof: a dead leg is dropped, never resumed.
                        self.reap_leg();
                        self.endpoint.leg_down();
                    } else {
                        self.leg_dead("write-failed");
                    }
                    return;
                }
            }
        }
    }

    /// A linked leg died (EOF, protocol error, silence or write failure): start resuming.
    fn leg_dead(&mut self, reason: &str) {
        let now = Instant::now();
        self.reap_leg();
        self.endpoint.leg_down();
        self.trace
            .emit("leg_down", serde_json::json!({ "reason": reason }));
        if self.down.is_none() {
            self.down = Some(Down {
                give_up_at: now + self.params.grace,
                last_inject: None,
                resuming_injected: false,
            });
        }
        if self.splitter.frames_seen() >= 1 && !self.client_closed {
            self.inject_resuming(now);
        }
        self.schedule_backoff();
    }

    // ---- closing ----------------------------------------------------------------------------

    /// `should_stop` is set (or seen by a dial): switch to Stopped semantics. The client is shut
    /// at once (its writer unblocks on EPIPE; nothing ever waits on it) and, with a live leg,
    /// `END{client-gone}` goes to the leg writer independently of the client drain. A Closing
    /// already open keeps its leg target and its deadline when that is sooner.
    fn on_stop(&mut self, now: Instant) {
        let live = self.stage == Stage::Linked && self.endpoint.is_up() && self.leg.is_some();
        if let Some(client) = &self.client {
            shutdown_client(&client.shutdown);
        }
        self.client_shut = true;
        let mut closing = self.closing.take().unwrap_or(Closing {
            why: CloseWhy::Stopped,
            leg_owed: false,
            leg_target: None,
            deadline: now + CLOSE_BUDGET,
            drain_until: None,
            client_deadline: None,
        });
        closing.why = CloseWhy::Stopped;
        closing.drain_until = None;
        closing.client_deadline = Some(now);
        if closing.leg_target.is_none() && live {
            if let Some(io) = &mut self.leg {
                io.pending = Some(self.endpoint.end_now(end_reason::CLIENT_GONE));
            }
            closing.leg_owed = true;
            closing.deadline = closing.deadline.min(now + CLOSE_BUDGET);
        }
        self.closing = Some(closing);
    }

    /// `END{server-eof}` received on a linked leg (live handoff): keep the leg running and
    /// deliver the remaining complete frames, bounded by the remaining grace, not by the stop
    /// budget. Acks follow delivery (rule 3), so the teardown stays bounded.
    fn begin_server_eof(&mut self) {
        if self.closing.is_some() {
            return;
        }
        self.splitter.drop_partial();
        let now = Instant::now();
        let drain_until = self
            .down
            .as_ref()
            .map_or(now + self.params.grace, |down| down.give_up_at);
        self.closing = Some(Closing {
            why: CloseWhy::ServerEof,
            leg_owed: false,
            leg_target: None,
            deadline: drain_until + CLOSE_BUDGET,
            drain_until: Some(drain_until),
            client_deadline: None,
        });
    }

    /// Everything the client will ever get is written (or the client side is already closed).
    fn client_drained(&self) -> bool {
        self.client_closed
            || self.client_eof_seen
            || self.client.as_ref().is_none_or(|client| {
                client.writer.is_finished()
                    || client.written.load(Ordering::Acquire) >= self.frames_end
            })
    }

    /// Advance Closing. Each pass: a server-eof drain, the hand-off of the owed END/ACK, the
    /// leg's write completion and EOF, and the client writer's sub-wait; all bounded by the
    /// deadlines set when Closing was entered.
    fn tick_closing(&mut self, now: Instant) {
        // Phase A (server-eof only): deliver the client queue.
        if let Some(by) = self.closing.as_ref().and_then(|c| c.drain_until) {
            let dead = self
                .leg
                .as_ref()
                .is_some_and(LegIo::worker_exited)
                || self.endpoint.poll_timers(now) == Some(LinkVerdict::Silence);
            if dead && self.leg.is_some() {
                self.reap_leg();
                self.endpoint.leg_down();
            }
            self.update_delivered(now);
            self.pump_sends(now);
            if !(self.client_drained() || now >= by) {
                return;
            }
            // Ack only what the client wrote (`update_delivered` covers the dropped partial).
            self.update_delivered(now);
            let ack = if self.endpoint.is_up() {
                self.endpoint.ack_now()
            } else {
                None
            };
            let live = ack.is_some() && self.leg.is_some();
            if let (Some(ack), Some(io)) = (ack, &mut self.leg) {
                io.pending = Some(ack);
            }
            if let Some(closing) = &mut self.closing {
                closing.drain_until = None;
                closing.leg_owed = live;
                closing.deadline = now + CLOSE_BUDGET;
            }
        }
        // Phase B: hand the owed END/ACK to the leg writer (never blocking), then wait for its
        // write and the leg's EOF.
        let Some(closing) = &mut self.closing else {
            return;
        };
        if closing.leg_owed && closing.leg_target.is_none() {
            match &mut self.leg {
                Some(io) => {
                    if let Some(message) = io.pending.take() {
                        match io.tx.try_send(message) {
                            Ok(()) => {
                                io.sent += 1;
                                closing.leg_target = Some(io.sent);
                            }
                            Err(TrySendError::Full(message)) => io.pending = Some(message),
                            Err(TrySendError::Disconnected(_)) => closing.leg_owed = false,
                        }
                    } else {
                        closing.leg_owed = false;
                    }
                }
                None => closing.leg_owed = false,
            }
        }
        let mut leg_done = !closing.leg_owed || now >= closing.deadline;
        if !leg_done {
            match &self.leg {
                None => leg_done = true,
                Some(io) if io.worker_exited() => {
                    let reader_gone = io.reader_done.load(Ordering::Acquire);
                    let flushed = closing
                        .leg_target
                        .is_some_and(|target| io.flushed.load(Ordering::Acquire) >= target);
                    if !flushed && matches!(closing.why, CloseWhy::EndSent) {
                        // The leg died before the END write completed: not the close edge.
                        // Resume; the endpoint re-sends the retained END on the next leg.
                        self.closing = None;
                        self.leg_dead(if reader_gone {
                            "eof-before-end-flushed"
                        } else {
                            "write-failed"
                        });
                        return;
                    }
                    leg_done = true;
                }
                Some(_) => {}
            }
        }
        if !leg_done {
            return;
        }
        // Phase C: the client writer finishes behind its frames (ServerEof sends the close only
        // now, so the final ACK reaches the keeper before a `--remote` client can exit).
        let client_deadline = match closing.client_deadline {
            Some(by) => by,
            None => {
                let by = now + CLOSE_BUDGET;
                closing.client_deadline = Some(by);
                self.client_send(ClientOut::Close);
                by
            }
        };
        let client_done = self.client_shut
            || self.client.as_ref().is_none_or(|c| c.writer.is_finished())
            || now >= client_deadline;
        if client_done {
            self.complete_closing();
        }
    }

    fn complete_closing(&mut self) {
        let Some(closing) = self.closing.take() else {
            return;
        };
        self.trace.emit(
            "session_end",
            serde_json::json!({ "reason": closing.why.reason() }),
        );
        let error = match closing.why {
            CloseWhy::Ended { error, .. } => error,
            _ => None,
        };
        self.done = Some(Done::Closed {
            error,
            kill: closing.leg_target.is_none(),
            reap_by: closing.deadline,
        });
    }

    // ---- teardown -------------------------------------------------------------------------

    /// Release resources, each step bounded: the Closing deadlines were already enforced by the
    /// loop, so nothing here waits on the client or on a peer.
    fn finish(mut self) -> io::Result<()> {
        self.cancel.store(true, Ordering::Release);
        let (error, kill, reap_by) = match self.done.take() {
            Some(Done::Plain) => {
                self.kill_leg(true);
                self.join_dial();
                self.trace
                    .emit("session_end", serde_json::json!({"reason": "plain-fallback"}));
                return match self.stream.take() {
                    Some(stream) => self.params.plain.run(stream, self.stop),
                    None => Err(io::Error::other("client stream was already taken")),
                };
            }
            Some(Done::Closed {
                error,
                kill,
                reap_by,
            }) => (error, kill, reap_by),
            None => (None, true, Instant::now() + LEG_REAP_BUDGET),
        };
        // The client threads exit on the shutdown, so the joins are bounded.
        if let Some(client) = self.client.take() {
            if !self.client_shut {
                shutdown_client(&client.shutdown);
            }
            drop(client.go_tx);
            drop(client.out_tx);
            let _ = client.writer.join();
            let _ = client.reader.join();
        }
        self.stream = None;
        // Dropping the leg's sender gives ssh's stdin EOF; the child then gets until `reap_by`
        // to exit before it is killed and reaped.
        if let Some(LegIo { leg, tx, .. }) = self.leg.take() {
            drop(tx);
            leg.finish(kill, reap_by);
        }
        self.join_dial();
        match error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn join_dial(&mut self) {
        if let Some(handle) = self.dial_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Kill and reap a leg on a reaper thread, bounded by `LEG_REAP_BUDGET`.
fn reap_in_background(leg: Leg) {
    thread::spawn(move || {
        leg.finish(true, Instant::now() + LEG_REAP_BUDGET);
    });
}

fn is_end_message(message: &[u8]) -> bool {
    let mut decoder = Decoder::default();
    decoder.push(message);
    matches!(decoder.next(), Ok(Some(SessionMsg::End(_))))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::client::mosh_outage::LINK_KIND;
    use super::super::session::Hello;
    use crate::protocol::{read_message, write_message, ServerMessage, MAX_GRAPHICS_FRAME_SIZE};
    use std::collections::VecDeque;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::atomic::AtomicUsize;

    const WAIT: Duration = Duration::from_secs(10);

    fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + timeout;
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // ---- frames ---------------------------------------------------------------------------

    fn frame_with(kind: &str, payload: usize) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(
            &mut out,
            &ServerMessage::EndpointControl {
                kind: kind.to_owned(),
                data: "x".repeat(payload),
            },
        )
        .unwrap();
        out
    }

    fn frame(kind: &str) -> Vec<u8> {
        frame_with(kind, 0)
    }

    fn shutdown_frame() -> Vec<u8> {
        let mut out = Vec::new();
        write_message(
            &mut out,
            &ServerMessage::ServerShutdown {
                reason: Some("bye".to_owned()),
            },
        )
        .unwrap();
        out
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    enum Seen {
        Data(String),
        Link(LinkStatus),
    }

    fn seen(frame: &[u8]) -> Seen {
        let message: ServerMessage =
            read_message(&mut &frame[..], MAX_GRAPHICS_FRAME_SIZE).expect("whole frame");
        match message {
            ServerMessage::EndpointControl { kind, data } if kind == LINK_KIND => {
                Seen::Link(serde_json::from_str(&data).unwrap())
            }
            ServerMessage::EndpointControl { kind, .. } => Seen::Data(kind),
            ServerMessage::ServerShutdown { .. } => Seen::Data("shutdown".to_owned()),
            other => panic!("unexpected frame {other:?}"),
        }
    }

    fn data(kind: &str) -> Seen {
        Seen::Data(kind.to_owned())
    }

    fn is_resuming(seen: &Seen) -> bool {
        matches!(seen, Seen::Link(LinkStatus::Resuming { .. }))
    }

    // ---- the client ------------------------------------------------------------------------

    enum Got {
        Frame(Vec<u8>),
        Closed,
        Torn,
    }

    struct Client {
        stream: LocalStream,
        rx: mpsc::Receiver<(Instant, Got)>,
        /// Keeps a paused client's collector parked until dropped.
        _release: Option<mpsc::Sender<()>>,
    }

    impl Client {
        fn connect(path: &Path) -> Client {
            Client::connect_with(path, false, Duration::ZERO)
        }

        /// A client whose collector never reads (a stalled terminal) until the client is dropped.
        fn connect_paused(path: &Path) -> Client {
            Client::connect_with(path, true, Duration::ZERO)
        }

        /// A client whose collector sleeps `pause` after each frame.
        fn connect_slow(path: &Path, pause: Duration) -> Client {
            Client::connect_with(path, false, pause)
        }

        fn connect_with(path: &Path, paused: bool, pause: Duration) -> Client {
            let stream = ipc::connect_local_stream(path).unwrap();
            let mut reader = stream.try_clone().unwrap();
            let (tx, rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let release = paused.then_some(release_tx);
            thread::spawn(move || {
                if paused {
                    let _ = release_rx.recv();
                }
                loop {
                    let mut prefix = [0u8; 4];
                    let mut have = 0;
                    while have < 4 {
                        match reader.read(&mut prefix[have..]) {
                            Ok(0) | Err(_) => {
                                let got = if have == 0 { Got::Closed } else { Got::Torn };
                                let _ = tx.send((Instant::now(), got));
                                return;
                            }
                            Ok(n) => have += n,
                        }
                    }
                    let mut frame = prefix.to_vec();
                    frame.resize(4 + u32::from_le_bytes(prefix) as usize, 0);
                    let mut at = 4;
                    while at < frame.len() {
                        match reader.read(&mut frame[at..]) {
                            Ok(0) | Err(_) => {
                                let _ = tx.send((Instant::now(), Got::Torn));
                                return;
                            }
                            Ok(n) => at += n,
                        }
                    }
                    if tx.send((Instant::now(), Got::Frame(frame))).is_err() {
                        return;
                    }
                thread::sleep(pause);
                }
            });
            Client {
                stream,
                rx,
                _release: release,
            }
        }

        fn write(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).unwrap();
        }

        /// The client stops reading: the bridge's writer fails with EPIPE while its reader sees
        /// no EOF.
        fn stop_reading(&self) {
            let LocalStream::UdSocket(stream) = &self.stream;
            let _ = stream.inner().shutdown(std::net::Shutdown::Read);
        }

        /// EOF toward the bridge (the collector thread holds a dup of the descriptor).
        fn close(self) {
            shutdown_client(&self.stream);
        }

        fn recv(&self, timeout: Duration) -> Option<(Instant, Got)> {
            self.rx.recv_timeout(timeout).ok()
        }

        /// The next whole frame, decoded; panics on close, tear or timeout.
        fn next_seen(&self, timeout: Duration) -> (Instant, Seen) {
            match self.recv(timeout) {
                Some((at, Got::Frame(frame))) => (at, seen(&frame)),
                Some((_, Got::Closed)) => panic!("client socket closed"),
                Some((_, Got::Torn)) => panic!("client socket closed mid-frame"),
                None => panic!("timed out waiting for a frame"),
            }
        }

        /// Wait for a data frame of `kind`, ignoring link frames; returns what was skipped.
        fn expect_data(&self, kind: &str) -> Vec<(Instant, Seen)> {
            let mut skipped = Vec::new();
            loop {
                let (at, got) = self.next_seen(WAIT);
                if got == data(kind) {
                    skipped.push((at, got));
                    return skipped;
                }
                skipped.push((at, got));
            }
        }

        /// Every frame up to the close, which must be at a frame boundary.
        fn until_closed(&self, timeout: Duration) -> Vec<(Instant, Seen)> {
            let deadline = Instant::now() + timeout;
            let mut frames = Vec::new();
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.recv(left) {
                    Some((at, Got::Frame(frame))) => frames.push((at, seen(&frame))),
                    Some((_, Got::Closed)) => return frames,
                    Some((_, Got::Torn)) => panic!("client closed mid-frame after {frames:?}"),
                    None => panic!("client socket still open after {timeout:?}; got {frames:?}"),
                }
            }
        }
    }

    // ---- the fake keeper -------------------------------------------------------------------

    #[derive(Clone, Copy)]
    enum Answer {
        New,
        Resumed,
        Unknown,
        End(&'static str),
        Silent,
    }

    #[derive(Clone, Copy)]
    struct KLeg {
        answer: Answer,
        welcome_delay: Duration,
        /// Write this many bytes on the leg, then cut it.
        kill_after: Option<usize>,
        /// The leg's writer fails the session-final END write and cuts the leg, so the END never
        /// counts as flushed: the leg always dies first.
        cut_on_end_write: bool,
        /// The leg's writer fails the session-final END write but leaves the socket open, so the
        /// leg reader never sees EOF.
        fail_end_write: bool,
    }

    impl KLeg {
        fn new(answer: Answer) -> KLeg {
            KLeg {
                answer,
                welcome_delay: Duration::ZERO,
                kill_after: None,
                cut_on_end_write: false,
                fail_end_write: false,
            }
        }
        fn delay(mut self, delay: Duration) -> KLeg {
            self.welcome_delay = delay;
            self
        }
        fn cut_on_end_write(mut self) -> KLeg {
            self.cut_on_end_write = true;
            self
        }
        fn fail_end_write(mut self) -> KLeg {
            self.fail_end_write = true;
            self
        }
        fn kill_after(mut self, bytes: usize) -> KLeg {
            self.kill_after = Some(bytes);
            self
        }
    }

    #[derive(Default)]
    struct KState {
        endpoint: Option<SessionEndpoint>,
        pending_down: Vec<Vec<u8>>,
        final_end: Option<&'static str>,
        upstream: Vec<u8>,
        upstream_at_end: Option<usize>,
        /// Messages the bridge sent after HELLO but before WELCOME was written.
        pre_welcome_msgs: usize,
        hellos: Vec<Hello>,
        ends: Vec<String>,
        /// When each entry of `ends` was received.
        ends_at: Vec<Instant>,
        /// Bytes written down the leg.
        down_sent: usize,
        /// The queued final END was written down the leg.
        end_written: bool,
        /// After its END the keeper saw everything acked and hung up.
        acked_all: bool,
        last_rx: Option<Instant>,
        stall: bool,
        kill_now: bool,
        /// Append a gap DATA to the next write that carries a DATA message.
        corrupt_next: bool,
    }

    #[derive(Clone, Default)]
    struct FakeKeeper(Arc<Mutex<KState>>);

    impl FakeKeeper {
        fn st(&self) -> std::sync::MutexGuard<'_, KState> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
        fn push_down(&self, bytes: Vec<u8>) {
            self.st().pending_down.push(bytes);
        }
        fn queue_end(&self, reason: &'static str) {
            self.st().final_end = Some(reason);
        }
        fn upstream(&self) -> Vec<u8> {
            self.st().upstream.clone()
        }
        fn pre_welcome_msgs(&self) -> usize {
            self.st().pre_welcome_msgs
        }
        fn hellos(&self) -> Vec<Hello> {
            self.st().hellos.clone()
        }
        fn ends(&self) -> Vec<String> {
            self.st().ends.clone()
        }
        fn set_stall(&self, stall: bool) {
            self.st().stall = stall;
        }
        fn kill_leg(&self) {
            self.st().kill_now = true;
        }
        fn corrupt_next_write(&self) {
            self.st().corrupt_next = true;
        }
        fn last_rx(&self) -> Option<Instant> {
            self.st().last_rx
        }

        fn spawn_leg(&self, mut stream: UnixStream, script: KLeg) {
            let keeper = self.clone();
            thread::spawn(move || keeper.run_leg(&mut stream, script));
        }

        fn run_leg(&self, s: &mut UnixStream, script: KLeg) {
            s.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let mut decoder = Decoder::default();
            let mut raw = Vec::new();
            let hello = loop {
                match s.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        decoder.push(&buf[..n]);
                        if let Ok(Some(SessionMsg::Hello(hello))) = decoder.next() {
                            break hello;
                        }
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return,
                }
            };
            self.st().hellos.push(hello.clone());
            let now = Instant::now();
            let scratch = || SessionEndpoint::new(Role::Keeper, hello.token, 1 << 20);
            let status = match script.answer {
                Answer::Silent => {
                    while !matches!(s.read(&mut buf), Ok(0)) {
                        thread::sleep(Duration::from_millis(5));
                    }
                    return;
                }
                Answer::End(reason) => {
                    thread::sleep(script.welcome_delay);
                    let _ = s.write_all(&scratch().refuse(reason));
                    return;
                }
                Answer::Unknown => {
                    let bytes = scratch().welcome(now, WelcomeStatus::Unknown, 0).unwrap();
                    let _ = s.write_all(&bytes);
                    return;
                }
                Answer::New => WelcomeStatus::New,
                Answer::Resumed => WelcomeStatus::Resumed,
            };
            // Keep reading while the WELCOME is delayed: a client byte sent
            // before WELCOME must be observable, not left in the socket buffer.
            let hello_len = 5 + u32::from_le_bytes(raw[1..5].try_into().unwrap()) as usize;
            let mut early = raw.split_off(hello_len);
            let delay_end = Instant::now() + script.welcome_delay;
            while Instant::now() < delay_end {
                match s.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => early.extend_from_slice(&buf[..n]),
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return,
                }
            }
            let mut early_decoder = Decoder::default();
            early_decoder.push(&early);
            let mut early_msgs = 0;
            while let Ok(Some(_)) = early_decoder.next() {
                early_msgs += 1;
            }
            self.st().pre_welcome_msgs += early_msgs;
            let welcome = {
                let mut st = self.st();
                let ep = st.endpoint.get_or_insert_with(|| {
                    SessionEndpoint::new(Role::Keeper, hello.token, LOCAL_RING_CAP)
                });
                ep.welcome(Instant::now(), status, hello.recv_offset).unwrap()
            };
            if s.write_all(&welcome).is_err() {
                return;
            }
            let mut budget = script.kill_after;
            'pump: loop {
                let (stalled, kill_now) = {
                    let mut st = self.st();
                    let kill_now = std::mem::take(&mut st.kill_now);
                    (st.stall, kill_now)
                };
                if kill_now {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                    break;
                }
                if stalled {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                // Bytes read before WELCOME are fed once, as if they had just arrived.
                let carried = std::mem::take(&mut early);
                let read = if carried.is_empty() {
                    s.read(&mut buf)
                } else {
                    buf[..carried.len()].copy_from_slice(&carried);
                    Ok(carried.len())
                };
                let mut eof = false;
                let mut out = Vec::new();
                {
                    let now = Instant::now();
                    let mut st = self.st();
                    match read {
                        Ok(0) => eof = true,
                        Ok(n) => {
                            st.last_rx = Some(now);
                            let events = st
                                .endpoint
                                .as_mut()
                                .unwrap()
                                .on_leg_bytes(now, &buf[..n])
                                .expect("local sent a valid message");
                            for event in events {
                                match event {
                                    SessionEvent::Deliver(bytes) => st.upstream.extend(bytes),
                                    SessionEvent::PeerEnd(reason) => {
                                        let len = st.upstream.len();
                                        st.upstream_at_end = Some(len);
                                        st.ends.push(reason);
                                        st.ends_at.push(now);
                                        eof = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) => {}
                        Err(_) => eof = true,
                    }
                    let pending = std::mem::take(&mut st.pending_down);
                    let final_end = st.final_end.take();
                    let end_written = st.end_written;
                    let ep = st.endpoint.as_mut().unwrap();
                    for bytes in pending {
                        ep.push_source(&bytes).expect("keeper ring");
                    }
                    if let Some(reason) = final_end {
                        ep.queue_final_end(reason);
                    }
                    ep.delivered(now, ep.recv_offset());
                    let hang_up = end_written && ep.fully_acked();
                    while let Some(message) = ep.poll_send(now) {
                        out.push(message);
                    }
                    if hang_up {
                        // Like the real keeper: a fully acked eof session is removed.
                        st.acked_all = true;
                        eof = true;
                    }
                    let has_data = out.iter().any(|message| {
                        let mut probe = Decoder::default();
                        probe.push(message);
                        matches!(probe.next(), Ok(Some(SessionMsg::Data { .. })))
                    });
                    if has_data && std::mem::take(&mut st.corrupt_next) {
                        // One write: the valid DATA, then a DATA far past the stream end
                        // that is not in the ring and so is never replayed.
                        let gap = st.endpoint.as_ref().unwrap().outbound_end() + 1000;
                        out.push(SessionMsg::Data { offset: gap, bytes: b"x".to_vec() }.encode());
                        out = vec![out.concat()];
                    }
                }
                if !eof {
                    for message in out {
                        let take = budget.map_or(message.len(), |left| left.min(message.len()));
                        if s.write_all(&message[..take]).is_err() {
                            break 'pump;
                        }
                        {
                            let mut st = self.st();
                            st.down_sent += take;
                            st.end_written |= is_end_message(&message);
                        }
                        if let Some(left) = &mut budget {
                            *left -= take;
                            if *left == 0 {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                                break 'pump;
                            }
                        }
                    }
                }
                if eof {
                    break;
                }
            }
            if let Some(ep) = self.st().endpoint.as_mut() {
                ep.leg_down();
            }
        }
    }

    /// A leg writer that fails the session-final END write, and cuts the leg when `cut` is set:
    /// the END never counts as flushed, so the bridge cannot close on a flushed END.
    struct CutOnEnd {
        inner: UnixStream,
        cut: bool,
    }

    impl Write for CutOnEnd {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if is_end_message(buf) {
                if self.cut {
                    let _ = self.inner.shutdown(std::net::Shutdown::Both);
                }
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.inner.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    // ---- the in-memory dialer ----------------------------------------------------------------

    enum LegScript {
        Keeper(FakeKeeper, KLeg),
        Fail(LegError),
        Exit { code: i32, stderr: &'static str },
        /// A keeper leg whose bytes toward the bridge never arrive: the keeper reads HELLO and
        /// answers, but the bridge's leg reads EOF (WELCOME lost with the leg).
        LostWelcome(FakeKeeper, KLeg),
        /// The inner script's leg, passed through a decorator (token, silence, standby).
        Decorated(Box<LegScript>, Box<dyn FnOnce(Leg) -> Leg + Send>),
    }

    fn decorated(script: LegScript, f: impl FnOnce(Leg) -> Leg + Send + 'static) -> LegScript {
        LegScript::Decorated(Box::new(script), Box::new(f))
    }

    /// Reads end of file, but only once the keeper has admitted the session, so the WELCOME is
    /// lost deterministically after the keeper acted on HELLO.
    struct LostWelcomeReader(FakeKeeper);

    impl Read for LostWelcomeReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            let deadline = Instant::now() + WAIT;
            while self.0.st().endpoint.is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(0)
        }
    }

    fn build_leg(script: LegScript) -> Result<Leg, LegError> {
        match script {
            LegScript::Fail(err) => Err(err),
            LegScript::Exit { code, stderr } => Ok(Leg::new(
                Box::new(io::empty()),
                Box::new(io::sink()),
                Box::new(move |_, _| {
                    (Some(ExitStatus::from_raw(code << 8)), Some(stderr.to_owned()))
                }),
            )),
            LegScript::Decorated(inner, f) => build_leg(*inner).map(f),
            LegScript::LostWelcome(keeper, script) => {
                let (local, remote) = UnixStream::pair().unwrap();
                keeper.spawn_leg(remote, script);
                let writer = local.try_clone().unwrap();
                Ok(Leg::new(
                    Box::new(LostWelcomeReader(keeper)),
                    Box::new(writer),
                    Box::new(move |_, _| {
                        let _ = local.shutdown(std::net::Shutdown::Both);
                        (None, None)
                    }),
                ))
            }
            LegScript::Keeper(keeper, script) => {
                let (local, remote) = UnixStream::pair().unwrap();
                keeper.spawn_leg(remote, script);
                let reader = local.try_clone().unwrap();
                let writer: Box<dyn Write + Send> = if script.cut_on_end_write || script.fail_end_write {
                    Box::new(CutOnEnd {
                        inner: local.try_clone().unwrap(),
                        cut: script.cut_on_end_write,
                    })
                } else {
                    Box::new(local.try_clone().unwrap())
                };
                Ok(Leg::new(
                    Box::new(reader),
                    writer,
                    Box::new(move |_, _| {
                        let _ = local.shutdown(std::net::Shutdown::Both);
                        (None, None)
                    }),
                ))
            }
        }
    }

    #[derive(Clone, Default)]
    struct MemDialer {
        scripts: Arc<Mutex<VecDeque<LegScript>>>,
        /// While set, every dial fails `Retryable` without consuming a script.
        blocked: Arc<AtomicBool>,
        dials: Arc<Mutex<Vec<(LegKind, bool)>>>,
        /// While set, every dial records its time and fails as a transient spawn error.
        spawn_fail: Arc<AtomicBool>,
        dial_times: Arc<Mutex<Vec<(LegKind, Instant)>>>,
        /// While set, `backoff` is the real reconnect schedule instead of 10 ms.
        real_backoff: Arc<AtomicBool>,
        /// While set (with `blocked`), a dial stays in flight until it is cleared.
        hold: Arc<AtomicBool>,
    }

    impl MemDialer {
        fn new() -> MemDialer {
            MemDialer::default()
        }
        fn script(&self, script: LegScript) -> &MemDialer {
            self.scripts.lock().unwrap().push_back(script);
            self
        }
        fn keeper(&self, keeper: &FakeKeeper, leg: KLeg) -> &MemDialer {
            self.script(LegScript::Keeper(keeper.clone(), leg))
        }
        fn block(&self, blocked: bool) {
            self.blocked.store(blocked, Ordering::SeqCst);
        }
        fn dial_count(&self) -> usize {
            self.dials.lock().unwrap().len()
        }
    }

    impl LegDialer for MemDialer {
        fn dial(
            &mut self,
            kind: LegKind,
            setup_deadline: Option<Instant>,
            _stop: &AtomicBool,
        ) -> Result<Leg, LegError> {
            if self.blocked.load(Ordering::SeqCst) {
                while self.hold.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                thread::sleep(Duration::from_millis(5));
                return Err(LegError::Retryable(io::Error::other("dialer blocked")));
            }
            if self.spawn_fail.load(Ordering::SeqCst) {
                self.dial_times.lock().unwrap().push((kind, Instant::now()));
                return Err(super::super::ssh_leg::classify_spawn_error(
                    kind,
                    io::Error::from_raw_os_error(libc::EAGAIN),
                ));
            }
            let script = self.scripts.lock().unwrap().pop_front();
            self.dials.lock().unwrap().push((kind, setup_deadline.is_some()));
            match script {
                None => Err(LegError::Retryable(io::Error::other("no scripted leg"))),
                Some(script) => build_leg(script),
            }
        }

        fn classify(&self, kind: LegKind, end: &LegEndReport) -> LegError {
            super::super::ssh_leg::classify_leg(kind, end)
        }

        fn backoff(&self, attempt: u32) -> Duration {
            if self.real_backoff.load(Ordering::SeqCst) {
                return super::super::ssh_leg::reconnect_backoff(attempt);
            }
            Duration::from_millis(10)
        }
    }

    // ---- the bridge harness ------------------------------------------------------------------

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Harness {
        bridge: Option<SshStdioBridge>,
        path: PathBuf,
        dir: PathBuf,
        stop: Arc<AtomicBool>,
    }

    fn unused_plain() -> PlainFallback {
        PlainFallback::Test(Box::new(|_| {
            Err(io::Error::other("plain fallback must not be used"))
        }))
    }

    fn fresh_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mlb-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    impl Harness {
        fn start(dialer: &MemDialer, grace: Duration) -> Harness {
            Harness::start_with(dialer, grace, unused_plain(), None, None)
        }

        /// A bridge for a remote that advertised `x-mosh-supersede-v1`.
        fn start_split(dialer: &MemDialer, grace: Duration) -> Harness {
            Harness::start_full(dialer, grace, unused_plain(), None, None, true)
        }

        fn start_with(
            dialer: &MemDialer,
            grace: Duration,
            plain: PlainFallback,
            trace_dir: Option<PathBuf>,
            path: Option<(PathBuf, PathBuf)>,
        ) -> Harness {
            Harness::start_full(dialer, grace, plain, trace_dir, path, false)
        }

        fn start_full(
            dialer: &MemDialer,
            grace: Duration,
            plain: PlainFallback,
            trace_dir: Option<PathBuf>,
            path: Option<(PathBuf, PathBuf)>,
            render_split: bool,
        ) -> Harness {
            Harness::start_params(dialer, grace, plain, trace_dir, path, render_split, false)
        }

        fn start_echo(dialer: &MemDialer, grace: Duration, echo: bool) -> Harness {
            Harness::start_params(dialer, grace, unused_plain(), None, None, false, echo)
        }

        fn start_params(
            dialer: &MemDialer,
            grace: Duration,
            plain: PlainFallback,
            trace_dir: Option<PathBuf>,
            path: Option<(PathBuf, PathBuf)>,
            render_split: bool,
            echo: bool,
        ) -> Harness {
            let (dir, path) = path.unwrap_or_else(|| {
                let dir = fresh_dir();
                let path = dir.join("s");
                (dir, path)
            });
            let stop = Arc::new(AtomicBool::new(false));
            let bridge = start(
                BridgeParams {
                    local_socket: path.clone(),
                    grace,
                    noninteractive: true,
                    plain,
                    trace_dir,
                    render_split,
                    echo,
                },
                Box::new(dialer.clone()),
                Arc::clone(&stop),
            )
            .unwrap();
            Harness {
                bridge: Some(bridge),
                path,
                dir,
                stop,
            }
        }

        fn connect(&self) -> Client {
            Client::connect(&self.path)
        }

        fn bridge(&self) -> &SshStdioBridge {
            self.bridge.as_ref().unwrap()
        }

        /// Drop the bridge (which stops and joins it) and return how long that took.
        fn shutdown(&mut self) -> Duration {
            let started = Instant::now();
            self.stop.store(true, Ordering::SeqCst);
            self.bridge = None;
            started.elapsed()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.bridge = None;
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A harness with one established session: a keeper whose leg 0 answered `new`, and a client
    /// that has received `first`.
    fn established(
        dialer: &MemDialer,
        keeper: &FakeKeeper,
        grace: Duration,
        leg: KLeg,
    ) -> (Harness, Client) {
        dialer.keeper(keeper, leg);
        keeper.push_down(frame("first"));
        let harness = Harness::start(dialer, grace);
        let client = harness.connect();
        client.expect_data("first");
        (harness, client)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    // ---- tests -------------------------------------------------------------------------------

    // ---- SRTT reporting ------------------------------------------------------------------------

    fn dur(n: u64) -> Option<Duration> {
        Some(Duration::from_millis(n))
    }

    #[test]
    fn srtt_reporter_first_sample_injects() {
        let mut r = SrttReporter::default();
        assert_eq!(r.update(dur(80)), Some(80));
        assert_eq!(r.update(dur(80)), None);
    }

    #[test]
    fn srtt_reporter_ignores_small_changes() {
        let mut r = SrttReporter::default();
        assert_eq!(r.update(dur(100)), Some(100));
        assert_eq!(r.update(dur(115)), None);
        assert_eq!(r.update(dur(85)), None);
        assert_eq!(r.update(dur(121)), Some(121));
        // Measured against the last reported value, not the last seen one.
        assert_eq!(r.update(dur(130)), None);
        // More than 20 % of a small value, though under 20 ms.
        let mut r = SrttReporter::default();
        assert_eq!(r.update(dur(40)), Some(40));
        assert_eq!(r.update(dur(48)), None, "exactly 20 % is not more than 20 %");
        assert_eq!(r.update(dur(49)), Some(49));
    }

    #[test]
    fn srtt_reporter_ms_threshold() {
        let mut r = SrttReporter::default();
        assert_eq!(r.update(dur(400)), Some(400));
        assert_eq!(r.update(dur(419)), None);
        assert_eq!(r.update(dur(421)), Some(421));
        assert_eq!(r.update(dur(400)), Some(400));
        assert_eq!(r.update(dur(380)), None, "exactly 20 ms is not more than 20 ms");
    }

    #[test]
    fn srtt_reporter_unknown_never_injects() {
        let mut r = SrttReporter::default();
        assert_eq!(r.update(None), None);
        assert_eq!(r.update(dur(50)), Some(50));
        assert_eq!(r.update(None), None);
    }

    #[test]
    fn srtt_reporter_resets_after_linked() {
        let mut r = SrttReporter::default();
        assert_eq!(r.update(dur(100)), Some(100));
        r.reported(None);
        assert_eq!(r.update(dur(100)), Some(100));
        r.reported(dur(300));
        assert_eq!(r.update(dur(305)), None);
    }

    /// Heartbeat/ACK samples on a synthetic clock reach the reporter through `srtt()`.
    #[test]
    fn heartbeat_samples_drive_the_reporter_on_a_fake_clock() {
        let token: Token = "000102030405060708090a0b0c0d0e0f".parse().unwrap();
        let mut local = SessionEndpoint::new(Role::Local, token, 1 << 20);
        let mut keeper = SessionEndpoint::new(Role::Keeper, token, 1 << 20);
        let t0 = Instant::now();
        let hello = local.hello(t0, 60_000, None);
        let events = keeper.on_leg_bytes(t0, &hello).unwrap();
        let [SessionEvent::Hello(h)] = &events[..] else { panic!("{events:?}") };
        let welcome = keeper.welcome(t0, WelcomeStatus::New, h.recv_offset).unwrap();
        keeper.on_leg_bytes(t0, &[]).unwrap();
        local.on_leg_bytes(t0, &welcome).unwrap();
        let mut reporter = SrttReporter::default();
        assert_eq!(reporter.update(local.srtt()), None);
        let mut round = |at: Instant, rtt: Duration| {
            assert!(local.poll_send(at).is_some_and(|m| m == SessionMsg::Heartbeat.encode()));
            keeper.on_leg_bytes(at + rtt / 2, &SessionMsg::Heartbeat.encode()).unwrap();
            let ack = keeper.poll_send(at + rtt / 2).unwrap();
            local.on_leg_bytes(at + rtt, &ack).unwrap();
            reporter.update(local.srtt())
        };
        let mut at = t0 + secs(2);
        assert_eq!(round(at, Duration::from_millis(100)), Some(100));
        at += secs(2);
        assert_eq!(round(at, Duration::from_millis(110)), None, "srtt 101 ms");
        // A sustained jump to 400 ms moves the EWMA past the threshold within a few samples.
        let mut injected = Vec::new();
        for _ in 0..12 {
            at += secs(2);
            injected.extend(round(at, Duration::from_millis(400)));
        }
        assert!(injected.len() >= 2 && injected.windows(2).all(|w| w[0] < w[1]), "{injected:?}");
    }

    #[test]
    fn rtt_probe_srtt_is_not_injected_before_the_first_server_frame() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.script(decorated(LegScript::Keeper(keeper.clone(), KLeg::new(Answer::New)), |leg| {
            leg.with_rtt_probe(Box::new(|| Some(Duration::from_millis(5))))
        }));
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        wait_until("hello", WAIT, || keeper.hellos().len() == 1);
        // Several bridge ticks pass with the leg linked and its SRTT known, but no server frame.
        thread::sleep(Duration::from_millis(300));
        keeper.push_down(frame("welcome"));
        let (_, first) = client.next_seen(WAIT);
        assert_eq!(first, data("welcome"), "a link frame preceded the first server frame");
        // Afterwards the probe's SRTT is reported as usual.
        loop {
            let (_, got) = client.next_seen(WAIT);
            if matches!(got, Seen::Link(LinkStatus::Linked { srtt_ms: Some(5), .. })) {
                break;
            }
        }
    }

    #[test]
    fn linked_session_reports_srtt_once_heartbeats_are_answered() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        // The bridge's first heartbeat goes out 2 s after the leg comes up; the fake keeper's
        // session endpoint answers it with an ACK.
        loop {
            let (_, got) = client.next_seen(WAIT);
            if matches!(got, Seen::Link(LinkStatus::Linked { srtt_ms: Some(_), .. })) {
                break;
            }
            assert!(
                !matches!(got, Seen::Link(LinkStatus::Resuming { .. })),
                "unexpected outage: {got:?}"
            );
        }
    }

    #[test]
    fn leg_killed_mid_frame_client_sees_whole_frames() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        // The first DATA message carries `first` plus the head of `big`, then the leg is cut.
        let first_data = 5 + 8 + MAX_DATA_CHUNK;
        keeper.push_down([frame("first"), frame_with("big", 150_000)].concat());
        dialer.keeper(&keeper, KLeg::new(Answer::New).kill_after(first_data));
        dialer.block(false);
        let harness = Harness::start(&dialer, secs(30));
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        let client = harness.connect();
        let seen = client.expect_data("big");
        let data_kinds: Vec<_> = seen.iter().map(|(_, s)| s.clone()).collect();
        assert_eq!(data_kinds[0], data("first"));
        assert!(
            data_kinds.iter().any(is_resuming),
            "an outage with a frame seen injects Resuming: {data_kinds:?}"
        );
        // `until_closed` panics on a torn frame: stop the bridge and read the rest.
        drop(harness);
        client.until_closed(secs(5));
    }

    #[test]
    fn first_resuming_at_dead_verdict_then_every_second_then_linked() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let trace = fresh_dir();
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        keeper.push_down(frame("first"));
        let harness = Harness::start_with(&dialer, secs(30), unused_plain(), Some(trace.clone()), None);
        let client = harness.connect();
        client.expect_data("first");
        dialer.block(true);
        let killed = Instant::now();
        keeper.kill_leg();
        let mut resuming = Vec::new();
        while resuming.len() < 4 {
            let (at, got) = client.next_seen(WAIT);
            assert!(is_resuming(&got), "only Resuming during the outage: {got:?}");
            resuming.push(at.duration_since(killed));
        }
        assert!(resuming[0] < Duration::from_millis(600), "first at the dead verdict: {resuming:?}");
        for pair in resuming.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(
                gap > Duration::from_millis(700) && gap < Duration::from_millis(1300),
                "about 1 s apart: {resuming:?}"
            );
        }
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        dialer.block(false);
        loop {
            let (_, got) = client.next_seen(WAIT);
            if matches!(got, Seen::Link(LinkStatus::Linked { .. })) {
                break;
            }
            assert!(is_resuming(&got), "{got:?}");
        }
        keeper.push_down(frame("after"));
        let after = client.expect_data("after");
        assert!(
            !after.iter().any(|(_, s)| is_resuming(s)),
            "no Resuming once linked: {after:?}"
        );
        drop(harness);
        let lines = std::fs::read_to_string(trace.join("local.jsonl")).unwrap();
        for event in ["leg_up", "leg_down", "welcome", "resume", "inject", "session_end"] {
            assert!(
                lines.contains(&format!("\"event\":\"{event}\"")),
                "trace lacks {event}: {lines}"
            );
        }
        let _ = std::fs::remove_dir_all(trace);
    }

    #[test]
    fn last_contact_ms_from_last_inbound() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.block(true);
        thread::sleep(Duration::from_millis(1000));
        keeper.kill_leg();
        let (_, got) = client.next_seen(WAIT);
        let Seen::Link(LinkStatus::Resuming { last_contact_ms }) = got else {
            panic!("expected Resuming, got {got:?}");
        };
        assert!(
            (700..=1700).contains(&last_contact_ms),
            "time since the keeper's last byte: {last_contact_ms}"
        );
    }

    #[test]
    fn no_injection_before_first_frame() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        dialer.block(false);
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        wait_until("leg 0", WAIT, || !keeper.hellos().is_empty());
        dialer.block(true);
        keeper.kill_leg();
        assert!(
            client.recv(Duration::from_millis(1800)).is_none(),
            "nothing is injected before the first complete frame"
        );
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.push_down(frame("x"));
        dialer.block(false);
        let (_, got) = client.next_seen(WAIT);
        assert_eq!(got, data("x"), "no Linked either");
    }

    #[test]
    fn give_up_closes_client_at_frame_boundary() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(1), KLeg::new(Answer::New));
        dialer.block(true);
        keeper.kill_leg();
        let started = Instant::now();
        let frames = client.until_closed(secs(5));
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(900), "closed too early: {elapsed:?}");
        assert!(frames.iter().all(|(_, s)| is_resuming(s)), "{frames:?}");
        assert!(!frames.is_empty() && frames.len() <= 2, "{frames:?}");
        assert!(
            frames.iter().all(|(at, _)| at.duration_since(started) < Duration::from_millis(1100)),
            "no injection after the deadline: {frames:?}"
        );
    }

    #[test]
    fn give_up_with_dial_in_flight_stays_graceful() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let trace = fresh_dir();
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        for i in 0..48 {
            keeper.push_down(frame_with(&format!("f{i}"), 60_000));
        }
        let harness = Harness::start_with(&dialer, secs(1), unused_plain(), Some(trace.clone()), None);
        // A stalled client keeps the graceful close pending behind its queued frames.
        let _client = Client::connect_paused(&harness.path);
        wait_until("leg 0", WAIT, || !keeper.hellos().is_empty());
        // The reconnect dial is held in flight across the give-up.
        dialer.hold.store(true, Ordering::SeqCst);
        dialer.block(true);
        keeper.kill_leg();
        thread::sleep(Duration::from_millis(1200));
        // The dial completes while the connection is Closing for give-up, not stopping.
        dialer.hold.store(false, Ordering::SeqCst);
        let log = trace.join("local.jsonl");
        wait_until("session_end", WAIT, || {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains("\"event\":\"session_end\""))
        });
        let lines = std::fs::read_to_string(&log).unwrap();
        assert!(
            lines.contains("\"reason\":\"give-up\""),
            "session_end keeps the give-up reason: {lines}"
        );
        drop(harness);
        let _ = std::fs::remove_dir_all(trace);
    }

    #[test]
    fn transient_spawn_failures_back_off_until_give_up() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(4), KLeg::new(Answer::New));
        dialer.spawn_fail.store(true, Ordering::SeqCst);
        dialer.real_backoff.store(true, Ordering::SeqCst);
        keeper.kill_leg();
        let lost = client.next_seen(WAIT).0; // the first Resuming frame marks the link loss
        client.until_closed(secs(10));
        let closed = lost.elapsed();
        assert!(
            closed >= Duration::from_millis(3800) && closed <= Duration::from_millis(5500),
            "closed {closed:?} after link loss"
        );
        let times: Vec<Instant> = dialer.dial_times.lock().unwrap().iter().map(|(_, at)| *at).collect();
        assert_eq!(times.len(), 2, "dials at +1 s and +3 s only: {times:?}");
        let first = times[0].duration_since(lost);
        let second = times[1].duration_since(lost);
        assert!(first >= Duration::from_millis(900), "first retry after {first:?}");
        assert!(second >= Duration::from_millis(2900), "second retry after {second:?}");
        assert!(times[1].duration_since(times[0]) >= Duration::from_millis(1900));
        thread::sleep(secs(1));
        assert_eq!(dialer.dial_times.lock().unwrap().len(), 2, "no dial after the give-up: no hot loop");
    }

    #[test]
    fn client_writes_accepted_during_outage_and_delivered_once() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        client.write(b"before|");
        wait_until("pre-outage input", WAIT, || keeper.upstream() == b"before|");
        dialer.block(true);
        keeper.kill_leg();
        client.next_seen(WAIT); // Resuming: the outage is known to the bridge
        for part in [&b"one|"[..], b"two|", b"three|"] {
            client.write(part);
            thread::sleep(Duration::from_millis(100));
        }
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        dialer.block(false);
        wait_until("replayed input", WAIT, || {
            keeper.upstream() == b"before|one|two|three|"
        });
        thread::sleep(Duration::from_millis(500));
        assert_eq!(keeper.upstream(), b"before|one|two|three|", "each byte exactly once");
    }

    #[test]
    fn outage_overflow_ends_session() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let trace = fresh_dir();
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        keeper.push_down(frame("first"));
        let harness = Harness::start_with(&dialer, secs(30), unused_plain(), Some(trace.clone()), None);
        let client = harness.connect();
        client.expect_data("first");
        dialer.block(true);
        keeper.kill_leg();
        client.next_seen(WAIT);
        let mut writer = client.stream.try_clone().unwrap();
        thread::spawn(move || {
            let _ = writer.write_all(&vec![7u8; 9 * 1024 * 1024]);
        });
        // Unblock the dialer on the observable overflow, not after a drain, so the
        // OVERFLOW_END_LINGER cannot expire before a leg is there to carry the END.
        wait_until("ring-overflow traced", WAIT, || {
            std::fs::read_to_string(trace.join("local.jsonl"))
                .is_ok_and(|t| t.contains(r#""reason":"ring-overflow""#))
        });
        // A leg that comes up now is told why, best effort.
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        dialer.block(false);
        let frames = client.until_closed(secs(20));
        assert!(frames.iter().all(|(_, s)| is_resuming(s)), "{frames:?}");
        wait_until("END ring-overflow", WAIT, || {
            keeper.ends() == [end_reason::RING_OVERFLOW]
        });
        let _ = std::fs::remove_dir_all(trace);
    }

    #[test]
    fn fatal_reconnect_goes_to_reported_failure() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.script(LegScript::Fail(LegError::Fatal(io::Error::other(
            "Permission denied (publickey)",
        ))));
        keeper.kill_leg();
        client.until_closed(secs(5));
        let failure = harness.bridge().reported_failure().expect("failure reported");
        assert!(failure.to_string().contains("Permission denied"), "{failure}");
    }

    #[test]
    fn reconnect_end_before_welcome_is_fatal() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.keeper(&keeper, KLeg::new(Answer::End(end_reason::KEEPER_UNAVAILABLE)));
        keeper.kill_leg();
        client.until_closed(secs(5));
        let failure = harness.bridge().reported_failure().expect("failure reported");
        assert!(failure.to_string().contains(end_reason::KEEPER_UNAVAILABLE), "{failure}");
        assert_eq!(dialer.dial_count(), 2, "no retry after a positive refusal");
    }

    #[test]
    fn reconnect_remote_error_exit_is_fatal() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.script(LegScript::Exit { code: 3, stderr: "herdr: boom" });
        keeper.kill_leg();
        client.until_closed(secs(5));
        let failure = harness.bridge().reported_failure().expect("failure reported");
        assert!(failure.to_string().contains("boom"), "{failure}");
        assert_eq!(dialer.dial_count(), 2);
    }

    #[test]
    fn reconnect_eof_before_welcome_is_retried() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.script(LegScript::Exit { code: 0, stderr: "" });
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        client.next_seen(WAIT); // Resuming: the bridge saw the leg die
        keeper.push_down(frame("after"));
        client.expect_data("after");
        assert_eq!(dialer.dial_count(), 3, "EOF before WELCOME was retried");
        assert!(dialer.dials.lock().unwrap()[1..].iter().all(|(kind, bounded)| *kind == LegKind::Reconnect && *bounded));
    }

    #[test]
    fn reconnect_bound_before_welcome_is_retried() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(60), KLeg::new(Answer::New));
        // A leg that never answers: the 20 s pre-WELCOME bound ends it, then the next one works.
        dialer.keeper(&keeper, KLeg::new(Answer::Silent));
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        let started = Instant::now();
        client.next_seen(WAIT); // Resuming: the bridge saw the leg die
        keeper.push_down(frame("after"));
        let skipped = client.expect_data("after");
        assert!(started.elapsed() >= secs(19), "the bound is 20 s: {:?}", started.elapsed());
        assert!(skipped.iter().any(|(_, s)| is_resuming(s)));
    }

    #[test]
    fn new_or_unknown_on_resume_ends_session() {
        for answer in [Answer::Unknown, Answer::New] {
            let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
            let (harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
            dialer.keeper(&keeper, KLeg::new(answer));
            keeper.kill_leg();
            client.until_closed(secs(5));
            assert_eq!(dialer.dial_count(), 2);
            drop(harness);
        }
    }

    #[test]
    fn leg0_unsupported_serves_plain() {
        let cases = [
            LegScript::Fail(LegError::Unsupported(io::Error::other("old herdr"))),
            LegScript::Exit { code: 2, stderr: "unknown flag" },
            LegScript::Keeper(FakeKeeper::default(), KLeg::new(Answer::End(end_reason::KEEPER_UNAVAILABLE))),
            LegScript::Exit { code: 0, stderr: "" },
        ];
        for script in cases {
            let dialer = MemDialer::new();
            dialer.script(script);
            let (tx, rx) = mpsc::channel();
            let plain = PlainFallback::Test(Box::new(move |mut stream| {
                let mut got = vec![0u8; 16];
                stream.read_exact(&mut got)?;
                tx.send(got).unwrap();
                Ok(())
            }));
            let harness = Harness::start_with(&dialer, secs(30), plain, None, None);
            let mut client = harness.connect();
            client.write(b"HANDSHAKE-BYTES!");
            let got = rx.recv_timeout(WAIT).expect("plain fallback served the connection");
            assert_eq!(got, b"HANDSHAKE-BYTES!", "the untouched client stream");
            assert_eq!(dialer.dial_count(), 1);
        }
    }

    #[test]
    fn leg0_exit_255_not_retried_goes_to_failure_tx() {
        let dialer = MemDialer::new();
        dialer.script(LegScript::Exit { code: 255, stderr: "ssh: connect to host h port 22: Connection refused" });
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        client.until_closed(secs(5));
        let failure = harness.bridge().reported_failure().expect("failure reported");
        assert!(failure.to_string().contains("Connection refused"), "{failure}");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(dialer.dial_count(), 1, "leg 0 is never retried");
    }

    #[test]
    fn pre_welcome_gate_reads_nothing() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New).delay(secs(1)));
        let harness = Harness::start(&dialer, secs(30));
        let mut client = harness.connect();
        wait_until("hello", WAIT, || keeper.hellos().len() == 1);
        client.write(b"0123456789");
        wait_until("bytes after WELCOME", WAIT, || keeper.upstream() == b"0123456789");
        assert_eq!(keeper.pre_welcome_msgs(), 0, "no client byte before WELCOME");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(keeper.upstream(), b"0123456789", "exactly once");
    }

    // ---- leg extensions: admission token, silence override, ssh standby ----------------------

    #[test]
    fn hello_carries_leg_admission_token() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let token = "ab".repeat(32);
        let sent = token.clone();
        dialer.script(decorated(LegScript::Keeper(keeper.clone(), KLeg::new(Answer::New)), move |leg| {
            leg.with_admission_token(sent)
        }));
        let harness = Harness::start(&dialer, secs(30));
        let _client = harness.connect();
        wait_until("hello", WAIT, || keeper.hellos().len() == 1);
        assert_eq!(keeper.hellos()[0].quic_token, Some(token));
    }

    #[test]
    fn hello_omits_quic_token_without_one() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let harness = Harness::start(&dialer, secs(30));
        let _client = harness.connect();
        wait_until("hello", WAIT, || keeper.hellos().len() == 1);
        assert_eq!(keeper.hellos()[0].quic_token, None);
    }

    #[test]
    fn leg_silence_override_reaches_endpoint() {
        // The real 8 s default and a 30 s override cannot be waited out here; the override is
        // honoured through the same path with a short value. A heartbeat arrives every 2 s, so
        // only the override can end the leg this early.
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.script(decorated(LegScript::Keeper(keeper.clone(), KLeg::new(Answer::New)), |leg| {
            leg.with_silence_timeout(Duration::from_millis(300))
        }));
        keeper.push_down(frame("first"));
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        client.expect_data("first");
        wait_until("silence verdict redials", WAIT, || dialer.dial_count() == 2);
        drop(harness);

        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (harness, _client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(dialer.dial_count(), 1, "no override keeps the Phase 1 limit");
        drop(harness);
    }

    /// A standby leg whose teardown sets the returned flag.
    fn tracked_standby(keeper: &FakeKeeper, answer: Answer) -> (Leg, Arc<AtomicBool>) {
        let torn_down = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&torn_down);
        let (local, remote) = UnixStream::pair().unwrap();
        keeper.spawn_leg(remote, KLeg::new(answer));
        let leg = Leg::new(
            Box::new(local.try_clone().unwrap()),
            Box::new(local.try_clone().unwrap()),
            Box::new(move |_, _| {
                flag.store(true, Ordering::SeqCst);
                let _ = local.shutdown(std::net::Shutdown::Both);
                (None, None)
            }),
        );
        (leg, torn_down)
    }

    #[test]
    fn pre_welcome_end_switches_to_standby() {
        let (dialer, quic_keeper, ssh_keeper) =
            (MemDialer::new(), FakeKeeper::default(), FakeKeeper::default());
        let (standby, _) = tracked_standby(&ssh_keeper, Answer::New);
        dialer.script(decorated(
            LegScript::Keeper(quic_keeper.clone(), KLeg::new(Answer::End("quic-token"))),
            move |leg| leg.with_standby(standby),
        ));
        ssh_keeper.push_down(frame("first"));
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        client.expect_data("first");
        let first = quic_keeper.hellos();
        let second = ssh_keeper.hellos();
        assert_eq!((first.len(), second.len()), (1, 1));
        assert!(!second[0].resume, "a leg-0-style HELLO");
        assert_ne!(second[0].token, first[0].token, "a fresh session token");
        assert_eq!(second[0].previous_token, Some(first[0].token));
        assert_eq!(dialer.dial_count(), 1, "the standby is not redialed");
        drop(harness);
    }

    #[test]
    fn standby_hello_evicts_ghost() {
        // The QUIC keeper answered HELLO (it admitted the session) but WELCOME never reached
        // the bridge. The standby HELLO names that session; the keeper-side eviction itself is
        // `keeper::tests::previous_token_evicts_session`.
        let (dialer, quic_keeper, ssh_keeper) =
            (MemDialer::new(), FakeKeeper::default(), FakeKeeper::default());
        let (standby, _) = tracked_standby(&ssh_keeper, Answer::New);
        dialer.script(decorated(
            LegScript::LostWelcome(quic_keeper.clone(), KLeg::new(Answer::New)),
            move |leg| leg.with_standby(standby),
        ));
        ssh_keeper.push_down(frame("first"));
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        client.expect_data("first");
        wait_until("the QUIC keeper admits the session", WAIT, || {
            quic_keeper.st().endpoint.is_some()
        });
        let admitted = quic_keeper.hellos()[0].token;
        let standby_hello = &ssh_keeper.hellos()[0];
        assert_eq!(standby_hello.previous_token, Some(admitted));
        assert_ne!(standby_hello.token, admitted);
        drop(harness);
    }

    #[test]
    fn welcome_drops_standby() {
        let (dialer, keeper, ssh_keeper) =
            (MemDialer::new(), FakeKeeper::default(), FakeKeeper::default());
        let (standby, torn_down) = tracked_standby(&ssh_keeper, Answer::New);
        dialer.script(decorated(
            LegScript::Keeper(keeper.clone(), KLeg::new(Answer::New).delay(Duration::from_millis(600))),
            move |leg| leg.with_standby(standby),
        ));
        let harness = Harness::start(&dialer, secs(30));
        let _client = harness.connect();
        wait_until("hello", WAIT, || keeper.hellos().len() == 1);
        assert!(!torn_down.load(Ordering::SeqCst), "the standby lives until WELCOME");
        wait_until("standby torn down", WAIT, || torn_down.load(Ordering::SeqCst));
        assert!(ssh_keeper.hellos().is_empty(), "the standby never sent HELLO");
        drop(harness);
    }

    #[test]
    fn slow_client_does_not_block_leg() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        for i in 0..48 {
            keeper.push_down(frame_with(&format!("f{i}"), 60_000));
        }
        let mut harness = Harness::start(&dialer, secs(30));
        let client = harness.connect(); // never reads
        thread::sleep(Duration::from_millis(500));
        let before = keeper.last_rx().expect("bytes from the bridge");
        thread::sleep(Duration::from_millis(2600));
        let after = keeper.last_rx().unwrap();
        assert!(after > before, "heartbeats keep flowing while the client is stuck");
        let took = harness.shutdown();
        assert!(took < Duration::from_millis(600), "stop is prompt: {took:?}");
        assert_eq!(keeper.ends(), [end_reason::CLIENT_GONE]);
        drop(client);
    }

    #[test]
    fn ghost_token_survives_new_bridge_instance() {
        let dir = fresh_dir();
        let path = dir.join("s");
        let (dialer_a, keeper_a) = (MemDialer::new(), FakeKeeper::default());
        dialer_a.keeper(&keeper_a, KLeg::new(Answer::New));
        let mut a = Harness::start_with(&dialer_a, secs(30), unused_plain(), None, Some((dir.clone(), path.clone())));
        let _client_a = a.connect();
        wait_until("A established", WAIT, || last_token(&path).is_some());
        let first = keeper_a.hellos();
        assert_eq!(first[0].previous_token, None);
        a.dir = fresh_dir(); // keep the shared dir alive across the instance
        a.shutdown();
        let (dialer_b, keeper_b) = (MemDialer::new(), FakeKeeper::default());
        dialer_b.keeper(&keeper_b, KLeg::new(Answer::New));
        let b = Harness::start_with(&dialer_b, secs(30), unused_plain(), None, Some((dir.clone(), path.clone())));
        let _client_b = b.connect();
        wait_until("B hello", WAIT, || !keeper_b.hellos().is_empty());
        let hello = keeper_b.hellos().remove(0);
        assert_eq!(hello.previous_token, Some(first[0].token), "stored token is previous_token");
        assert!(!hello.resume);
        // A different path knows nothing.
        let (dialer_c, keeper_c) = (MemDialer::new(), FakeKeeper::default());
        dialer_c.keeper(&keeper_c, KLeg::new(Answer::New));
        let c = Harness::start(&dialer_c, secs(30));
        let _client_c = c.connect();
        wait_until("C hello", WAIT, || !keeper_c.hellos().is_empty());
        assert_eq!(keeper_c.hellos()[0].previous_token, None);
        drop(b);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn health_margin_stall_after_ping() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        // The client pings; the pong comes back through the session, then the leg stalls.
        client.write(b"ping");
        wait_until("ping at the keeper", WAIT, || keeper.upstream() == b"ping");
        keeper.push_down(frame("pong"));
        let pong_at = client.expect_data("pong").last().unwrap().0;
        keeper.set_stall(true);
        let (at, got) = client.next_seen(secs(12));
        let first_injection = at.duration_since(pong_at);
        assert!(is_resuming(&got), "{got:?}");
        assert!(
            first_injection < Duration::from_millis(8500),
            "the first Resuming must beat the client's 10 s health timeout by ~1.5 s: {first_injection:?}"
        );
        assert!(first_injection > secs(7), "and is the dead verdict, not earlier: {first_injection:?}");
        keeper.set_stall(false);
    }

    #[test]
    fn server_eof_delivers_frames_drops_partial_then_closes() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        keeper.push_down([frame("a"), shutdown_frame(), vec![100, 0, 0, 0, 1, 2, 3]].concat());
        keeper.queue_end(end_reason::SERVER_EOF);
        let harness = Harness::start(&dialer, secs(30));
        let client = harness.connect();
        let frames: Vec<_> = client.until_closed(WAIT).into_iter().map(|(_, s)| s).collect();
        assert_eq!(frames, [data("a"), data("shutdown")]);
    }

    #[test]
    fn server_eof_while_down_then_resume() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.block(true);
        keeper.kill_leg();
        client.next_seen(WAIT);
        keeper.push_down([frame("late"), shutdown_frame(), vec![100, 0, 0, 0, 1]].concat());
        keeper.queue_end(end_reason::SERVER_EOF);
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        dialer.block(false);
        let frames: Vec<_> = client
            .until_closed(WAIT)
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| !matches!(s, Seen::Link(_)))
            .collect();
        assert_eq!(frames, [data("late"), data("shutdown")]);
    }

    #[test]
    fn stop_with_live_leg_sends_client_gone_within_500ms() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (mut harness, _client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        let took = harness.shutdown();
        assert!(took < Duration::from_millis(700), "{took:?}");
        assert_eq!(keeper.ends(), [end_reason::CLIENT_GONE]);
    }

    #[test]
    fn stop_after_client_eof_still_ends_keeper_session() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (mut harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        client.write(&[9u8; 1024]);
        client.close();
        let took = harness.shutdown();
        assert!(took < Duration::from_millis(700), "{took:?}");
        wait_until("END", WAIT, || keeper.ends() == [end_reason::CLIENT_GONE]);
    }

    #[test]
    fn final_end_lost_with_leg_is_resent_on_resume() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New).cut_on_end_write());
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        let mut harness = Harness::start(&dialer, secs(30));
        let mut client = harness.connect();
        client.write(b"hello");
        client.close();
        wait_until("END resent on the resumed leg", WAIT, || {
            keeper.ends() == [end_reason::CLIENT_GONE]
        });
        assert_eq!(keeper.hellos().len(), 2, "the END needed a second leg");
        assert_eq!(keeper.upstream(), b"hello");
        harness.shutdown();
    }

    #[test]
    fn end_write_failure_with_open_leg_resends_end_on_resume() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New).fail_end_write());
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        let mut harness = Harness::start(&dialer, secs(30));
        let mut client = harness.connect();
        client.write(b"hello");
        client.close();
        wait_until("END resent on the resumed leg", WAIT, || {
            keeper.ends() == [end_reason::CLIENT_GONE]
        });
        assert_eq!(keeper.hellos().len(), 2, "the END needed a second leg");
        assert_eq!(keeper.upstream(), b"hello");
        harness.shutdown();
    }

    #[test]
    fn client_gone_mid_server_eof_drain_closes_within_stop_budget() {
        let trace = fresh_dir();
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        // Four frames: the keeper's initial window (256 KiB) must hold them all, or the END
        // would wait for ACKs the paused client never lets the bridge send.
        for _ in 0..4 {
            keeper.push_down(frame_with("big", 60_000));
        }
        keeper.push_down(shutdown_frame());
        keeper.queue_end(end_reason::SERVER_EOF);
        let harness =
            Harness::start_with(&dialer, secs(30), unused_plain(), Some(trace.clone()), None);
        let client = Client::connect_paused(&harness.path);
        wait_until("the keeper wrote END{server-eof}", WAIT, || {
            keeper.st().end_written
        });
        let gone_at = Instant::now();
        client.stop_reading();
        let log = trace.join("local.jsonl");
        wait_until("session_end traced", Duration::from_secs(3), || {
            std::fs::read_to_string(&log).is_ok_and(|l| l.contains("\"event\":\"session_end\""))
        });
        assert!(
            gone_at.elapsed() < CLOSE_BUDGET + Duration::from_secs(1),
            "closed {:?} after the client went away",
            gone_at.elapsed()
        );
        let lines = std::fs::read_to_string(&log).unwrap();
        let end_line = lines
            .lines()
            .find(|l| l.contains("\"event\":\"session_end\""))
            .unwrap();
        assert!(end_line.contains("server-eof"), "{end_line}");
        assert!(
            !keeper.st().acked_all,
            "the bridge acked frames the client never wrote"
        );
        drop(client);
        let _ = std::fs::remove_dir_all(trace);
    }

    #[test]
    fn stalled_client_does_not_delay_client_gone_end() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let mut harness = Harness::start(&dialer, secs(30));
        let _client = Client::connect_paused(&harness.path);
        wait_until("leg 0", WAIT, || !keeper.hellos().is_empty());
        for _ in 0..48 {
            keeper.push_down(frame_with("big", 60_000));
        }
        // The keeper's whole window is in flight (its endpoint now runs the adaptive controller,
        // so INITIAL, not the hard cap), so the bridge's client writer is blocked on the stalled
        // socket with frames queued behind it.
        wait_until("a window of downstream bytes", WAIT, || {
            keeper.st().down_sent >= super::super::window::INITIAL
        });
        let stop_at = Instant::now();
        let took = harness.shutdown();
        assert!(took < Duration::from_millis(700), "{took:?}");
        assert_eq!(keeper.ends(), [end_reason::CLIENT_GONE]);
        let end_at = keeper.st().ends_at[0];
        assert!(
            end_at.saturating_duration_since(stop_at) < Duration::from_millis(500),
            "END reached the leg {:?} after stop",
            end_at.saturating_duration_since(stop_at)
        );
    }

    #[test]
    fn server_eof_with_slow_client_delivers_shutdown() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        for _ in 0..20 {
            keeper.push_down(frame_with("big", 60_000));
        }
        keeper.push_down(shutdown_frame());
        keeper.queue_end(end_reason::SERVER_EOF);
        let harness = Harness::start(&dialer, secs(30));
        let started = Instant::now();
        let client = Client::connect_slow(&harness.path, Duration::from_millis(50));
        let frames: Vec<_> = client.until_closed(WAIT).into_iter().map(|(_, s)| s).collect();
        assert!(
            started.elapsed() > CLOSE_BUDGET,
            "the drain must outlast the stop budget: {:?}",
            started.elapsed()
        );
        let mut expected = vec![data("big"); 20];
        expected.push(data("shutdown"));
        assert_eq!(frames, expected);
        wait_until("the keeper saw the final ACK", WAIT, || keeper.st().acked_all);
    }

    #[test]
    fn client_eof_sends_client_gone_after_ring() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        client.write(&vec![5u8; 300 * 1024]);
        client.close();
        wait_until("END", WAIT, || !keeper.ends().is_empty());
        assert_eq!(keeper.ends(), [end_reason::CLIENT_GONE]);
        assert_eq!(keeper.st().upstream_at_end, Some(300 * 1024), "END follows every ring byte");
    }

    #[test]
    fn valid_data_then_gap_in_one_write_is_delivered_once() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.corrupt_next_write();
        keeper.push_down(frame("lead"));
        // The bridge delivers `lead`, drops the leg on the gap, and resumes from a
        // recv_offset that already counts `lead`.
        wait_until("second leg", WAIT, || keeper.hellos().len() == 2);
        keeper.push_down(frame("after"));
        let mut frames = Vec::new();
        while frames.len() < 2 {
            match client.next_seen(WAIT) {
                (_, Seen::Link(_)) => {}
                (_, got) => frames.push(got),
            }
        }
        assert_eq!(frames, [data("lead"), data("after")]);
        assert!(keeper.hellos()[1].resume, "second leg is a resume");
    }

    // ---- render hosting, resync and upstream injection --------------------------------------------

    use crate::client::mosh_outage::server_hooks::SURFACE_RESYNC_KIND;
    use crate::protocol::{surface_reuse, ClientMessage};

    fn client_frame(kind: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(
            &mut out,
            &ClientMessage::EndpointControl {
                kind: kind.to_owned(),
                data: String::new(),
            },
        )
        .unwrap();
        out
    }

    /// The kinds of the whole client frames in a client->server byte stream.
    fn kinds(stream: &[u8]) -> Vec<String> {
        let frames = FrameSplitter::new().push(stream).unwrap();
        frames
            .iter()
            .map(|frame| {
                match read_message::<_, ClientMessage>(&mut &frame[..], MAX_GRAPHICS_FRAME_SIZE)
                    .unwrap()
                {
                    ClientMessage::EndpointControl { kind, .. } => kind,
                    other => panic!("unexpected client frame {other:?}"),
                }
            })
            .collect()
    }

    fn resync_kind() -> String {
        SURFACE_RESYNC_KIND.to_owned()
    }

    /// A split-session bridge on `dialer` (scripted by the caller) whose client has seen `first`.
    fn split_established(dialer: &MemDialer, keeper: &FakeKeeper) -> (Harness, Client) {
        keeper.push_down(frame("first"));
        let harness = Harness::start_split(dialer, secs(30));
        let client = harness.connect();
        client.expect_data("first");
        (harness, client)
    }

    fn lane_leg(keeper: &FakeKeeper, answer: Answer) -> (LegScript, mpsc::Sender<RenderArrival>) {
        let (lane_tx, lane_rx) = mpsc::channel();
        let script = decorated(
            LegScript::Keeper(keeper.clone(), KLeg::new(answer)),
            move |leg| leg.with_render_lane(lane_rx),
        );
        (script, lane_tx)
    }

    fn lane_frame(seq: u64, class: FrameClass, kind: &str) -> RenderArrival {
        RenderArrival::Complete {
            seq,
            class,
            bytes: frame(kind),
        }
    }

    /// Frames of data kinds the client receives until `last`, link frames dropped.
    fn data_until(client: &Client, last: &str) -> Vec<String> {
        let mut kinds = Vec::new();
        loop {
            match client.next_seen(WAIT).1 {
                Seen::Link(_) => {}
                Seen::Data(kind) => {
                    let done = kind == last;
                    kinds.push(kind);
                    if done {
                        return kinds;
                    }
                }
            }
        }
    }

    #[test]
    fn resume_after_first_leg_emits_exactly_one_resync_between_client_frames() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let (_harness, mut client) = split_established(&dialer, &keeper);
        client.write(&client_frame("c1"));
        wait_until("c1", WAIT, || kinds(&keeper.upstream()) == ["c1"]);
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        wait_until("the resync", WAIT, || kinds(&keeper.upstream()).len() == 2);
        client.write(&client_frame("c2"));
        wait_until("c2", WAIT, || kinds(&keeper.upstream()).len() == 3);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(kinds(&keeper.upstream()), ["c1", &resync_kind(), "c2"]);
        assert!(keeper.hellos()[1].render_split, "every leg's HELLO asks for the split");
    }

    #[test]
    fn resume_resync_waits_for_the_client_frame_in_flight() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let (_harness, mut client) = split_established(&dialer, &keeper);
        let (c1, c2) = (client_frame("c1"), client_frame("c2"));
        client.write(&[c1.clone(), c2[..3].to_vec()].concat());
        wait_until("c1 and the head of c2", WAIT, || {
            keeper.upstream().len() == c1.len() + 3
        });
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        wait_until("second leg", WAIT, || keeper.hellos().len() == 2);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(keeper.upstream().len(), c1.len() + 3, "nothing inside c2");
        client.write(&c2[3..]);
        wait_until("c2 and the resync", WAIT, || kinds(&keeper.upstream()).len() == 3);
        assert_eq!(kinds(&keeper.upstream()), ["c1", "c2", &resync_kind()]);
    }

    #[test]
    fn non_split_session_sends_no_resync_on_resume() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        client.write(&client_frame("c1"));
        wait_until("c1", WAIT, || kinds(&keeper.upstream()) == ["c1"]);
        keeper.kill_leg();
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        wait_until("second leg", WAIT, || keeper.hellos().len() == 2);
        client.write(&client_frame("c2"));
        wait_until("c2", WAIT, || kinds(&keeper.upstream()).len() == 2);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(kinds(&keeper.upstream()), ["c1", "c2"], "no resync without the split");
    }

    // ---- echo subscribe -------------------------------------------------------------------------

    use crate::client::mosh_outage::predict::ECHO_SUBSCRIBE_KIND;

    fn subscribe_kind() -> String {
        ECHO_SUBSCRIBE_KIND.to_owned()
    }

    /// A linked, established session with a leg RTT probe (so `Linked` is injected without
    /// waiting for heartbeats); the client has seen `first`.
    fn echo_established(
        dialer: &MemDialer,
        keeper: &FakeKeeper,
        echo: bool,
    ) -> (Harness, Client) {
        dialer.script(decorated(
            LegScript::Keeper(keeper.clone(), KLeg::new(Answer::New)),
            |leg| leg.with_rtt_probe(Box::new(|| Some(Duration::from_millis(5)))),
        ));
        keeper.push_down(frame("first"));
        let harness = Harness::start_echo(dialer, secs(30), echo);
        let client = harness.connect();
        client.expect_data("first");
        (harness, client)
    }

    /// The `echo` field of the next `Linked` the client receives.
    fn next_linked_echo(client: &Client) -> bool {
        loop {
            if let Seen::Link(LinkStatus::Linked { echo, .. }) = client.next_seen(WAIT).1 {
                return echo;
            }
        }
    }

    #[test]
    fn subscribe_appears_once_right_after_hello_and_is_replayed_after_resume() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = echo_established(&dialer, &keeper, true);
        // The bytes of the first leg never reach the keeper; the new leg must replay them.
        keeper.set_stall(true);
        client.write(&[client_frame("hello"), client_frame("c2")].concat());
        thread::sleep(Duration::from_millis(300));
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        wait_until("second leg", WAIT, || keeper.hellos().len() == 2);
        keeper.set_stall(false);
        wait_until("the replay", WAIT, || kinds(&keeper.upstream()).len() == 3);
        client.write(&client_frame("c3"));
        wait_until("c3", WAIT, || kinds(&keeper.upstream()).len() == 4);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            kinds(&keeper.upstream()),
            ["hello", &subscribe_kind(), "c2", "c3"],
            "the subscribe sits right after HELLO, exactly once, offsets intact"
        );
    }

    #[test]
    fn subscribe_waits_for_the_client_frame_in_flight() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = echo_established(&dialer, &keeper, true);
        let hello = client_frame("hello");
        client.write(&hello[..3]);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(keeper.upstream().len(), 3, "nothing injected inside HELLO");
        client.write(&hello[3..]);
        wait_until("hello and the subscribe", WAIT, || kinds(&keeper.upstream()).len() == 2);
        assert_eq!(kinds(&keeper.upstream()), ["hello", &subscribe_kind()]);
    }

    #[test]
    fn no_subscribe_without_capability_or_with_mode_off() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        // `echo` is false when the probe lacks `x-mosh-echo-v1` or the mode is off.
        let (_harness, mut client) = echo_established(&dialer, &keeper, false);
        client.write(&[client_frame("hello"), client_frame("c2")].concat());
        wait_until("both frames", WAIT, || kinds(&keeper.upstream()).len() == 2);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(kinds(&keeper.upstream()), ["hello", "c2"]);
        // SRTT keeps being reported, never with echo; the JSON has no echo field.
        let mut linked = 0;
        while linked < 2 {
            match client.next_seen(WAIT).1 {
                Seen::Link(LinkStatus::Linked { echo, .. }) => {
                    assert!(!echo);
                    linked += 1;
                }
                Seen::Link(other) => panic!("unexpected {other:?}"),
                Seen::Data(_) => {}
            }
            if linked == 1 {
                keeper.push_down(frame("poke"));
                linked += 1;
            }
        }
        let json = serde_json::to_string(&LinkStatus::Linked { srtt_ms: Some(5), echo: false })
            .unwrap();
        assert!(!json.contains("echo"), "{json}");
    }

    #[test]
    fn linked_carries_echo_only_after_subscribe_injected() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = echo_established(&dialer, &keeper, true);
        // Before the client's HELLO the subscribe is not on the stream: echo is false.
        assert!(!next_linked_echo(&client));
        client.write(&client_frame("hello"));
        // The subscribe is injected right behind HELLO and the client is told at once.
        assert!(next_linked_echo(&client));
        wait_until("hello and the subscribe", WAIT, || kinds(&keeper.upstream()).len() == 2);
        assert_eq!(kinds(&keeper.upstream()), ["hello", &subscribe_kind()]);
    }

    #[test]
    fn resume_linked_keeps_echo_after_subscribe() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = echo_established(&dialer, &keeper, true);
        client.write(&client_frame("hello"));
        // Skip the pre-HELLO `Linked` (no echo yet) still queued for the client.
        while !next_linked_echo(&client) {}
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        keeper.kill_leg();
        loop {
            if matches!(client.next_seen(WAIT).1, Seen::Link(LinkStatus::Resuming { .. })) {
                break;
            }
        }
        assert!(next_linked_echo(&client), "the Linked after the resume still says echo");
    }

    #[test]
    fn render_frames_on_session_stream_of_lane_leg_are_not_forwarded() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (script, _lane) = lane_leg(&keeper, Answer::New);
        dialer.script(script);
        let (_harness, client) = split_established(&dialer, &keeper);
        keeper.push_down(
            [frame(surface_reuse::MESSAGE_KIND), frame("after")].concat(),
        );
        assert_eq!(data_until(&client, "after"), ["after"]);
        keeper.push_down(frame(surface_reuse::MESSAGE_KIND));
        wait_until("the dropped frame is acked", WAIT, || {
            keeper
                .st()
                .endpoint
                .as_ref()
                .is_some_and(|endpoint| endpoint.fully_acked())
        });
    }

    #[test]
    fn render_frames_on_ssh_leg_are_forwarded() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let (_harness, client) = split_established(&dialer, &keeper);
        keeper.push_down([frame(surface_reuse::MESSAGE_KIND), frame("after")].concat());
        assert_eq!(
            data_until(&client, "after"),
            [surface_reuse::MESSAGE_KIND, "after"]
        );
    }

    #[test]
    fn lane_gap_produces_one_resync_and_no_chained_before_self_contained() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (script, lane) = lane_leg(&keeper, Answer::New);
        dialer.script(script);
        let (_harness, mut client) = split_established(&dialer, &keeper);
        client.write(&client_frame("c1"));
        wait_until("c1", WAIT, || kinds(&keeper.upstream()) == ["c1"]);
        let send = |arrival| lane.send(arrival).unwrap();
        send(lane_frame(0, FrameClass::SelfContained, "sc0"));
        assert_eq!(data_until(&client, "sc0"), ["sc0"]);
        // Seq 1 is reset: one gap, one resync; the chained frames behind it are dropped.
        send(RenderArrival::Reset { seq: 1 });
        send(lane_frame(2, FrameClass::Chained, "ch2"));
        send(RenderArrival::Reset { seq: 3 });
        send(lane_frame(4, FrameClass::Chained, "ch4"));
        send(lane_frame(5, FrameClass::SelfContained, "sc5"));
        assert_eq!(data_until(&client, "sc5"), ["sc5"]);
        wait_until("the resync", WAIT, || kinds(&keeper.upstream()).len() == 2);
        thread::sleep(Duration::from_millis(200));
        assert_eq!(kinds(&keeper.upstream()), ["c1", &resync_kind()]);
    }

    #[test]
    fn probe_without_supersede_capability_sends_no_render_split() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, _client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        assert!(!keeper.hellos()[0].render_split);
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        dialer.keeper(&keeper, KLeg::new(Answer::New));
        let (_harness, _client) = split_established(&dialer, &keeper);
        assert!(keeper.hellos()[0].render_split);
    }

    // ---- upstream injection (the API mosh-19g.3.4 builds on) ------------------------------------------

    struct Bare {
        params: BridgeParams,
        dialer: SharedDialer,
        stop: Arc<AtomicBool>,
        trace: Trace,
    }

    impl Bare {
        fn new() -> Bare {
            Bare {
                params: BridgeParams {
                    local_socket: PathBuf::from("unused"),
                    grace: secs(30),
                    noninteractive: true,
                    plain: unused_plain(),
                    trace_dir: None,
                    render_split: true,
                    echo: false,
                },
                dialer: Arc::new(Mutex::new(Box::new(MemDialer::new()))),
                stop: Arc::new(AtomicBool::new(false)),
                trace: Trace::new(None),
            }
        }

        fn conn(&self) -> Conn<'_> {
            Conn::new(
                &self.params,
                &self.dialer,
                &self.stop,
                &self.trace,
                Token::generate().unwrap(),
                None,
            )
        }
    }

    /// Bring `conn`'s endpoint up against a fresh keeper endpoint (a fresh keeper has received
    /// nothing, so a resume replays the whole ring).
    fn link_up(conn: &mut Conn<'_>) -> SessionEndpoint {
        let now = Instant::now();
        let hello = conn.endpoint.hello(now, 60_000, None);
        let mut keeper = SessionEndpoint::new(Role::Keeper, conn.endpoint.token(), 1 << 20);
        let events = keeper.on_leg_bytes(now, &hello).unwrap();
        let [SessionEvent::Hello(h)] = &events[..] else { panic!("{events:?}") };
        let status = if h.resume { WelcomeStatus::Resumed } else { WelcomeStatus::New };
        let welcome = keeper.welcome(now, status, h.recv_offset).unwrap();
        conn.endpoint.on_leg_bytes(now, &welcome).unwrap();
        keeper
    }

    /// Everything `conn` has due, as the keeper receives it.
    fn flow(conn: &mut Conn<'_>, keeper: &mut SessionEndpoint) -> Vec<u8> {
        let now = Instant::now();
        let mut got = Vec::new();
        while let Some(message) = conn.endpoint.poll_send(now) {
            for event in keeper.on_leg_bytes(now, &message).unwrap() {
                if let SessionEvent::Deliver(bytes) = event {
                    got.extend(bytes);
                }
            }
        }
        got
    }

    #[test]
    fn inject_upstream_waits_for_a_client_frame_boundary() {
        let bare = Bare::new();
        let mut conn = bare.conn();
        let mut keeper = link_up(&mut conn);
        let (c1, c2, injected) = (client_frame("c1"), client_frame("c2"), client_frame("inj"));
        conn.push_client_bytes(&c1[..3]).unwrap();
        conn.inject_upstream(injected.clone(), InjectAt::NextBoundary);
        assert_eq!(flow(&mut conn, &mut keeper), c1[..3], "held mid-frame");
        // The chunk that completes c1 also carries the head of c2: the frame goes between.
        conn.push_client_bytes(&[&c1[3..], &c2[..2]].concat()).unwrap();
        assert_eq!(
            flow(&mut conn, &mut keeper),
            [&c1[3..], &injected[..], &c2[..2]].concat()
        );
        conn.push_client_bytes(&c2[2..]).unwrap();
        let mut all = [&c1[..], &injected[..], &c2[..]].concat();
        assert_eq!(flow(&mut conn, &mut keeper), c2[2..]);
        assert_eq!(kinds(&all), ["c1", "inj", "c2"]);
        // At a boundary the next injection goes out at once.
        conn.inject_upstream(client_frame("now"), InjectAt::NextBoundary);
        assert_eq!(flow(&mut conn, &mut keeper), client_frame("now"));
        all.extend(client_frame("now"));
        // Offset-bearing: a new leg to a keeper that saw nothing replays all of it, in order.
        conn.endpoint.leg_down();
        let mut fresh = link_up(&mut conn);
        assert_eq!(flow(&mut conn, &mut fresh), all);
    }

    #[test]
    fn inject_after_first_frame_follows_hello() {
        let bare = Bare::new();
        let mut conn = bare.conn();
        let mut keeper = link_up(&mut conn);
        let (hello, c2, injected) = (client_frame("hello"), client_frame("c2"), client_frame("sub"));
        // No client frame yet: even at a boundary the frame waits for the first one.
        conn.inject_upstream(injected.clone(), InjectAt::AfterFirstFrame);
        assert!(flow(&mut conn, &mut keeper).is_empty());
        // Hello and c2 in one chunk: the frame follows hello, not c2.
        conn.push_client_bytes(&[&hello[..], &c2[..]].concat()).unwrap();
        let got = flow(&mut conn, &mut keeper);
        assert_eq!(kinds(&got), ["hello", "sub", "c2"]);
        conn.endpoint.leg_down();
        let mut fresh = link_up(&mut conn);
        assert_eq!(flow(&mut conn, &mut fresh), got, "replayed with offsets intact");
    }
}
