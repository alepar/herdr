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
use super::link_frames::{encode_link_frame, FrameSplitter};
use super::session::{
    self, end_reason, Decoder, LinkVerdict, Role, SessionEndpoint, SessionEvent, SessionMsg, Token,
    WelcomeStatus, MAX_DATA_CHUNK,
};
use super::ssh_leg::{Leg, LegDialer, LegEndReport, LegError, LegKind};
use crate::client::mosh_outage::LinkStatus;
use crate::ipc::{self, LocalStream};
use interprocess::local_socket::traits::Listener as _;
use interprocess::local_socket::ListenerNonblockingMode;
use interprocess::TryClone as _;
use std::collections::BTreeMap;
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
/// How long a stop waits for `END{client-gone}` to be flushed before tearing the leg down.
const STOP_END_FLUSH: Duration = Duration::from_millis(500);
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
    let (tx, rx) = mpsc::channel();
    let mut conn = Conn {
        params,
        dialer,
        stop,
        trace,
        cancel: Arc::new(AtomicBool::new(false)),
        tx,
        rx,
        endpoint: SessionEndpoint::new(Role::Local, token, LOCAL_RING_CAP),
        splitter: FrameSplitter::new(),
        stream: Some(stream),
        client: None,
        leg: None,
        leg_gen: 0,
        stage: Stage::Dialing,
        kind: LegKind::Initial,
        attempt: 0,
        down: None,
        frames_end: 0,
        client_closed: false,
        client_idle: false,
        end_deadline: None,
        end_flush: None,
        stop_deadline: None,
        peek: Decoder::default(),
        peer_recv_offset: None,
        dial_thread: None,
        done: None,
    };
    conn.run();
    conn.finish()
}

enum Event {
    Leg(u64, LegIn),
    Dialed(Result<Leg, LegError>),
    Client(Vec<u8>),
    ClientEof,
}

enum LegIn {
    Bytes(Vec<u8>),
    Eof,
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

struct Down {
    give_up_at: Instant,
    last_inject: Option<Instant>,
    resuming_injected: bool,
}

enum Done {
    /// Serve this connection through the upstream plain bridge with the untouched stream.
    Plain,
    /// Tear down at once; the client socket is shut, nothing more is delivered.
    Stopped,
    /// Deliver every queued frame, then close the client socket.
    Close {
        reason: String,
        error: Option<io::Error>,
        final_ack: bool,
    },
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
}

struct ClientIo {
    go_tx: mpsc::Sender<()>,
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
    splitter: FrameSplitter,
    /// The client stream until the client threads take it (and for the leg-0 fallback).
    stream: Option<LocalStream>,
    client: Option<ClientIo>,
    leg: Option<LegIo>,
    leg_gen: u64,
    stage: Stage,
    kind: LegKind,
    attempt: u32,
    down: Option<Down>,
    /// Stream bytes of the complete frames handed to the client writer.
    frames_end: u64,
    /// The client side is finished (EOF, error or overflow): nothing more is forwarded.
    client_closed: bool,
    /// The client reader waits for a go token.
    client_idle: bool,
    end_deadline: Option<Instant>,
    /// (leg writer message count that includes the final END, flush deadline)
    end_flush: Option<(u64, Instant)>,
    /// The stop budget set by `on_stop`; teardown reuses what remains of it.
    stop_deadline: Option<Instant>,
    /// Pre-WELCOME copy of the inbound bytes, only to learn WELCOME's `recv_offset` for tracing.
    peek: Decoder,
    peer_recv_offset: Option<u64>,
    dial_thread: Option<JoinHandle<()>>,
    done: Option<Done>,
}

impl Conn<'_> {
    fn run(&mut self) {
        self.start_dial(LegKind::Initial, None);
        while self.done.is_none() {
            match self.rx.recv_timeout(TICK) {
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

    fn close_with(&mut self, reason: &str, error: Option<io::Error>) {
        self.done = Some(Done::Close {
            reason: reason.to_owned(),
            error,
            final_ack: false,
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
        if self.stop.load(Ordering::Acquire) {
            self.done = Some(Done::Stopped);
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
            (LegKind::Initial, LegError::Fatal(err)) => self.close_with("leg0-failed", Some(err)),
            // Only a stop makes leg 0 retryable.
            (LegKind::Initial, LegError::Retryable(_)) => self.done = Some(Done::Stopped),
            (LegKind::Reconnect, LegError::Retryable(err)) => {
                tracing::debug!(%err, attempt = self.attempt, "mosh reconnect attempt failed");
                self.attempt += 1;
                self.schedule_backoff();
            }
            (LegKind::Reconnect, LegError::Fatal(err) | LegError::Unsupported(err)) => {
                self.close_with("reconnect-fatal", Some(err));
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
            self.close_with(
                "leg-io",
                Some(io::Error::other("leg has no byte stream halves")),
            );
            return;
        };
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
        let (leg_tx, leg_rx) = mpsc::sync_channel::<Vec<u8>>(4);
        let flushed = Arc::new(AtomicU64::new(0));
        let writer_flushed = Arc::clone(&flushed);
        thread::spawn(move || {
            for message in leg_rx {
                if writer.write_all(&message).and_then(|()| writer.flush()).is_err() {
                    break;
                }
                writer_flushed.fetch_add(1, Ordering::Release);
            }
        });

        let now = Instant::now();
        let previous = match self.kind {
            LegKind::Initial => last_token(&self.params.local_socket),
            LegKind::Reconnect => None,
        };
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
        });
        self.stage = Stage::Welcome;
    }

    /// Tear the current leg down and return its end report.
    fn kill_leg(&mut self, kill: bool) -> Option<LegEndReport> {
        let LegIo { leg, tx, .. } = self.leg.take()?;
        drop(tx);
        Some(leg.finish(kill))
    }

    /// Graceful leg close: the writer drains what is queued and drops ssh's stdin (EOF), then
    /// we give the child until `deadline` to hang up before it is killed.
    fn close_leg(&mut self, deadline: Instant) {
        let Some(LegIo {
            leg,
            tx,
            reader_done,
            ..
        }) = self.leg.take()
        else {
            return;
        };
        drop(tx);
        while !reader_done.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        leg.finish(true);
    }

    /// The leg ended before `WELCOME` (explicit END, EOF, protocol error or the bound).
    fn pre_welcome_failed(&mut self, kill: bool) {
        let Some(report) = self.kill_leg(kill) else {
            return;
        };
        self.endpoint.leg_down();
        let err = lock_dialer(self.dialer).classify(self.kind, &report);
        self.dial_failed(err);
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
        }
    }

    fn leg_ended(&mut self, reason: &str) {
        match self.stage {
            Stage::Welcome => self.pre_welcome_failed(reason != "eof"),
            Stage::Linked => self.leg_dead(reason),
            _ => {}
        }
    }

    fn on_leg_bytes(&mut self, bytes: &[u8]) {
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
            if self.done.is_some() || self.leg.is_none() {
                break;
            }
            self.on_session_event(event);
        }
        if let Some(err) = error {
            if self.done.is_none() && self.leg.is_some() {
                self.leg_ended(&format!("protocol error: {err}"));
            }
        }
        if self.done.is_none() && self.stage == Stage::Linked {
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
                self.close_with("session-gone", None);
            }
            SessionEvent::PeerEnd(reason) => {
                if self.stage == Stage::Welcome {
                    if let Some(io) = &mut self.leg {
                        io.leg.record_pre_welcome_end(reason);
                    }
                    self.pre_welcome_failed(false);
                } else if reason == end_reason::SERVER_EOF {
                    self.splitter.drop_partial();
                    self.done = Some(Done::Close {
                        reason: reason.clone(),
                        error: None,
                        final_ack: true,
                    });
                } else {
                    self.close_with(&reason, None);
                }
            }
            SessionEvent::Hello(_) => self.leg_ended("unexpected HELLO"),
        }
    }

    fn deliver(&mut self, bytes: &[u8]) {
        if self.client_closed {
            return;
        }
        match self.splitter.push(bytes) {
            Ok(frames) => {
                for frame in frames {
                    self.frames_end += frame.len() as u64;
                    let end_offset = self.frames_end;
                    self.client_send(ClientOut::Stream {
                        bytes: frame,
                        end_offset,
                    });
                }
            }
            Err(oversized) => self.close_with(
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
                    self.close_with("client-setup", Some(err));
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
                        self.inject(LinkStatus::Linked);
                    }
                }
                self.attempt = 0;
            }
            WelcomeStatus::Unknown => {}
        }
        self.stage = Stage::Linked;
        self.trace.emit("leg_up", serde_json::json!({}));
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

        let (go_tx, go_rx) = mpsc::channel::<()>();
        let events = self.tx.clone();
        let reader = thread::spawn(move || {
            let mut read_half = read_half;
            let mut buf = vec![0u8; MAX_DATA_CHUNK];
            while go_rx.recv().is_ok() {
                loop {
                    match read_half.read(&mut buf) {
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

        let _ = go_tx.send(());
        self.client = Some(ClientIo {
            go_tx,
            out_tx,
            written,
            shutdown,
            reader,
            writer,
        });
        self.client_idle = false;
        Ok(())
    }

    fn on_client(&mut self, bytes: &[u8]) {
        self.client_idle = true;
        if self.client_closed {
            return;
        }
        if self.endpoint.push_source(bytes).is_err() {
            // Only possible while no leg is up: the session ends here.
            self.trace.emit(
                "session_end",
                serde_json::json!({"reason": end_reason::RING_OVERFLOW}),
            );
            self.endpoint.queue_final_end(end_reason::RING_OVERFLOW);
            self.client_gone();
            self.end_deadline = Some(Instant::now() + OVERFLOW_END_LINGER);
        } else {
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
        self.endpoint.queue_final_end(end_reason::CLIENT_GONE);
        self.client_gone();
    }

    /// Close the client socket behind the frames already queued; the keeper is still told.
    fn client_gone(&mut self) {
        self.client_closed = true;
        self.client_send(ClientOut::Close);
    }

    fn inject(&mut self, status: LinkStatus) {
        let state = match status {
            LinkStatus::Resuming { .. } => "resuming",
            LinkStatus::Linked => "linked",
        };
        self.trace.emit("inject", serde_json::json!({ "state": state }));
        self.client_send(ClientOut::Injected(encode_link_frame(&status)));
    }

    // ---- pump and timers ------------------------------------------------------------------

    fn tick(&mut self) {
        let now = Instant::now();
        if self.stop.load(Ordering::Acquire) {
            self.cancel.store(true, Ordering::Release);
            self.on_stop();
            return;
        }
        if self.end_deadline.is_some_and(|by| now >= by) {
            self.close_with("end-linger-expired", None);
            return;
        }
        self.tick_outage(now);
        if self.done.is_some() {
            return;
        }
        self.feed_client();
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
        if let Some(client) = &self.client {
            if client.go_tx.send(()).is_ok() {
                self.client_idle = false;
            }
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
            self.close_with("give-up", None);
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
        if self.stage != Stage::Linked {
            return;
        }
        if let (Some((target, by)), Some(io)) = (self.end_flush, &self.leg) {
            if io.flushed.load(Ordering::Acquire) >= target || now >= by {
                self.close_with("client-gone", None);
            }
        }
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
            let is_end = self.client_closed && is_end_message(&message);
            match io.tx.try_send(message) {
                Ok(()) => {
                    io.sent += 1;
                    if is_end {
                        self.end_flush = Some((io.sent, now + STOP_END_FLUSH));
                    }
                }
                Err(TrySendError::Full(message)) => {
                    io.pending = Some(message);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.leg_dead("write-failed");
                    return;
                }
            }
        }
    }

    /// A linked leg died (EOF, protocol error, silence or write failure): start resuming.
    fn leg_dead(&mut self, reason: &str) {
        let now = Instant::now();
        self.kill_leg(true);
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
        self.end_flush = None;
        self.schedule_backoff();
    }

    /// `should_stop` is set. With a live leg, tell the keeper the client is gone first.
    fn on_stop(&mut self) {
        let live = self.stage == Stage::Linked && self.endpoint.is_up();
        if live {
            let deadline = Instant::now() + STOP_END_FLUSH;
            self.stop_deadline = Some(deadline);
            let end = self.endpoint.end_now(end_reason::CLIENT_GONE);
            if let Some(io) = &mut self.leg {
                io.pending = None;
                let mut target = self.end_flush.map(|(target, _)| target);
                if target.is_none() {
                    let mut message = end;
                    loop {
                        match io.tx.try_send(message) {
                            Ok(()) => {
                                io.sent += 1;
                                target = Some(io.sent);
                                break;
                            }
                            Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                                message = back;
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                        }
                    }
                }
                if let Some(target) = target {
                    while io.flushed.load(Ordering::Acquire) < target && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        }
        self.trace
            .emit("session_end", serde_json::json!({"reason": "stopped"}));
        self.done = Some(Done::Stopped);
    }

    // ---- teardown -------------------------------------------------------------------------

    fn finish(mut self) -> io::Result<()> {
        let done = self.done.take().unwrap_or(Done::Stopped);
        self.cancel.store(true, Ordering::Release);
        let result = match done {
            Done::Plain => {
                self.kill_leg(true);
                self.join_dial();
                self.trace
                    .emit("session_end", serde_json::json!({"reason": "plain-fallback"}));
                return match self.stream.take() {
                    Some(stream) => self.params.plain.run(stream, self.stop),
                    None => Err(io::Error::other("client stream was already taken")),
                };
            }
            Done::Stopped => {
                let deadline = self.stop_deadline.unwrap_or_else(Instant::now);
                self.close_client(false, deadline);
                self.close_leg(deadline);
                Ok(())
            }
            Done::Close {
                reason,
                error,
                final_ack,
            } => {
                self.trace
                    .emit("session_end", serde_json::json!({ "reason": reason }));
                let deadline = Instant::now() + STOP_END_FLUSH;
                // The final ACK goes first: a `--remote` client may exit as soon as its
                // stream closes.
                if final_ack {
                    self.final_ack(deadline);
                }
                self.close_client(true, deadline);
                self.close_leg(deadline);
                match error {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        };
        self.kill_leg(true);
        self.join_dial();
        result
    }

    fn join_dial(&mut self) {
        if let Some(handle) = self.dial_thread.take() {
            let _ = handle.join();
        }
    }

    /// Close the client socket. Graceful: behind every queued frame, unless stopped meanwhile.
    fn close_client(&mut self, graceful: bool, deadline: Instant) {
        let Some(client) = self.client.take() else {
            self.stream = None;
            return;
        };
        if graceful {
            let _ = client.out_tx.send(ClientOut::Close);
            while !client.writer.is_finished()
                && !self.stop.load(Ordering::Acquire)
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(5));
            }
        }
        shutdown_client(&client.shutdown);
        drop(client.go_tx);
        drop(client.out_tx);
        let _ = client.writer.join();
        let _ = client.reader.join();
    }

    /// Tell the keeper everything delivered was written, so it can drop its server-eof session.
    /// The ACK is unconditional; the session is over, so nothing can be replayed.
    fn final_ack(&mut self, deadline: Instant) {
        if !self.endpoint.is_up() {
            return;
        }
        self.endpoint
            .delivered(Instant::now(), self.endpoint.recv_offset());
        let Some(ack) = self.endpoint.ack_now() else {
            return;
        };
        let Some(io) = &mut self.leg else { return };
        let mut message = ack;
        loop {
            match io.tx.try_send(message) {
                Ok(()) => {
                    io.sent += 1;
                    break;
                }
                Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                    message = back;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
        while io.flushed.load(Ordering::Acquire) < io.sent && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
    }
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
    }

    impl Client {
        fn connect(path: &Path) -> Client {
            let stream = ipc::connect_local_stream(path).unwrap();
            let mut reader = stream.try_clone().unwrap();
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || loop {
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
            });
            Client { stream, rx }
        }

        fn write(&mut self, bytes: &[u8]) {
            self.stream.write_all(bytes).unwrap();
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
        /// Decode the session-final END but do not record it, then cut the leg: a leg that
        /// died after the bridge wrote the END.
        swallow_end: bool,
    }

    impl KLeg {
        fn new(answer: Answer) -> KLeg {
            KLeg {
                answer,
                welcome_delay: Duration::ZERO,
                kill_after: None,
                swallow_end: false,
            }
        }
        fn delay(mut self, delay: Duration) -> KLeg {
            self.welcome_delay = delay;
            self
        }
        fn swallow_end(mut self) -> KLeg {
            self.swallow_end = true;
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
        last_rx: Option<Instant>,
        stall: bool,
        kill_now: bool,
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
                                    SessionEvent::PeerEnd(_) if script.swallow_end => eof = true,
                                    SessionEvent::PeerEnd(reason) => {
                                        let len = st.upstream.len();
                                        st.upstream_at_end = Some(len);
                                        st.ends.push(reason);
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
                    let ep = st.endpoint.as_mut().unwrap();
                    for bytes in pending {
                        ep.push_source(&bytes).expect("keeper ring");
                    }
                    if let Some(reason) = final_end {
                        ep.queue_final_end(reason);
                    }
                    ep.delivered(now, ep.recv_offset());
                    while let Some(message) = ep.poll_send(now) {
                        out.push(message);
                    }
                }
                if !eof {
                    for message in out {
                        let take = budget.map_or(message.len(), |left| left.min(message.len()));
                        if s.write_all(&message[..take]).is_err() {
                            break 'pump;
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

    // ---- the in-memory dialer ----------------------------------------------------------------

    enum LegScript {
        Keeper(FakeKeeper, KLeg),
        Fail(LegError),
        Exit { code: i32, stderr: &'static str },
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
                Some(LegScript::Fail(err)) => Err(err),
                Some(LegScript::Exit { code, stderr }) => Ok(Leg::new(
                    Box::new(io::empty()),
                    Box::new(io::sink()),
                    Box::new(move |_| {
                        (Some(ExitStatus::from_raw(code << 8)), Some(stderr.to_owned()))
                    }),
                )),
                Some(LegScript::Keeper(keeper, script)) => {
                    let (local, remote) = UnixStream::pair().unwrap();
                    keeper.spawn_leg(remote, script);
                    let reader = local.try_clone().unwrap();
                    let writer = local.try_clone().unwrap();
                    Ok(Leg::new(
                        Box::new(reader),
                        Box::new(writer),
                        Box::new(move |_| {
                            let _ = local.shutdown(std::net::Shutdown::Both);
                            (None, None)
                        }),
                    ))
                }
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

        fn start_with(
            dialer: &MemDialer,
            grace: Duration,
            plain: PlainFallback,
            trace_dir: Option<PathBuf>,
            path: Option<(PathBuf, PathBuf)>,
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
            if got == Seen::Link(LinkStatus::Linked) {
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
        let (_harness, client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        dialer.block(true);
        keeper.kill_leg();
        client.next_seen(WAIT);
        let mut writer = client.stream.try_clone().unwrap();
        thread::spawn(move || {
            let _ = writer.write_all(&vec![7u8; 9 * 1024 * 1024]);
        });
        let frames = client.until_closed(secs(20));
        assert!(frames.iter().all(|(_, s)| is_resuming(s)), "{frames:?}");
        // A leg that comes up afterwards is told why, best effort.
        dialer.keeper(&keeper, KLeg::new(Answer::Resumed));
        dialer.block(false);
        wait_until("END ring-overflow", WAIT, || {
            keeper.ends() == [end_reason::RING_OVERFLOW]
        });
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
        dialer.keeper(&keeper, KLeg::new(Answer::New).swallow_end());
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
    fn client_eof_sends_client_gone_after_ring() {
        let (dialer, keeper) = (MemDialer::new(), FakeKeeper::default());
        let (_harness, mut client) = established(&dialer, &keeper, secs(30), KLeg::new(Answer::New));
        client.write(&vec![5u8; 300 * 1024]);
        client.close();
        wait_until("END", WAIT, || !keeper.ends().is_empty());
        assert_eq!(keeper.ends(), [end_reason::CLIENT_GONE]);
        assert_eq!(keeper.st().upstream_at_end, Some(300 * 1024), "END follows every ring byte");
    }
}
