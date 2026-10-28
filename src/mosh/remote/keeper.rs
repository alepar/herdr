//! The detached process-lifetime keeper: token table, HELLO state table, server
//! connections, rings, drain policy, grace timers, leg stealing, ghost eviction and
//! lock-ordered idle exit. Never parses herdr frames. Filled by mosh-dqw.7.
//!
//! The keeper reads its environment exactly once (`HERDR_MOSH_TRACE`, in `run`). Grace
//! comes only from `HELLO.grace_ms`; the kill switch and link thresholds are never read
//! here. Diagnostics go to the trace file only; stdout and stderr stay silent.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::rendezvous::{self, KeeperRendezvous};
use crate::remote::attach::mosh::session::{
    self, end_reason, End, Hello, HelloAnswer, Role, SessionEndpoint, SessionEvent, SessionMsg,
    Token, Welcome, WelcomeStatus, MAX_CONTROL_PAYLOAD, MAX_DATA_CHUNK, PRE_WELCOME_BOUND,
    SESSION_PROTOCOL_VERSION,
};
use crate::remote::attach::mosh::window::WindowSnapshot;

/// Bytes the server wrote that the keeper has not read: FIONREAD on the server stream, the
/// same quantity as `ss` Recv-Q. Saturates near the server socket's send buffer. `None` if
/// the ioctl fails.
fn server_backlog(server: &UnixStream) -> Option<u64> {
    let mut queued: libc::c_int = 0;
    // SAFETY: FIONREAD stores one c_int through the pointer; the fd is open for the borrow.
    let rc = unsafe { libc::ioctl(server.as_raw_fd(), libc::FIONREAD, &mut queued) };
    (rc == 0).then(|| u64::try_from(queued).unwrap_or(0))
}

/// Outbound (server to client) replay ring capacity.
const KEEPER_RING_CAP: usize = 16 * 1024 * 1024;
/// Keeper exits after the token table has been empty this long.
const IDLE_EXIT_AFTER: Duration = Duration::from_secs(30);
/// Main-loop poll period (accept, timers, idle check).
const POLL: Duration = Duration::from_millis(50);
/// Largest back-off between accept attempts after a non-fatal accept error.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
/// The one Closing deadline: how long `server_writer` may drain `to_server` into a stalled
/// server before the server stream is shut down.
const CLOSE_BUDGET: Duration = Duration::from_millis(500);

/// `args` is the slice `remote::dispatch` receives (the `remote-client-bridge` arguments).
pub(super) fn run(args: &[String]) -> io::Result<()> {
    if args != [crate::remote::attach::mosh::KEEPER_FLAG] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "keeper expects exactly the keeper flag",
        ));
    }
    let session = crate::session::active_name()
        .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
    serve(KeeperConfig {
        rendezvous: rendezvous::resolve(&session),
        server_socket: crate::server::socket_paths::client_socket_path(),
        idle_after: IDLE_EXIT_AFTER,
        trace_dir: std::env::var_os("HERDR_MOSH_TRACE").map(PathBuf::from),
        #[cfg(test)]
        before_drain: None,
        #[cfg(test)]
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        #[cfg(test)]
        accept_fault: None,
    })
}

struct KeeperConfig {
    rendezvous: KeeperRendezvous,
    server_socket: PathBuf,
    idle_after: Duration,
    trace_dir: Option<PathBuf>,
    /// Test seam: runs while the idle exit holds the spawn lock, before the final accept.
    #[cfg(test)]
    before_drain: Option<Box<dyn Fn() + Send + Sync>>,
    /// Test seam: lets a test end the main loop.
    #[cfg(test)]
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// Test seam: consulted before each accept; `Some(err)` is treated as the accept's error.
    #[cfg(test)]
    accept_fault: Option<Arc<dyn Fn() -> Option<io::Error> + Send + Sync>>,
}

struct Shared {
    table: Mutex<Table>,
    kick: Condvar,
    server_socket: PathBuf,
    trace_dir: Option<PathBuf>,
    next_id: AtomicU64,
    /// Accepted connections whose HELLO is not yet answered; they defer the idle exit.
    pending: AtomicUsize,
}

struct Table {
    sessions: HashMap<Token, Session>,
    empty_since: Option<Instant>,
}

struct Session {
    /// Distinguishes a session from a later one reusing its token (thread exit check).
    gen: u64,
    endpoint: SessionEndpoint,
    /// Kept for shutdown; the reader and writer threads own clones.
    server: UnixStream,
    /// Bytes for the server with the stream offset reached once they are written.
    to_server: mpsc::Sender<(Vec<u8>, u64)>,
    leg: Option<LegSlot>,
    grace: Duration,
    grace_deadline: Option<Instant>,
    /// The server closed its end; the final END is queued on the endpoint.
    eof: bool,
    /// The final END was written to the current leg.
    end_sent: bool,
}

struct LegSlot {
    id: u64,
    /// Clone, for shutdown on steal, verdict or end.
    stream: UnixStream,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn trace(&self, event: &str, token: Token, fields: Value) {
        self.write_trace(json!({"event": event, "token": token.to_string()}), fields);
    }

    /// One `window` line: the controller's state at an epoch close (or its reset value on
    /// leg up) and the server stream's backlog. Measures nothing without a trace dir.
    fn trace_window(
        &self,
        token: Token,
        leg: u64,
        snapshot: WindowSnapshot,
        server: &UnixStream,
    ) {
        if self.trace_dir.is_none() {
            return;
        }
        let ms = |rtt: Option<Duration>| rtt.map(|d| d.as_secs_f64() * 1000.0);
        self.trace(
            "window",
            token,
            json!({
                "leg": leg,
                "window": snapshot.window,
                "base_rtt": ms(snapshot.base_rtt),
                "epoch_min": ms(snapshot.epoch_min),
                "server_backlog": server_backlog(server),
            }),
        );
    }

    /// A trace line that belongs to the keeper, not to a session: no `token`.
    fn trace_keeper(&self, event: &str, fields: Value) {
        self.write_trace(json!({"event": event}), fields);
    }

    fn write_trace(&self, mut line: Value, fields: Value) {
        let Some(dir) = &self.trace_dir else { return };
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        if let (Some(line), Some(extra)) = (line.as_object_mut(), fields.as_object()) {
            line.insert("ms".to_owned(), json!(ms));
            line.extend(extra.clone());
        }
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("keeper.jsonl"))
            .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
    }

    /// The one Closing path. Forgets the session, which drops the `to_server` sender, so
    /// `server_writer` drains what is queued and then shuts the server stream. The drain is
    /// bounded by `CLOSE_BUDGET`: the write timeout stops a write into a stalled server and
    /// the backstop shuts the stream regardless. The leg is shut at once. No tombstone: a
    /// later resume of the token answers `unknown`.
    fn close_session(&self, table: &mut Table, token: Token, reason: &str) {
        if let Some(session) = table.sessions.remove(&token) {
            if let Some(leg) = session.leg {
                let _ = leg.stream.shutdown(Shutdown::Both);
            }
            let _ = session.server.set_write_timeout(Some(CLOSE_BUDGET));
            let server = session.server;
            thread::spawn(move || {
                thread::sleep(CLOSE_BUDGET);
                let _ = server.shutdown(Shutdown::Both);
            });
            self.trace("session_end", token, json!({"reason": reason}));
        }
        self.kick.notify_all();
    }

    /// The one leg-down reset: the leg is gone (or replaced); rewind the endpoint and start
    /// the grace. Callers decide removal with `should_remove` first.
    fn detach_leg(&self, session: &mut Session, token: Token, now: Instant, reason: &str) {
        if let Some(leg) = session.leg.take() {
            let _ = leg.stream.shutdown(Shutdown::Both);
        }
        session.endpoint.leg_down();
        session.end_sent = false;
        session.grace_deadline = Some(now + session.grace);
        self.trace("leg_down", token, json!({"reason": reason}));
        self.kick.notify_all();
    }

    /// The leg `id` is gone: remove the session if it is finished, else park it. No-op when a
    /// newer leg has replaced it.
    fn drop_leg(&self, table: &mut Table, token: Token, id: u64, reason: &str) {
        let Some(session) = table.sessions.get_mut(&token) else {
            return;
        };
        if session.leg.as_ref().map(|leg| leg.id) != Some(id) {
            return;
        }
        let now = Instant::now();
        if let Some(why) = should_remove(session, now) {
            // The local end acked everything and hung up before the next timer pass.
            self.close_session(table, token, why);
            return;
        }
        self.detach_leg(session, token, now, reason);
    }
}

/// The one removal predicate, evaluated before any per-leg state is reset: the grace has
/// passed, or the server closed and the local end has been sent the END and acked everything.
fn should_remove(session: &Session, now: Instant) -> Option<&'static str> {
    if session
        .grace_deadline
        .is_some_and(|deadline| now >= deadline)
    {
        Some("grace")
    } else if session.eof && session.end_sent && session.endpoint.fully_acked() {
        Some(end_reason::SERVER_EOF)
    } else {
        None
    }
}

/// True only for an error that means the listener itself can no longer work. Anything else
/// (EMFILE, ENFILE, ECONNABORTED, ...) is transient and the keeper keeps serving.
fn accept_error_is_fatal(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::EBADF | libc::EINVAL | libc::ENOTSOCK | libc::EOPNOTSUPP)
    )
}

fn next_backoff(backoff: Duration) -> Duration {
    (backoff * 2).min(ACCEPT_BACKOFF_MAX)
}

fn accept_once(
    config: &KeeperConfig,
    listener: &UnixListener,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    #[cfg(test)]
    if let Some(err) = config.accept_fault.as_ref().and_then(|fault| fault()) {
        return Err(err);
    }
    let _ = config;
    listener.accept()
}

fn serve(config: KeeperConfig) -> io::Result<()> {
    let rdv = &config.rendezvous;
    rendezvous::ensure_private_dir(&rdv.dir)?;
    let live = match rendezvous::try_lock(&rdv.live_lock)? {
        Some(lock) => lock,
        None => return Ok(()),
    };
    // The live lock proves no other keeper owns the socket path.
    let _ = fs::remove_file(&rdv.socket);
    let listener = UnixListener::bind(&rdv.socket)?;
    crate::ipc::restrict_socket_permissions(&rdv.socket, 0o600)?;
    listener.set_nonblocking(true)?;

    let shared = Arc::new(Shared {
        table: Mutex::new(Table {
            sessions: HashMap::new(),
            empty_since: None,
        }),
        kick: Condvar::new(),
        server_socket: config.server_socket.clone(),
        trace_dir: config.trace_dir.clone(),
        next_id: AtomicU64::new(1),
        pending: AtomicUsize::new(0),
    });

    let mut backoff = POLL;
    loop {
        #[cfg(test)]
        if config.stop.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&rdv.socket);
            return Ok(());
        }
        loop {
            match accept_once(&config, &listener) {
                Ok((stream, _)) => {
                    backoff = POLL;
                    spawn_handler(&shared, stream);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if accept_error_is_fatal(&err) => return Err(err),
                Err(err) => {
                    // The pending connection stays in the backlog, so retrying at once would
                    // spin: back off, and keep every session alive meanwhile.
                    shared.trace_keeper("accept_error", json!({"error": err.to_string()}));
                    thread::sleep(backoff);
                    backoff = next_backoff(backoff);
                    break;
                }
            }
        }
        let now = Instant::now();
        let idle = {
            let mut table = shared.lock();
            run_timers(&shared, &mut table, now);
            if table.sessions.is_empty() && shared.pending.load(Ordering::SeqCst) == 0 {
                let since = *table.empty_since.get_or_insert(now);
                now.saturating_duration_since(since) >= config.idle_after
            } else {
                table.empty_since = None;
                false
            }
        };
        if idle {
            // Lock order: take `.spawn`, drain the backlog, then release `.live` before `.spawn`.
            let Some(spawn) = rendezvous::try_lock(&rdv.spawn_lock)? else {
                shared.lock().empty_since = Some(Instant::now());
                continue;
            };
            #[cfg(test)]
            if let Some(hook) = &config.before_drain {
                hook();
            }
            match accept_once(&config, &listener) {
                Ok((stream, _)) => {
                    drop(spawn);
                    backoff = POLL;
                    spawn_handler(&shared, stream);
                    shared.lock().empty_since = Some(Instant::now());
                    continue;
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) if accept_error_is_fatal(&err) => return Err(err),
                Err(err) => {
                    drop(spawn);
                    shared.trace_keeper("accept_error", json!({"error": err.to_string()}));
                    thread::sleep(backoff);
                    backoff = next_backoff(backoff);
                    shared.lock().empty_since = Some(Instant::now());
                    continue;
                }
            }
            let busy = !shared.lock().sessions.is_empty() || shared.pending.load(Ordering::SeqCst) > 0;
            if busy {
                drop(spawn);
                shared.lock().empty_since = Some(Instant::now());
                continue;
            }
            drop(listener);
            let _ = fs::remove_file(&rdv.socket);
            drop(live);
            drop(spawn);
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

/// Link verdicts, grace expiry and finished eof sessions. Runs from the main loop under the lock.
fn run_timers(shared: &Shared, table: &mut Table, now: Instant) {
    let tokens: Vec<Token> = table.sessions.keys().copied().collect();
    for token in tokens {
        let Some(session) = table.sessions.get_mut(&token) else {
            continue;
        };
        if let Some(why) = should_remove(session, now) {
            shared.close_session(table, token, why);
        } else if session.leg.is_some() && session.endpoint.poll_timers(now).is_some() {
            shared.detach_leg(session, token, now, "silence");
        }
    }
}

fn spawn_handler(shared: &Arc<Shared>, stream: UnixStream) {
    shared.pending.fetch_add(1, Ordering::SeqCst);
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let admitted = admit(&shared, stream);
        shared.pending.fetch_sub(1, Ordering::SeqCst);
        if let Ok(Some(admitted)) = admitted {
            run_leg(&shared, admitted);
        }
    });
}

/// A leg that passed the HELLO table and is attached to a session.
struct Admitted {
    stream: UnixStream,
    token: Token,
    id: u64,
    status: WelcomeStatus,
    welcome: Vec<u8>,
}

/// Reads exactly one message, which must be a HELLO. Consumes nothing after it.
fn read_hello(stream: &mut UnixStream) -> io::Result<Hello> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let mut header = [0u8; 5];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_CONTROL_PAYLOAD {
        return Err(bad("HELLO too long"));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    let mut decoder = session::Decoder::default();
    decoder.push(&header);
    decoder.push(&payload);
    match decoder.next() {
        Ok(Some(SessionMsg::Hello(hello))) => Ok(hello),
        _ => Err(bad("first message is not a HELLO")),
    }
}

/// Write an answer that does not bring a leg up, then close the leg.
fn answer_and_close(mut stream: UnixStream, bytes: &[u8]) {
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn end_bytes(reason: &str) -> Vec<u8> {
    SessionMsg::End(End {
        reason: reason.to_string(),
    })
    .encode()
}

fn is_end(message: &[u8]) -> bool {
    message.first() == end_bytes("").first()
}

/// Apply the per-HELLO table. `Ok(None)`: the leg was answered and closed.
fn admit(shared: &Arc<Shared>, mut stream: UnixStream) -> io::Result<Option<Admitted>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(PRE_WELCOME_BOUND))?;
    let hello = read_hello(&mut stream)?;
    stream.set_read_timeout(None)?;
    let token = hello.token;
    let refuse = |stream: UnixStream, reason: &str| {
        shared.trace("welcome", token, json!({"status": "refused", "reason": reason}));
        answer_and_close(stream, &end_bytes(reason));
        Ok(None)
    };

    let mut table = shared.lock();
    if let Some(previous) = hello.previous_token {
        if previous != token && table.sessions.contains_key(&previous) {
            shared.close_session(&mut table, previous, "evicted");
        }
    }
    match session::answer_hello(&hello, table.sessions.contains_key(&token)) {
        HelloAnswer::RefuseVersion => {
            drop(table);
            refuse(stream, end_reason::PROTOCOL_VERSION)
        }
        HelloAnswer::RefuseTokenInUse => {
            drop(table);
            refuse(stream, end_reason::TOKEN_IN_USE)
        }
        HelloAnswer::Welcome(WelcomeStatus::Unknown) => {
            drop(table);
            shared.trace("welcome", token, json!({"status": "unknown"}));
            let unknown = SessionMsg::Welcome(Welcome {
                v: SESSION_PROTOCOL_VERSION,
                status: WelcomeStatus::Unknown,
                recv_offset: 0,
            });
            answer_and_close(stream, &unknown.encode());
            Ok(None)
        }
        HelloAnswer::Welcome(WelcomeStatus::New) => {
            drop(table);
            let server = match UnixStream::connect(&shared.server_socket) {
                Ok(server) => server,
                Err(_) => return refuse(stream, end_reason::SERVER_CONNECT_FAILED),
            };
            let mut table = shared.lock();
            if table.sessions.contains_key(&token) {
                drop(table);
                let _ = server.shutdown(Shutdown::Both);
                return refuse(stream, end_reason::TOKEN_IN_USE);
            }
            let now = Instant::now();
            let mut endpoint = SessionEndpoint::new(Role::Keeper, token, KEEPER_RING_CAP);
            let welcome = match endpoint.welcome(now, WelcomeStatus::New, 0) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(None),
            };
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            let (to_server, from_client) = mpsc::channel();
            table.sessions.insert(
                token,
                Session {
                    gen: id,
                    endpoint,
                    server: server.try_clone()?,
                    to_server,
                    leg: Some(LegSlot {
                        id,
                        stream: stream.try_clone()?,
                    }),
                    grace: Duration::from_millis(u64::from(hello.grace_ms)),
                    grace_deadline: None,
                    eof: false,
                    end_sent: false,
                },
            );
            drop(table);
            shared.trace("server_connect", token, json!({}));
            let reader = server.try_clone()?;
            let (reader_shared, writer_shared) = (Arc::clone(shared), Arc::clone(shared));
            thread::spawn(move || server_reader(&reader_shared, token, id, reader));
            thread::spawn(move || server_writer(&writer_shared, token, id, server, from_client));
            Ok(Some(Admitted {
                stream,
                token,
                id,
                status: WelcomeStatus::New,
                welcome,
            }))
        }
        HelloAnswer::Welcome(WelcomeStatus::Resumed) => {
            let clone = stream.try_clone()?;
            let now = Instant::now();
            let session = table.sessions.get_mut(&token).expect("checked by answer_hello");
            shared.detach_leg(session, token, now, "stolen");
            session.grace = Duration::from_millis(u64::from(hello.grace_ms));
            session.grace_deadline = None;
            let replayed_down = session.endpoint.outbound_end().saturating_sub(hello.recv_offset);
            let welcome = match session
                .endpoint
                .welcome(now, WelcomeStatus::Resumed, hello.recv_offset)
            {
                Ok(bytes) => bytes,
                Err(_) => {
                    // The peer's offset is not in the ring: leave the session parked.
                    session.grace_deadline = Some(now + session.grace);
                    return Ok(None);
                }
            };
            if session.eof {
                // The endpoint retains the END and re-sends it on this leg; the queue is a no-op
                // unless the session is new.
                session.endpoint.queue_final_end(end_reason::SERVER_EOF);
            }
            let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
            session.leg = Some(LegSlot { id, stream: clone });
            shared.trace(
                "resume",
                token,
                json!({"replayed_down": replayed_down, "replayed_up": 0}),
            );
            shared.kick.notify_all();
            Ok(Some(Admitted {
                stream,
                token,
                id,
                status: WelcomeStatus::Resumed,
                welcome,
            }))
        }
    }
}

/// Answer WELCOME, then pump: a writer thread for the leg and this thread as its reader.
fn run_leg(shared: &Arc<Shared>, admitted: Admitted) {
    let Admitted {
        mut stream,
        token,
        id,
        status,
        welcome,
    } = admitted;
    if stream.write_all(&welcome).is_err() {
        let mut table = shared.lock();
        shared.drop_leg(&mut table, token, id, "write");
        return;
    }
    let status_name = if status == WelcomeStatus::New { "new" } else { "resumed" };
    shared.trace("welcome", token, json!({"status": status_name}));
    shared.trace("leg_up", token, json!({}));
    {
        let table = shared.lock();
        if let Some(session) = table
            .sessions
            .get(&token)
            .filter(|s| s.leg.as_ref().map(|leg| leg.id) == Some(id))
        {
            let reset = WindowSnapshot {
                window: session.endpoint.window(),
                base_rtt: None,
                epoch_min: None,
            };
            shared.trace_window(token, id, reset, &session.server);
        }
    }
    if let Ok(writer) = stream.try_clone() {
        let shared = Arc::clone(shared);
        thread::spawn(move || leg_writer(&shared, token, id, writer));
    }

    let mut buf = vec![0u8; MAX_DATA_CHUNK + 64];
    let reason = loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => break "eof",
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break "read",
        };
        let mut table = shared.lock();
        let Some(session) = table.sessions.get_mut(&token) else {
            return;
        };
        if session.leg.as_ref().map(|leg| leg.id) != Some(id) {
            return;
        }
        let now = Instant::now();
        let mut offset = session.endpoint.recv_offset();
        let (events, failed) = match session.endpoint.on_leg_bytes(now, &buf[..n]) {
            Ok(events) => (events, false),
            // Deliver what decoded before the error: recv_offset already counts it.
            Err(err) => (err.delivered, true),
        };
        if let Some(snapshot) = session.endpoint.take_window_snapshot() {
            shared.trace_window(token, id, snapshot, &session.server);
        }
        let mut ended = None;
        for event in events {
            match event {
                SessionEvent::Deliver(bytes) => {
                    offset += bytes.len() as u64;
                    let _ = session.to_server.send((bytes, offset));
                }
                SessionEvent::PeerEnd(reason) => {
                    ended = Some(reason);
                    break;
                }
                _ => {}
            }
        }
        if let Some(reason) = ended {
            shared.close_session(&mut table, token, &reason);
            return;
        }
        if failed {
            break "protocol";
        }
        shared.kick.notify_all();
    };
    let mut table = shared.lock();
    shared.drop_leg(&mut table, token, id, reason);
}

/// Writes whatever the endpoint wants sent on leg `id` until the leg is replaced or gone.
fn leg_writer(shared: &Shared, token: Token, id: u64, mut stream: UnixStream) {
    loop {
        let (message, is_final) = {
            let mut table = shared.lock();
            loop {
                let Some(session) = table.sessions.get_mut(&token) else {
                    return;
                };
                if session.leg.as_ref().map(|leg| leg.id) != Some(id) {
                    return;
                }
                let now = Instant::now();
                if let Some(message) = session.endpoint.poll_send(now) {
                    let is_final = is_end(&message);
                    break (message, is_final);
                }
                let wait = session
                    .endpoint
                    .next_deadline(now)
                    .map_or(POLL, |at| at.saturating_duration_since(now))
                    .clamp(Duration::from_millis(1), POLL);
                table = shared
                    .kick
                    .wait_timeout(table, wait)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        };
        if stream.write_all(&message).is_err() {
            let _ = stream.shutdown(Shutdown::Both);
            return;
        }
        if is_final {
            // Sent only once the write succeeded, and only if this is still the live leg.
            let mut table = shared.lock();
            if let Some(session) = table.sessions.get_mut(&token) {
                if session.leg.as_ref().map(|leg| leg.id) == Some(id) {
                    session.end_sent = true;
                }
            }
        }
    }
}

/// Reads the server into the ring whenever the endpoint's reading rule allows, sized to the
/// endpoint's room.
fn server_reader(shared: &Shared, token: Token, gen: u64, mut server: UnixStream) {
    let mut buf = vec![0u8; MAX_DATA_CHUNK];
    loop {
        let want = {
            let mut table = shared.lock();
            loop {
                match table.sessions.get(&token) {
                    Some(session) if session.gen == gen => {
                        let size = session.endpoint.source_read_size();
                        if size > 0 {
                            break size;
                        }
                    }
                    _ => return,
                }
                table = shared.kick.wait(table).unwrap_or_else(|e| e.into_inner());
            }
        };
        let n = match server.read(&mut buf[..want]) {
            Ok(n) => n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
        let mut table = shared.lock();
        let Some(session) = table.sessions.get_mut(&token).filter(|s| s.gen == gen) else {
            return;
        };
        if n == 0 {
            session.eof = true;
            session.endpoint.queue_final_end(end_reason::SERVER_EOF);
            shared.kick.notify_all();
            return;
        }
        if session.endpoint.push_source(&buf[..n]).is_err() {
            shared.close_session(&mut table, token, end_reason::RING_OVERFLOW);
            return;
        }
        shared.kick.notify_all();
    }
}

/// Writes client bytes to the server; acks are released only after the write.
fn server_writer(
    shared: &Shared,
    token: Token,
    gen: u64,
    mut server: UnixStream,
    from_client: mpsc::Receiver<(Vec<u8>, u64)>,
) {
    for (bytes, offset) in from_client {
        if server.write_all(&bytes).is_err() {
            let _ = server.shutdown(Shutdown::Both);
            return;
        }
        let mut table = shared.lock();
        if let Some(session) = table.sessions.get_mut(&token).filter(|s| s.gen == gen) {
            session.endpoint.delivered(Instant::now(), offset);
        }
        shared.kick.notify_all();
    }
    // The sender is gone (the session closed) and the queue is drained: close the server.
    let _ = server.shutdown(Shutdown::Both);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::remote::attach::mosh::window::{CAP, FLOOR, INITIAL};
    use std::sync::atomic::AtomicBool;

    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    /// Short base so socket paths fit `sun_path`.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> TestDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("mk-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            crate::platform::create_remote_private_dir(&path).unwrap();
            TestDir(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct ServerConn {
        stream: UnixStream,
        received: Arc<Mutex<Vec<u8>>>,
        eof: Arc<AtomicBool>,
        written: Arc<AtomicUsize>,
    }

    impl ServerConn {
        /// Write on a background thread; `written` counts what the kernel accepted.
        fn write_async(&self, bytes: Vec<u8>) {
            let mut stream = self.stream.try_clone().unwrap();
            let written = Arc::clone(&self.written);
            thread::spawn(move || {
                for block in bytes.chunks(100_003) {
                    if stream.write_all(block).is_err() {
                        return;
                    }
                    written.fetch_add(block.len(), Ordering::SeqCst);
                }
            });
        }

        fn write(&self, bytes: &[u8]) {
            (&self.stream).write_all(bytes).unwrap();
        }

        fn close_write(&self) {
            self.stream.shutdown(Shutdown::Write).unwrap();
        }

        fn received(&self) -> Vec<u8> {
            self.received.lock().unwrap().clone()
        }

        fn saw_eof(&self) -> bool {
            self.eof.load(Ordering::SeqCst)
        }
    }

    /// Stands in for the herdr server: accepts and records connections.
    struct FakeServer {
        conns: Arc<Mutex<Vec<Arc<ServerConn>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl FakeServer {
        fn start(path: &std::path::Path) -> FakeServer {
            let listener = UnixListener::bind(path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let conns: Arc<Mutex<Vec<Arc<ServerConn>>>> = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let (conns2, stop2) = (Arc::clone(&conns), Arc::clone(&stop));
            let thread = thread::spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let conn = Arc::new(ServerConn {
                                stream: stream.try_clone().unwrap(),
                                received: Arc::default(),
                                eof: Arc::default(),
                                written: Arc::default(),
                            });
                            let reader = Arc::clone(&conn);
                            let mut stream = stream;
                            thread::spawn(move || {
                                let mut buf = [0u8; 8192];
                                loop {
                                    match stream.read(&mut buf) {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => {
                                            reader.received.lock().unwrap().extend(&buf[..n])
                                        }
                                    }
                                }
                                reader.eof.store(true, Ordering::SeqCst);
                            });
                            conns2.lock().unwrap().push(conn);
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            FakeServer {
                conns,
                stop,
                thread: Some(thread),
            }
        }

        fn connections(&self) -> usize {
            self.conns.lock().unwrap().len()
        }

        fn conn(&self, index: usize) -> Arc<ServerConn> {
            assert!(
                wait_until(Duration::from_secs(5), || self.connections() > index),
                "server connection {index} never arrived"
            );
            Arc::clone(&self.conns.lock().unwrap()[index])
        }
    }

    impl Drop for FakeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            for conn in self.conns.lock().unwrap().iter() {
                let _ = conn.stream.shutdown(Shutdown::Both);
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct Harness {
        dir: TestDir,
        rdv: KeeperRendezvous,
        trace_dir: PathBuf,
        server: Option<FakeServer>,
        stop: Arc<AtomicBool>,
        keeper: Option<thread::JoinHandle<io::Result<()>>>,
        accept_fault: Option<AcceptFault>,
    }

    type AcceptFault = Arc<dyn Fn() -> Option<io::Error> + Send + Sync>;

    impl Harness {
        fn new() -> Harness {
            Harness::with(true, Duration::from_secs(600), None)
        }

        fn with(
            with_server: bool,
            idle_after: Duration,
            before_drain: Option<Box<dyn Fn() + Send + Sync>>,
        ) -> Harness {
            Harness::with_fault(with_server, idle_after, before_drain, None)
        }

        fn with_fault(
            with_server: bool,
            idle_after: Duration,
            before_drain: Option<Box<dyn Fn() + Send + Sync>>,
            accept_fault: Option<AcceptFault>,
        ) -> Harness {
            let dir = TestDir::new();
            let trace_dir = dir.0.join("trace");
            fs::create_dir(&trace_dir).unwrap();
            let server_path = dir.0.join("server.sock");
            let server = with_server.then(|| FakeServer::start(&server_path));
            let rdv = KeeperRendezvous {
                dir: dir.0.clone(),
                socket: dir.0.join("k.sock"),
                live_lock: dir.0.join("k.live"),
                spawn_lock: dir.0.join("k.spawn"),
            };
            let stop = Arc::new(AtomicBool::new(false));
            let mut harness = Harness {
                dir,
                rdv,
                trace_dir,
                server,
                stop,
                keeper: None,
                accept_fault,
            };
            harness.start_keeper(idle_after, before_drain);
            harness
        }

        fn start_keeper(
            &mut self,
            idle_after: Duration,
            before_drain: Option<Box<dyn Fn() + Send + Sync>>,
        ) {
            let config = KeeperConfig {
                rendezvous: self.rdv.clone(),
                server_socket: self.dir.0.join("server.sock"),
                idle_after,
                trace_dir: Some(self.trace_dir.clone()),
                before_drain,
                stop: Arc::clone(&self.stop),
                accept_fault: self.accept_fault.clone(),
            };
            self.keeper = Some(thread::spawn(move || serve(config)));
            let socket = self.rdv.socket.clone();
            assert!(wait_until(Duration::from_secs(5), || socket.exists()));
        }

        fn server(&self) -> &FakeServer {
            self.server.as_ref().unwrap()
        }

        fn local(&self) -> TestLocal {
            TestLocal::new(self.rdv.socket.clone())
        }

        fn trace(&self) -> Vec<Value> {
            fs::read_to_string(self.trace_dir.join("keeper.jsonl"))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).expect("trace line is JSON"))
                .collect()
        }

        fn trace_has(&self, event: &str, field: &str, value: &str) -> bool {
            self.trace()
                .iter()
                .any(|line| line["event"] == event && (field.is_empty() || line[field] == value))
        }

        fn wait_trace(&self, event: &str, field: &str, value: &str) {
            assert!(
                wait_until(Duration::from_secs(5), || self.trace_has(event, field, value)),
                "no trace {event} {field}={value}; have {:?}",
                self.trace()
            );
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.server.take();
            if let Some(keeper) = self.keeper.take() {
                let _ = keeper.join();
            }
        }
    }

    /// A local end over a real socket, driven by the real `SessionEndpoint`.
    struct TestLocal {
        socket: PathBuf,
        stream: UnixStream,
        endpoint: SessionEndpoint,
        received: Vec<u8>,
        status: Option<WelcomeStatus>,
        refused: bool,
        ended: Option<String>,
        closed: bool,
        auto_ack: bool,
    }

    impl TestLocal {
        fn new(socket: PathBuf) -> TestLocal {
            let stream = UnixStream::connect(&socket).unwrap();
            TestLocal {
                socket,
                stream,
                endpoint: SessionEndpoint::new(Role::Local, Token::generate().unwrap(), 8 << 20),
                received: Vec::new(),
                status: None,
                refused: false,
                ended: None,
                closed: false,
                auto_ack: true,
            }
        }

        fn token(&self) -> Token {
            self.endpoint.token()
        }

        /// Start a leg with the endpoint's own HELLO (resume is true after the first WELCOME).
        fn start_leg(&mut self, grace_ms: u32, previous: Option<Token>) {
            self.start_leg_with(grace_ms, previous, |_| {});
        }

        /// As `start_leg`, but `edit` may rewrite the HELLO before it is sent.
        fn start_leg_with(
            &mut self,
            grace_ms: u32,
            previous: Option<Token>,
            edit: impl FnOnce(&mut Hello),
        ) {
            self.stream = UnixStream::connect(&self.socket).unwrap();
            self.closed = false;
            self.ended = None;
            self.status = None;
            self.refused = false;
            self.endpoint.leg_down();
            let bytes = self.endpoint.hello(Instant::now(), grace_ms, previous);
            let mut decoder = session::Decoder::default();
            decoder.push(&bytes);
            let Some(SessionMsg::Hello(mut hello)) = decoder.next().unwrap() else {
                panic!("endpoint produced a non-HELLO");
            };
            edit(&mut hello);
            self.stream
                .write_all(&SessionMsg::Hello(hello).encode())
                .unwrap();
        }

        /// Drop the connection without a goodbye.
        fn drop_leg(&mut self) {
            let _ = self.stream.shutdown(Shutdown::Both);
            self.closed = true;
        }

        fn pump(&mut self, duration: Duration) {
            let deadline = Instant::now() + duration;
            let mut buf = vec![0u8; 128 * 1024];
            while Instant::now() < deadline && !self.closed {
                let left = deadline.saturating_duration_since(Instant::now());
                self.stream
                    .set_read_timeout(Some(left.clamp(Duration::from_millis(1), Duration::from_millis(20))))
                    .unwrap();
                match self.stream.read(&mut buf) {
                    Ok(0) => self.closed = true,
                    Ok(n) => {
                        let events = self
                            .endpoint
                            .on_leg_bytes(Instant::now(), &buf[..n])
                            .expect("keeper sent a valid stream");
                        for event in events {
                            match event {
                                SessionEvent::Deliver(bytes) => {
                                    self.received.extend(bytes);
                                    if self.auto_ack {
                                        let offset = self.endpoint.recv_offset();
                                        self.endpoint.delivered(Instant::now(), offset);
                                    }
                                }
                                SessionEvent::Welcomed(status) => self.status = Some(status),
                                SessionEvent::ResumeRefused => self.refused = true,
                                SessionEvent::PeerEnd(reason) => {
                                    if reason == end_reason::SERVER_EOF {
                                        // Mirror the product's final ACK on server-eof.
                                        let _ = self.stream.write_all(
                                            &self.endpoint.ack_now().unwrap_or_default(),
                                        );
                                    }
                                    self.ended = Some(reason);
                                }
                                SessionEvent::Hello(_) => panic!("keeper sent HELLO"),
                            }
                        }
                    }
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => self.closed = true,
                }
                self.flush();
            }
        }

        fn flush(&mut self) {
            if self.closed || !self.endpoint.is_up() {
                return;
            }
            while let Some(message) = self.endpoint.poll_send(Instant::now()) {
                if self.stream.write_all(&message).is_err() {
                    self.closed = true;
                    return;
                }
            }
        }

        fn wait(&mut self, timeout: Duration, mut cond: impl FnMut(&TestLocal) -> bool) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if cond(self) {
                    return true;
                }
                self.pump(Duration::from_millis(20));
            }
            cond(self)
        }

        fn wait_welcomed(&mut self, status: WelcomeStatus) {
            assert!(
                self.wait(Duration::from_secs(5), |l| l.status == Some(status)),
                "no WELCOME {status:?}; ended {:?} closed {}",
                self.ended,
                self.closed
            );
        }

        fn wait_received(&mut self, len: usize) {
            let got = self.wait(Duration::from_secs(20), |l| l.received.len() >= len);
            assert!(got, "received {} of {len} bytes", self.received.len());
        }

        fn wait_end(&mut self, reason: &str) {
            assert!(
                self.wait(Duration::from_secs(5), |l| l.ended.is_some()),
                "no END"
            );
            assert_eq!(self.ended.as_deref(), Some(reason));
        }

        fn wait_closed(&mut self) {
            assert!(self.wait(Duration::from_secs(5), |l| l.closed), "leg not closed");
        }

        fn send_data(&mut self, bytes: &[u8]) {
            self.endpoint.push_source(bytes).unwrap();
            self.flush();
        }
    }

    const SMALL: Duration = Duration::from_millis(100);

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn new_session(h: &Harness) -> TestLocal {
        let mut local = h.local();
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::New);
        local
    }

    #[test]
    fn peer_end_drains_to_server_before_shutdown() {
        let h = Harness::new();
        let mut local = new_session(&h);
        // The endpoint's window is smaller than the payload: the final END is queued behind the
        // ring and goes out only as ACKs reopen the window (pumped by `wait`).
        local.endpoint.queue_final_end(end_reason::CLIENT_GONE);
        local.send_data(&pattern(1 << 20));
        assert!(local.wait(Duration::from_secs(10), |_| {
            h.server().conn(0).received().len() == 1 << 20 && h.server().conn(0).saw_eof()
        }));
        assert!(h.server().conn(0).received() == pattern(1 << 20));
        h.wait_trace("session_end", "reason", "client-gone");
    }

    #[test]
    fn silence_verdict_does_not_park_a_finished_eof_session() {
        let shared = Shared {
            table: Mutex::new(Table {
                sessions: HashMap::new(),
                empty_since: None,
            }),
            kick: Condvar::new(),
            server_socket: PathBuf::new(),
            trace_dir: None,
            next_id: AtomicU64::new(1),
            pending: AtomicUsize::new(0),
        };
        let now = Instant::now();
        let token = Token::generate().unwrap();
        let mut endpoint = SessionEndpoint::new(Role::Keeper, token, KEEPER_RING_CAP);
        endpoint.welcome(now, WelcomeStatus::New, 0).unwrap();
        assert!(endpoint.fully_acked());
        let (leg, _leg_peer) = UnixStream::pair().unwrap();
        let (server, _server_peer) = UnixStream::pair().unwrap();
        let (to_server, _from_client) = mpsc::channel();
        let mut table = shared.lock();
        table.sessions.insert(
            token,
            Session {
                gen: 1,
                endpoint,
                server,
                to_server,
                leg: Some(LegSlot { id: 1, stream: leg }),
                grace: Duration::from_secs(60),
                grace_deadline: None,
                eof: true,
                end_sent: true,
            },
        );
        run_timers(
            &shared,
            &mut table,
            now + session::SILENCE_TIMEOUT + Duration::from_secs(1),
        );
        assert!(table.sessions.is_empty(), "finished eof session was parked");
    }

    #[test]
    fn accept_error_classification() {
        let err = |code| io::Error::from_raw_os_error(code);
        for code in [libc::EMFILE, libc::ENFILE, libc::ECONNABORTED] {
            assert!(!accept_error_is_fatal(&err(code)), "errno {code} is transient");
        }
        for code in [libc::EBADF, libc::EINVAL] {
            assert!(accept_error_is_fatal(&err(code)), "errno {code} is fatal");
        }
    }

    #[test]
    fn accept_error_backs_off_and_keeps_serving() {
        let faults = Arc::new(AtomicUsize::new(0));
        let armed = Arc::clone(&faults);
        let fault: AcceptFault = Arc::new(move || {
            armed
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .ok()
                .map(|_| io::Error::from_raw_os_error(libc::EMFILE))
        });
        let h = Harness::with_fault(true, Duration::from_secs(600), None, Some(fault));
        let mut live = new_session(&h);
        faults.store(3, Ordering::SeqCst);
        assert!(wait_until(Duration::from_secs(10), || faults.load(Ordering::SeqCst) == 0));
        h.wait_trace("accept_error", "", "");
        h.server().conn(0).write(b"x");
        live.wait_received(1);
        let mut fresh = h.local();
        fresh.start_leg(60_000, None);
        fresh.wait_welcomed(WelcomeStatus::New);
        let errors = h.trace().iter().filter(|l| l["event"] == "accept_error").count();
        assert_eq!(errors, 3);
        assert!(h.keeper.as_ref().is_some_and(|k| !k.is_finished()));
    }

    #[test]
    fn fresh_hello_creates_session_and_welcomes_new() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        assert_eq!(h.server().connections(), 1);
        local.send_data(b"hello server");
        assert!(wait_until(Duration::from_secs(5), || conn.received()
            == b"hello server"));
        conn.write(b"hello client");
        local.wait_received(12);
        assert_eq!(local.received, b"hello client");
        h.wait_trace("welcome", "status", "new");
        h.wait_trace("server_connect", "", "");
        h.wait_trace("leg_up", "", "");
    }

    #[test]
    fn resume_absent_token_answers_unknown_without_server_connect() {
        let h = Harness::new();
        let mut local = h.local();
        local.start_leg_with(60_000, None, |hello| hello.resume = true);
        assert!(local.wait(Duration::from_secs(5), |l| l.refused));
        local.wait_closed();
        assert_eq!(h.server().connections(), 0);
        h.wait_trace("welcome", "status", "unknown");
    }

    #[test]
    fn server_connect_failure_answers_end_and_creates_nothing() {
        let h = Harness::with(false, Duration::from_secs(600), None);
        let mut local = h.local();
        local.start_leg(60_000, None);
        local.wait_end(end_reason::SERVER_CONNECT_FAILED);
        local.wait_closed();
        // Nothing was created: a resume of the same token is unknown.
        let mut again = h.local();
        again.endpoint = SessionEndpoint::new(Role::Local, local.token(), 1024);
        again.start_leg_with(60_000, None, |hello| hello.resume = true);
        assert!(again.wait(Duration::from_secs(5), |l| l.refused));
    }

    #[test]
    fn token_in_use_leaves_session_untouched() {
        let h = Harness::new();
        let mut first = new_session(&h);
        let conn = h.server().conn(0);
        let mut second = h.local();
        second.endpoint = SessionEndpoint::new(Role::Local, first.token(), 1024);
        second.start_leg(60_000, None);
        second.wait_end(end_reason::TOKEN_IN_USE);
        second.wait_closed();
        // The first leg keeps working and no second server connection was made.
        conn.write(b"still here");
        first.wait_received(10);
        assert_eq!(first.received, b"still here");
        assert!(!first.closed);
        assert_eq!(h.server().connections(), 1);
    }

    #[test]
    fn resume_live_steals_leg() {
        let h = Harness::new();
        let mut first = new_session(&h);
        let conn = h.server().conn(0);
        first.pump(SMALL);
        // Same endpoint on a second connection while the first is still up.
        let mut old_stream = first.stream.try_clone().unwrap();
        first.start_leg(60_000, None);
        first.wait_welcomed(WelcomeStatus::Resumed);
        // The old leg reads EOF.
        old_stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 64];
        loop {
            match old_stream.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(err) => panic!("old leg not closed: {err}"),
            }
        }
        conn.write(b"after steal");
        first.wait_received(11);
        assert_eq!(h.server().connections(), 1);
        h.wait_trace("leg_down", "reason", "stolen");
    }

    #[test]
    fn resume_parked_cancels_grace_and_replays() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        local.drop_leg();
        h.wait_trace("leg_down", "", "");
        let data = pattern(100 * 1024);
        conn.write(&data);
        // Wait until the keeper has taken it all (the server write completing is not enough).
        thread::sleep(Duration::from_millis(300));
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::Resumed);
        local.wait_received(data.len());
        assert_eq!(local.received, data);
        h.wait_trace("resume", "", "");
        let resume = h.trace().into_iter().find(|l| l["event"] == "resume").unwrap();
        assert_eq!(resume["replayed_up"], 0);
    }

    #[test]
    fn eof_while_parked_replays_then_end() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        local.drop_leg();
        h.wait_trace("leg_down", "", "");
        conn.write(b"last words");
        conn.close_write();
        thread::sleep(Duration::from_millis(300));
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::Resumed);
        local.wait_end(end_reason::SERVER_EOF);
        assert_eq!(local.received, b"last words");
    }

    #[test]
    fn server_eof_forwards_remaining_bytes_then_end() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        let data = pattern(300_000);
        conn.write(&data);
        conn.close_write();
        local.wait_end(end_reason::SERVER_EOF);
        assert_eq!(local.received, data);
        // The keeper never saw frames: arbitrary bytes pass through untouched. Keep pumping so
        // the local end's final ACK is read and written.
        assert!(
            local.wait(Duration::from_secs(5), |_| h.trace_has("session_end", "", "")),
            "no session_end; trace {:?}",
            h.trace()
        );
    }

    #[test]
    fn protocol_version_refused() {
        let h = Harness::new();
        let mut local = h.local();
        local.start_leg_with(60_000, None, |hello| hello.v = 2);
        local.wait_end(end_reason::PROTOCOL_VERSION);
        local.wait_closed();
        assert_eq!(h.server().connections(), 0);
    }

    #[test]
    fn previous_token_evicts_session() {
        let h = Harness::new();
        let mut ghost = new_session(&h);
        let ghost_conn = h.server().conn(0);
        let mut fresh = h.local();
        fresh.start_leg(60_000, Some(ghost.token()));
        fresh.wait_welcomed(WelcomeStatus::New);
        assert!(wait_until(Duration::from_secs(5), || ghost_conn.saw_eof()));
        h.wait_trace("session_end", "reason", "evicted");
        // The ghost's token is gone: resume is unknown.
        ghost.start_leg(60_000, None);
        assert!(ghost.wait(Duration::from_secs(5), |l| l.refused));
        // A previous_token equal to the token, or unknown, evicts nothing.
        let mut same = h.local();
        let own = same.token();
        same.start_leg(60_000, Some(own));
        same.wait_welcomed(WelcomeStatus::New);
        assert!(!fresh.closed);
    }

    #[test]
    fn keeper_closes_leg_after_any_end() {
        let h = Harness::new();
        let mut version = h.local();
        version.start_leg_with(60_000, None, |hello| hello.v = 9);
        version.wait_end(end_reason::PROTOCOL_VERSION);
        version.wait_closed();

        let mut first = new_session(&h);
        let mut in_use = h.local();
        in_use.endpoint = SessionEndpoint::new(Role::Local, first.token(), 1024);
        in_use.start_leg(60_000, None);
        in_use.wait_end(end_reason::TOKEN_IN_USE);
        in_use.wait_closed();
        first.pump(SMALL);

        let dead = Harness::with(false, Duration::from_secs(600), None);
        let mut failed = dead.local();
        failed.start_leg(60_000, None);
        failed.wait_end(end_reason::SERVER_CONNECT_FAILED);
        failed.wait_closed();
    }

    #[test]
    fn window_limits_server_reads_with_leg_up() {
        let h = Harness::new();
        let mut local = new_session(&h);
        local.auto_ack = false;
        let conn = h.server().conn(0);
        conn.write_async(pattern(8 << 20));
        local.pump(Duration::from_millis(700));
        // With no acks the keeper holds at most the window plus a sub-4 KiB read rounding in its
        // ring. The rest sits in socket buffers on the server side (a few hundred KiB each way;
        // 1 MiB of slack is generous) and the server's writer is blocked well short of 8 MiB.
        let written = conn.written.load(Ordering::SeqCst);
        assert!(written >= INITIAL / 2, "keeper did not read: {written}");
        assert!(
            written <= INITIAL + 4096 + (1 << 20) + 200_000,
            "keeper read past the window: {written}"
        );
        // Acking releases the rest.
        local.auto_ack = true;
        local.endpoint.delivered(Instant::now(), local.endpoint.recv_offset());
        local.wait_received(8 << 20);
        assert_eq!(local.received, pattern(8 << 20));
    }

    #[test]
    fn window_trace_on_leg_up_and_per_epoch_under_flood() {
        let h = Harness::new();
        let mut local = new_session(&h);
        h.wait_trace("window", "", "");
        let windows = |h: &Harness| -> Vec<Value> {
            h.trace().into_iter().filter(|l| l["event"] == "window").collect()
        };
        let first = windows(&h).remove(0);
        assert_eq!(first["window"], INITIAL as u64, "{first}");
        assert!(first["base_rtt"].is_null() && first["epoch_min"].is_null(), "{first}");
        assert!(first["leg"].is_u64() && first["server_backlog"].is_u64(), "{first}");
        let conn = h.server().conn(0);
        conn.write_async(pattern(8 << 20));
        local.wait_received(8 << 20);
        let epochs: Vec<Value> = windows(&h)
            .into_iter()
            .filter(|l| !l["epoch_min"].is_null())
            .collect();
        assert!(!epochs.is_empty(), "no epoch closed under the flood: {:?}", h.trace());
        for line in &epochs {
            for field in ["leg", "window", "base_rtt", "epoch_min", "server_backlog"] {
                assert!(!line[field].is_null(), "{field} missing: {line}");
            }
            let w = line["window"].as_u64().unwrap() as usize;
            assert!((FLOOR..=CAP).contains(&w), "{line}");
            assert_eq!(line["leg"], first["leg"]);
        }
    }

    #[test]
    fn drain_while_down_captures_server_output() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        local.drop_leg();
        h.wait_trace("leg_down", "", "");
        let data = pattern(4 << 20);
        conn.write_async(data.clone());
        assert!(wait_until(Duration::from_secs(20), || conn
            .written
            .load(Ordering::SeqCst)
            == data.len()));
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::Resumed);
        local.wait_received(data.len());
        assert_eq!(local.received, data);
    }

    #[test]
    fn overflow_ends_session_next_resume_unknown() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        local.drop_leg();
        h.wait_trace("leg_down", "", "");
        // 17 MiB against a 16 MiB ring. Block size 100_003 avoids landing exactly on the cap.
        conn.write_async(pattern(17 << 20));
        h.wait_trace("session_end", "reason", end_reason::RING_OVERFLOW);
        assert!(wait_until(Duration::from_secs(5), || conn.saw_eof()));
        local.start_leg(60_000, None);
        assert!(local.wait(Duration::from_secs(5), |l| l.refused));
    }

    #[test]
    fn grace_expiry_closes_server_stream() {
        let h = Harness::new();
        let mut local = h.local();
        local.start_leg(300, None);
        local.wait_welcomed(WelcomeStatus::New);
        let conn = h.server().conn(0);
        local.drop_leg();
        assert!(wait_until(Duration::from_millis(1500), || conn.saw_eof()));
        h.wait_trace("session_end", "reason", "grace");
        local.start_leg(300, None);
        assert!(local.wait(Duration::from_secs(5), |l| l.refused));
    }

    #[test]
    fn hello_grace_wins_over_env() {
        // No keeper code reads this variable; the HELLO value must decide.
        std::env::set_var("HERDR_MOSH_GRACE", "3600");
        let h = Harness::new();
        let started = Instant::now();
        let mut local = h.local();
        local.start_leg(300, None);
        local.wait_welcomed(WelcomeStatus::New);
        let conn = h.server().conn(0);
        local.drop_leg();
        assert!(wait_until(Duration::from_millis(1500), || conn.saw_eof()));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn idle_exit_deferred_by_held_spawn_lock() {
        let h = Harness::with(true, Duration::from_millis(200), None);
        let spawn = rendezvous::lock(&h.rdv.spawn_lock).unwrap();
        thread::sleep(Duration::from_millis(800));
        assert!(!h.keeper.as_ref().unwrap().is_finished());
        // Still connectable while the exit is deferred.
        let mut local = h.local();
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::New);
        local.drop_leg();
        drop(spawn);
        // The table is non-empty (parked within its 60 s grace), so the keeper stays up.
        thread::sleep(Duration::from_millis(600));
        assert!(!h.keeper.as_ref().unwrap().is_finished());
    }

    #[test]
    fn idle_exit_after_spawn_lock_release() {
        let mut h = Harness::with(true, Duration::from_millis(200), None);
        let spawn = rendezvous::lock(&h.rdv.spawn_lock).unwrap();
        thread::sleep(Duration::from_millis(600));
        assert!(!h.keeper.as_ref().unwrap().is_finished());
        drop(spawn);
        let keeper = h.keeper.take().unwrap();
        assert!(wait_until(Duration::from_millis(1500), || keeper.is_finished()));
        keeper.join().unwrap().unwrap();
        assert!(!h.rdv.socket.exists());
    }

    #[test]
    fn idle_exit_serves_connection_in_backlog() {
        let socket = Arc::new(Mutex::new(None::<PathBuf>));
        let hello_stream = Arc::new(Mutex::new(None::<UnixStream>));
        let (socket2, stream2) = (Arc::clone(&socket), Arc::clone(&hello_stream));
        let hook: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            let mut slot = stream2.lock().unwrap();
            if slot.is_some() {
                return;
            }
            let path = socket2.lock().unwrap().clone().unwrap();
            let mut stream = UnixStream::connect(path).unwrap();
            let mut endpoint = SessionEndpoint::new(Role::Local, Token::generate().unwrap(), 1024);
            stream
                .write_all(&endpoint.hello(Instant::now(), 60_000, None))
                .unwrap();
            *slot = Some(stream);
        });
        let h = Harness::with(true, Duration::from_millis(200), Some(hook));
        *socket.lock().unwrap() = Some(h.rdv.socket.clone());
        // The hook connects at the first drain; that connection is served.
        h.wait_trace("welcome", "status", "new");
        thread::sleep(Duration::from_millis(500));
        assert!(!h.keeper.as_ref().unwrap().is_finished());
        assert_eq!(h.server().connections(), 1);
    }

    #[test]
    fn attacher_after_exit_gets_enoent_and_fresh_keeper_starts() {
        let mut h = Harness::with(true, Duration::from_millis(200), None);
        let keeper = h.keeper.take().unwrap();
        assert!(wait_until(Duration::from_secs(3), || keeper.is_finished()));
        keeper.join().unwrap().unwrap();
        let err = UnixStream::connect(&h.rdv.socket).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ),
            "{err:?}"
        );
        assert!(rendezvous::try_lock(&h.rdv.live_lock).unwrap().is_some());
        h.start_keeper(Duration::from_secs(600), None);
        let mut local = h.local();
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::New);
    }

    #[test]
    fn second_keeper_exits_when_live_lock_held() {
        let h = Harness::new();
        let started = Instant::now();
        let second = serve(KeeperConfig {
            rendezvous: h.rdv.clone(),
            server_socket: h.dir.0.join("server.sock"),
            idle_after: Duration::from_secs(600),
            trace_dir: None,
            before_drain: None,
            stop: Arc::new(AtomicBool::new(false)),
            accept_fault: None,
        });
        assert!(second.is_ok());
        assert!(started.elapsed() < Duration::from_millis(500));
        // The first keeper's socket survived and still serves.
        let mut local = h.local();
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::New);
    }

    #[test]
    fn trace_lines_written() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let _ = h.server().conn(0);
        local.drop_leg();
        h.wait_trace("leg_down", "reason", "eof");
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::Resumed);
        local.send_data(b"x");
        // End the session from the local side.
        let end = SessionMsg::End(End {
            reason: end_reason::CLIENT_GONE.into(),
        });
        local.stream.write_all(&end.encode()).unwrap();
        h.wait_trace("session_end", "reason", end_reason::CLIENT_GONE);
        for event in ["welcome", "server_connect", "leg_up", "leg_down", "resume", "session_end"] {
            assert!(h.trace_has(event, "", ""), "missing {event}");
        }
        for line in h.trace() {
            assert!(line["event"].is_string());
            assert_eq!(line["token"].as_str().unwrap().len(), 32);
            assert!(line["ms"].as_u64().unwrap() > 1_600_000_000_000);
        }
    }

    #[test]
    fn keeper_reads_only_trace_env() {
        let source = include_str!("keeper.rs");
        let production = source.split("#[cfg(all(test, unix))]").next().unwrap();
        let needle = concat!("std::env::", "var");
        assert_eq!(production.matches(needle).count(), 1);
        assert!(production.contains("HERDR_MOSH_TRACE"));
    }

    #[test]
    fn run_rejects_other_arguments() {
        let err = run(&["--something-else".to_string()]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn valid_data_then_gap_in_one_write_reaches_server_once() {
        let h = Harness::new();
        let mut local = new_session(&h);
        let conn = h.server().conn(0);
        // Built from the local endpoint so its offsets stay consistent, then a DATA far past
        // the stream end, all in one write.
        local.endpoint.push_source(b"lead").unwrap();
        let mut buf = Vec::new();
        while let Some(message) = local.endpoint.poll_send(Instant::now()) {
            buf.extend(message);
        }
        buf.extend(SessionMsg::Data { offset: 1000, bytes: b"x".to_vec() }.encode());
        local.stream.write_all(&buf).unwrap();
        h.wait_trace("leg_down", "reason", "protocol");
        // The keeper's recv_offset is 4, matching the local's outbound_end: nothing is replayed.
        local.start_leg(60_000, None);
        local.wait_welcomed(WelcomeStatus::Resumed);
        local.send_data(b"after");
        assert!(
            wait_until(Duration::from_secs(5), || conn.received() == b"leadafter"),
            "server saw {:?}",
            String::from_utf8_lossy(&conn.received())
        );
    }
}
