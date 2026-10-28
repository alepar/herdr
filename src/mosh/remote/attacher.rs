//! Locates or spawns the keeper, verifies the keeper's peer uid, pumps stdio and the keeper
//! socket, and reports its own bring-up failure as a session `END`. Beyond that one `END` it
//! is a dumb pump.

use super::rendezvous::{
    ensure_private_dir, lock, resolve, try_lock, validate_private_dir_for, KeeperRendezvous,
};
use crate::remote::attach::mosh::quic_bootstrap::QuicBootstrap;
use crate::remote::attach::mosh::session::{end_reason, End, SessionMsg, BOOTSTRAP_QUERY};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KEEPER_WAIT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// `idle_timeout`: the caller opted into upstream's 60 s idle watchdog (`--idle-timeout-v1`).
/// Session heartbeats keep a live leg busy; a dead leg's attacher is reaped like a plain bridge.
/// `quic`: the caller passed `--x-mosh-quic-v1`; the attacher prints one bootstrap line first
/// and opens the session connection lazily (see `run_quic_with`).
pub(super) fn run(idle_timeout: bool, quic: bool) -> io::Result<()> {
    let session = crate::session::active_name();
    let rdv = resolve(
        session
            .as_deref()
            .unwrap_or(crate::session::DEFAULT_SESSION_NAME),
    );
    let spawn = || keeper_command(&std::env::current_exe()?, session.as_deref()).spawn().map(drop);
    let connect = || bring_up(&rdv, geteuid(), &mut || spawn());
    // The forwarder reads the same global `io::stdin()`, so what the QUIC path's lock
    // buffers is not lost; both locks are released before forwarding starts.
    let opened = open_session(
        quic,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        connect,
        connect,
    );
    match opened {
        Ok(Some(stream)) => {
            use interprocess::os::unix::uds_local_socket::Stream as UdStream;
            crate::platform::forward_remote_bridge_stdio(
                crate::ipc::LocalStream::UdSocket(UdStream::from(stream)),
                idle_timeout,
            )
        }
        // stdin closed before any byte: nothing to attach.
        Ok(None) => Ok(()),
        Err(err) => {
            let _ = report_unavailable(&mut io::stdout().lock());
            Err(io::Error::new(
                err.kind(),
                format!("mosh keeper unavailable: {err}"),
            ))
        }
    }
}

/// Without `quic`: connect and write nothing, byte-for-byte the pre-QUIC attacher.
fn open_session(
    quic: bool,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    connect_query: impl FnOnce() -> io::Result<UnixStream>,
    connect_session: impl FnOnce() -> io::Result<UnixStream>,
) -> io::Result<Option<UnixStream>> {
    if quic {
        run_quic_with(stdin, stdout, connect_query, connect_session, QUERY_TIMEOUT)
    } else {
        connect_session().map(Some)
    }
}

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound on the keeper's one-line bootstrap reply.
const QUERY_REPLY_MAX: usize = 4096;

/// Asks the keeper for its QUIC bootstrap on a short-lived connection of its own. EOF, error,
/// timeout (`timeout` is a deadline for the whole reply, not per read) or an unparseable
/// reply is `None`. The connection is closed on return, so a late reply never reaches anyone.
fn query_bootstrap(
    connect: impl FnOnce() -> io::Result<UnixStream>,
    timeout: Duration,
) -> Option<QuicBootstrap> {
    let mut stream = connect().ok()?;
    let deadline = Instant::now() + timeout;
    stream.write_all(&[BOOTSTRAP_QUERY]).ok()?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        stream.set_read_timeout(Some(remaining.max(Duration::from_millis(1)))).ok()?;
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) if line.len() < QUERY_REPLY_MAX => line.push(byte[0]),
            Ok(_) => return None,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    QuicBootstrap::parse_reply(std::str::from_utf8(&line).ok()?)
}

/// The `--x-mosh-quic-v1` path up to (not including) the forwarder: query the keeper, write
/// exactly one bootstrap line (`none` on any failure), block for the first stdin chunk, and
/// only then open the session connection and write that chunk to it. `Ok(None)` is stdin EOF
/// before any byte, with no session connection opened. A session bring-up failure is `Err`.
fn run_quic_with(
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    connect_query: impl FnOnce() -> io::Result<UnixStream>,
    connect_session: impl FnOnce() -> io::Result<UnixStream>,
    timeout: Duration,
) -> io::Result<Option<UnixStream>> {
    let bootstrap = query_bootstrap(connect_query, timeout);
    stdout.write_all(QuicBootstrap::encode_line(bootstrap.as_ref()).as_bytes())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    let mut chunk = [0u8; 8192];
    let n = loop {
        match stdin.read(&mut chunk) {
            Ok(n) => break n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    };
    if n == 0 {
        return Ok(None);
    }
    let mut stream = connect_session()?;
    stream.write_all(&chunk[..n])?;
    Ok(Some(stream))
}

fn geteuid() -> u32 {
    // SAFETY: geteuid takes no pointers and has no preconditions.
    unsafe { libc::geteuid() }
}

/// The only message the attacher originates.
fn report_unavailable(out: &mut dyn Write) -> io::Result<()> {
    let end = SessionMsg::End(End {
        reason: end_reason::KEEPER_UNAVAILABLE.into(),
    });
    out.write_all(&end.encode())?;
    out.flush()
}

fn bring_up(
    rdv: &KeeperRendezvous,
    expected_uid: u32,
    spawn: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<UnixStream> {
    bring_up_with(rdv, expected_uid, spawn, KEEPER_WAIT)
}

fn bring_up_with(
    rdv: &KeeperRendezvous,
    expected_uid: u32,
    spawn: &mut dyn FnMut() -> io::Result<()>,
    wait: Duration,
) -> io::Result<UnixStream> {
    ensure_private_dir(&rdv.dir)?;
    validate_private_dir_for(&rdv.dir, expected_uid)?;
    if let Some(stream) = connect_checked(rdv, expected_uid)? {
        return Ok(stream);
    }
    // Held until return: serialises keeper spawning across attachers.
    let _spawn_guard = lock(&rdv.spawn_lock)?;
    if let Some(stream) = connect_checked(rdv, expected_uid)? {
        return Ok(stream);
    }
    let live = match try_lock(&rdv.live_lock)? {
        Some(live) => live,
        None => {
            // A keeper is alive but not listening yet.
            if let Some(stream) = poll_connect(rdv, expected_uid, wait)? {
                return Ok(stream);
            }
            try_lock(&rdv.live_lock)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "keeper holds its lock but never listened",
                )
            })?
        }
    };
    // No keeper alive: clear the stale socket, hand the lock over and spawn.
    match std::fs::remove_file(&rdv.socket) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    drop(live);
    spawn()?;
    poll_connect(rdv, expected_uid, wait)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::TimedOut, "keeper did not start listening")
    })
}

fn poll_connect(
    rdv: &KeeperRendezvous,
    expected_uid: u32,
    wait: Duration,
) -> io::Result<Option<UnixStream>> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(stream) = connect_checked(rdv, expected_uid)? {
            return Ok(Some(stream));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// `Ok(None)` when nothing is listening. A stream is returned only after its peer's uid
/// matches `expected_uid`.
fn connect_checked(rdv: &KeeperRendezvous, expected_uid: u32) -> io::Result<Option<UnixStream>> {
    let stream = match UnixStream::connect(&rdv.socket) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None)
        }
        Err(err) => return Err(err),
    };
    let uid = peer_uid(&stream)?;
    if uid != expected_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("keeper socket is owned by uid {uid}, expected {expected_uid}"),
        ));
    }
    Ok(Some(stream))
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the call; the fd is owned by `stream`.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result == 0 {
        Ok(cred.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    use std::os::fd::AsRawFd as _;

    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both out-pointers are valid for the call; the fd is owned by `stream`.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Mirrors `build_server_daemon_command`: no stdio (so the keeper holds none of the sshd
/// channel fds and its output can never reach the leg), no agent socket, own session.
fn keeper_command(exe: &Path, session: Option<&str>) -> Command {
    let mut command = Command::new(exe);
    if let Some(session) = session {
        command.args(["--session", session]);
    }
    command
        .args(["remote-client-bridge", crate::remote::attach::mosh::KEEPER_FLAG])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("SSH_AUTH_SOCK");
    crate::platform::detach_server_daemon_command(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use super::super::rendezvous::{rendezvous_paths, RendezvousLock};
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    const SHORT: Duration = Duration::from_millis(800);

    struct Env {
        base: std::path::PathBuf,
        rdv: KeeperRendezvous,
    }

    impl Env {
        fn new() -> Env {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            // Fixed short base: a long TMPDIR would push the socket path past
            // sun_path and make `rendezvous_paths` fall back to the host's shared
            // /tmp/herdr-mosh-<euid>, which these tests must never touch.
            let base = std::path::PathBuf::from("/tmp")
                .join(format!("att-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            let rdv = rendezvous_paths(&base, geteuid(), "t");
            assert!(
                rdv.dir.starts_with(&base),
                "test rendezvous escaped its base: {}",
                rdv.dir.display()
            );
            Env { base, rdv }
        }

        fn prepare_dir(&self) {
            ensure_private_dir(&self.rdv.dir).unwrap();
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// Holds the live lock and accepts connections, like a keeper would.
    struct FakeKeeper {
        stop: Arc<AtomicBool>,
        accepted: Arc<Mutex<Vec<UnixStream>>>,
        /// Bytes that arrived on connections whose first byte was not `Q` (query mode only).
        received: Arc<Mutex<Vec<u8>>>,
        handle: Option<std::thread::JoinHandle<()>>,
        _live: Arc<RendezvousLock>,
    }

    /// How a query-mode `FakeKeeper` answers the bootstrap query byte.
    #[derive(Clone)]
    enum QueryReply {
        Reply { delay: Duration, line: String },
        CloseOnQ,
    }

    /// Per-connection handler: `Q` gets `reply`; any other first byte starts a "session"
    /// whose bytes are recorded in `received`.
    fn handle_conn(
        mut stream: UnixStream,
        reply: QueryReply,
        received: Arc<Mutex<Vec<u8>>>,
        stop: Arc<AtomicBool>,
    ) {
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        let mut first = true;
        let mut buf = [0u8; 256];
        while !stop.load(Ordering::Relaxed) {
            match stream.read(&mut buf) {
                Ok(0) => return,
                Ok(n) if first && buf[0] == BOOTSTRAP_QUERY => {
                    match &reply {
                        QueryReply::Reply { delay, line } => {
                            let until = Instant::now() + *delay;
                            while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            let _ = stream.write_all(format!("{line}\n").as_bytes());
                        }
                        QueryReply::CloseOnQ => {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                        }
                    }
                    let _ = n;
                    return;
                }
                Ok(n) => {
                    first = false;
                    received.lock().unwrap().extend_from_slice(&buf[..n]);
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
        }
    }

    impl FakeKeeper {
        fn start(rdv: &KeeperRendezvous) -> FakeKeeper {
            let live = Arc::new(try_lock(&rdv.live_lock).unwrap().expect("live lock free"));
            Self::listen(rdv, live, None)
        }

        /// A keeper that answers the bootstrap query byte like `reply` says.
        fn answering(rdv: &KeeperRendezvous, reply: QueryReply) -> FakeKeeper {
            let live = Arc::new(try_lock(&rdv.live_lock).unwrap().expect("live lock free"));
            Self::listen(rdv, live, Some(reply))
        }

        fn listen(
            rdv: &KeeperRendezvous,
            live: Arc<RendezvousLock>,
            reply: Option<QueryReply>,
        ) -> FakeKeeper {
            let listener = UnixListener::bind(&rdv.socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let accepted = Arc::new(Mutex::new(Vec::new()));
            let received = Arc::new(Mutex::new(Vec::new()));
            let handle = {
                let (stop, accepted, received) = (stop.clone(), accepted.clone(), received.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                if let Some(reply) = &reply {
                                    let clone = stream.try_clone().unwrap();
                                    let (reply, received, stop) =
                                        (reply.clone(), received.clone(), stop.clone());
                                    std::thread::spawn(move || {
                                        handle_conn(clone, reply, received, stop)
                                    });
                                }
                                accepted.lock().unwrap().push(stream);
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                })
            };
            FakeKeeper {
                stop,
                accepted,
                received,
                handle: Some(handle),
                _live: live,
            }
        }

        fn accepted_count(&self) -> usize {
            self.accepted.lock().unwrap().len()
        }
    }

    impl Drop for FakeKeeper {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn wait_accepted(keeper: &FakeKeeper, count: usize) {
        for _ in 0..200 {
            if keeper.accepted_count() >= count {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("keeper accepted {} of {count}", keeper.accepted_count());
    }

    fn sample_bootstrap() -> QuicBootstrap {
        let line = format!("herdr-mosh-quic:1 40123 {} {}", "ab".repeat(32), "cd".repeat(32));
        QuicBootstrap::parse_line(&line).expect("valid sample line")
    }

    fn connect_to(env: &Env) -> impl FnOnce() -> io::Result<UnixStream> + '_ {
        || bring_up(&env.rdv, geteuid(), &mut || panic!("must not spawn"))
    }

    fn wait_received(keeper: &FakeKeeper, want: &[u8]) {
        for _ in 0..200 {
            if keeper.received.lock().unwrap().as_slice() == want {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("keeper received {:?}, wanted {want:?}", keeper.received.lock().unwrap());
    }

    #[test]
    fn no_flag_prints_no_bootstrap_line() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::answering(&env.rdv, QueryReply::CloseOnQ);
        let mut stdout = Vec::new();
        let stream = open_session(
            false,
            &mut io::empty(),
            &mut stdout,
            || panic!("the no-flag path must not query"),
            connect_to(&env),
        )
        .unwrap();
        assert!(stream.is_some());
        assert!(stdout.is_empty(), "stdout: {stdout:?}");
        wait_accepted(&keeper, 1);
    }

    #[test]
    fn flag_prints_exactly_one_line_before_session_bytes() {
        let env = Env::new();
        env.prepare_dir();
        let served = sample_bootstrap();
        let keeper = FakeKeeper::answering(
            &env.rdv,
            QueryReply::Reply {
                delay: Duration::ZERO,
                line: QuicBootstrap::encode_reply(Some(&served)),
            },
        );
        let mut stdout = Vec::new();
        let stream = run_quic_with(
            &mut &b"x"[..],
            &mut stdout,
            connect_to(&env),
            connect_to(&env),
            QUERY_TIMEOUT,
        )
        .unwrap();
        assert!(stream.is_some());
        let text = String::from_utf8(stdout).unwrap();
        assert_eq!(text.matches('\n').count(), 1, "{text:?}");
        assert!(text.ends_with('\n'));
        assert_eq!(QuicBootstrap::parse_line(&text), Some(served));
        wait_received(&keeper, b"x");
    }

    #[test]
    fn withheld_reply_prints_one_none_line() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::answering(
            &env.rdv,
            QueryReply::Reply {
                delay: Duration::from_secs(10),
                line: "{}".into(),
            },
        );
        let mut stdout = Vec::new();
        let started = Instant::now();
        let stream = run_quic_with(
            &mut &b"x"[..],
            &mut stdout,
            connect_to(&env),
            connect_to(&env),
            QUERY_TIMEOUT,
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(elapsed >= QUERY_TIMEOUT, "gave up early: {elapsed:?}");
        assert!(elapsed < QUERY_TIMEOUT + Duration::from_secs(2), "too slow: {elapsed:?}");
        assert!(stream.is_some());
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
        wait_received(&keeper, b"x");
    }

    #[test]
    fn late_reply_prints_none_and_never_reaches_stdout() {
        let env = Env::new();
        env.prepare_dir();
        let served = sample_bootstrap();
        let _keeper = FakeKeeper::answering(
            &env.rdv,
            QueryReply::Reply {
                delay: Duration::from_millis(700),
                line: QuicBootstrap::encode_reply(Some(&served)),
            },
        );
        let mut stdout = Vec::new();
        run_quic_with(
            &mut &b"x"[..],
            &mut stdout,
            connect_to(&env),
            connect_to(&env),
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
        // Past the late reply: the query connection is gone, nothing else was written.
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
    }

    #[test]
    fn trickled_reply_is_bounded_by_one_deadline() {
        let env = Env::new();
        env.prepare_dir();
        let listener = std::os::unix::net::UnixListener::bind(&env.rdv.socket).unwrap();
        let _live = try_lock(&env.rdv.live_lock).unwrap().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let trickler = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let mut q = [0u8; 1];
                let _ = s.read(&mut q);
                // One byte every 100 ms, never a newline: per-read timeouts alone never fire.
                while !stop.load(Ordering::Relaxed) && s.write_all(b" ").is_ok() {
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        };
        let started = Instant::now();
        let got = query_bootstrap(connect_to(&env), Duration::from_millis(400));
        stop.store(true, Ordering::Relaxed);
        trickler.join().unwrap();
        assert_eq!(got, None);
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    }

    #[test]
    fn old_keeper_closing_on_q_yields_none_and_session_works() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::answering(&env.rdv, QueryReply::CloseOnQ);
        let mut stdout = Vec::new();
        let stream = run_quic_with(
            &mut &b"x"[..],
            &mut stdout,
            connect_to(&env),
            connect_to(&env),
            QUERY_TIMEOUT,
        )
        .unwrap();
        assert!(stream.is_some());
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
        wait_received(&keeper, b"x");
    }

    /// Blocks in `read` until the test sends a chunk; EOF once the sender is dropped.
    struct ChannelReader(std::sync::mpsc::Receiver<Vec<u8>>);

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(chunk) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                Err(_) => Ok(0),
            }
        }
    }

    #[test]
    fn no_session_connection_until_first_stdin_byte() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::answering(
            &env.rdv,
            QueryReply::Reply {
                delay: Duration::ZERO,
                line: "{}".into(),
            },
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let mut stdout = Vec::new();
                let stream = run_quic_with(
                    &mut ChannelReader(rx),
                    &mut stdout,
                    connect_to(&env),
                    connect_to(&env),
                    QUERY_TIMEOUT,
                )
                .unwrap();
                (stream.is_some(), stdout)
            });
            wait_accepted(&keeper, 1);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(keeper.accepted_count(), 1, "session connection opened before stdin");
            tx.send(b"x".to_vec()).unwrap();
            wait_accepted(&keeper, 2);
            let (opened, stdout) = worker.join().unwrap();
            assert!(opened);
            assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
        });
        wait_received(&keeper, b"x");
    }

    #[test]
    fn stdin_eof_before_any_byte_opens_no_session() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::answering(
            &env.rdv,
            QueryReply::Reply {
                delay: Duration::ZERO,
                line: "{}".into(),
            },
        );
        let mut stdout = Vec::new();
        let stream = run_quic_with(
            &mut io::empty(),
            &mut stdout,
            connect_to(&env),
            || panic!("no session connection without stdin"),
            QUERY_TIMEOUT,
        )
        .unwrap();
        assert!(stream.is_none());
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
        wait_accepted(&keeper, 1);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(keeper.accepted_count(), 1);
    }

    #[test]
    fn query_bring_up_failure_yields_none_line() {
        let mut stdout = Vec::new();
        let stream = run_quic_with(
            &mut io::empty(),
            &mut stdout,
            || Err(io::Error::other("no keeper")),
            || panic!("no session connection without stdin"),
            QUERY_TIMEOUT,
        )
        .unwrap();
        assert!(stream.is_none());
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
    }

    #[test]
    fn session_bring_up_failure_is_an_error() {
        let mut stdout = Vec::new();
        let result = run_quic_with(
            &mut &b"x"[..],
            &mut stdout,
            || Err(io::Error::other("no keeper")),
            || Err(io::Error::other("no keeper")),
            QUERY_TIMEOUT,
        );
        assert!(result.is_err());
        assert_eq!(stdout, b"herdr-mosh-quic:1 none\n");
    }

    #[test]
    fn stale_socket_recovered() {
        let env = Env::new();
        env.prepare_dir();
        drop(UnixListener::bind(&env.rdv.socket).unwrap());
        assert!(env.rdv.socket.exists());
        let spawns = AtomicU32::new(0);
        let keeper = Mutex::new(None);
        let mut spawn = || {
            spawns.fetch_add(1, Ordering::SeqCst);
            // The stale socket was unlinked before the spawn.
            assert!(!env.rdv.socket.exists());
            *keeper.lock().unwrap() = Some(FakeKeeper::start(&env.rdv));
            Ok(())
        };
        bring_up(&env.rdv, geteuid(), &mut spawn).unwrap();
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        wait_accepted(keeper.lock().unwrap().as_ref().unwrap(), 1);
    }

    #[test]
    fn live_keeper_connected_without_spawning() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::start(&env.rdv);
        let mut spawn = || panic!("must not spawn");
        bring_up(&env.rdv, geteuid(), &mut spawn).unwrap();
        wait_accepted(&keeper, 1);
    }

    #[test]
    fn concurrent_attachers_spawn_one_keeper() {
        let env = Env::new();
        env.prepare_dir();
        let spawns = AtomicU32::new(0);
        let keeper = Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    let mut spawn = || {
                        spawns.fetch_add(1, Ordering::SeqCst);
                        // Slow start, so the second attacher really waits on the spawn lock.
                        std::thread::sleep(Duration::from_millis(200));
                        *keeper.lock().unwrap() = Some(FakeKeeper::start(&env.rdv));
                        Ok(())
                    };
                    bring_up(&env.rdv, geteuid(), &mut spawn).unwrap();
                });
            }
        });
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
        wait_accepted(keeper.lock().unwrap().as_ref().unwrap(), 2);
    }

    #[test]
    fn live_lock_held_not_listening_connects_when_keeper_binds() {
        let env = Env::new();
        env.prepare_dir();
        let live = Arc::new(try_lock(&env.rdv.live_lock).unwrap().unwrap());
        let keeper = Mutex::new(None);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                *keeper.lock().unwrap() = Some(FakeKeeper::listen(&env.rdv, live, None));
            });
            let mut spawn = || panic!("must not spawn");
            bring_up(&env.rdv, geteuid(), &mut spawn).unwrap();
        });
        wait_accepted(keeper.lock().unwrap().as_ref().unwrap(), 1);
    }

    #[test]
    fn live_lock_released_without_listening_relocks_and_spawns() {
        let env = Env::new();
        env.prepare_dir();
        let live = try_lock(&env.rdv.live_lock).unwrap().unwrap();
        let spawns = AtomicU32::new(0);
        let keeper = Mutex::new(None);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                drop(live);
            });
            let mut spawn = || {
                spawns.fetch_add(1, Ordering::SeqCst);
                *keeper.lock().unwrap() = Some(FakeKeeper::start(&env.rdv));
                Ok(())
            };
            bring_up_with(&env.rdv, geteuid(), &mut spawn, SHORT).unwrap();
        });
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn live_lock_held_forever_times_out_without_spawning() {
        let env = Env::new();
        env.prepare_dir();
        let _live = try_lock(&env.rdv.live_lock).unwrap().unwrap();
        let mut spawn = || panic!("must not spawn");
        let err = bring_up_with(&env.rdv, geteuid(), &mut spawn, Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    fn assert_reports_unavailable(result: io::Result<UnixStream>) {
        assert!(result.is_err());
        let mut out = Vec::new();
        report_unavailable(&mut out).unwrap();
        let expected = SessionMsg::End(End {
            reason: "keeper-unavailable".into(),
        })
        .encode();
        assert_eq!(out, expected);
        assert!(String::from_utf8_lossy(&out).contains("\"keeper-unavailable\""));
    }

    #[test]
    fn bad_private_dir_is_rejected_before_any_connect() {
        let mut spawn = || panic!("must not spawn");

        let mode = Env::new();
        mode.prepare_dir();
        std::fs::set_permissions(&mode.rdv.dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let keeper = FakeKeeper::start(&mode.rdv);
        let result = bring_up(&mode.rdv, geteuid(), &mut spawn);
        assert_eq!(result.as_ref().err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_reports_unavailable(result);
        assert_eq!(keeper.accepted_count(), 0);

        let linked = Env::new();
        let target = linked.base.join("real");
        ensure_private_dir(&target).unwrap();
        symlink(&target, &linked.rdv.dir).unwrap();
        let result = bring_up(&linked.rdv, geteuid(), &mut spawn);
        assert_eq!(result.as_ref().err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_reports_unavailable(result);

        let foreign = Env::new();
        foreign.prepare_dir();
        let result = bring_up(&foreign.rdv, geteuid().wrapping_add(1), &mut spawn);
        assert_eq!(result.as_ref().err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_reports_unavailable(result);
    }

    #[test]
    fn peer_uid_mismatch_is_rejected_without_forwarding() {
        let env = Env::new();
        env.prepare_dir();
        let keeper = FakeKeeper::start(&env.rdv);
        // The private dir is ours, so only the peer check can reject; skip the dir check by
        // calling connect_checked directly with the wrong expectation.
        let result = connect_checked(&env.rdv, geteuid().wrapping_add(1));
        assert_eq!(result.as_ref().err().unwrap().kind(), io::ErrorKind::PermissionDenied);
        assert_reports_unavailable(result.map(|s| s.unwrap()));
        wait_accepted(&keeper, 1);
        let mut stream = keeper.accepted.lock().unwrap().remove(0);
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 16];
        // The attacher dropped the connection having written nothing: EOF, not data.
        assert_eq!(stream.read(&mut buf).unwrap(), 0);

        let mut spawn = || panic!("must not spawn");
        let result = bring_up(&env.rdv, geteuid().wrapping_add(1), &mut spawn);
        assert!(result.is_err());
    }

    #[test]
    fn peer_uid_matches_own_euid() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), geteuid());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn keeper_command_null_stdio_no_agent_setsid() {
        let env = Env::new();
        let cmd = keeper_command(Path::new("/x/herdr"), Some("s"));
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "SSH_AUTH_SOCK" && v.is_none()));
        let got: Vec<_> = cmd.get_args().collect();
        assert_eq!(
            got,
            ["--session", "s", "remote-client-bridge", "--x-mosh-keeper-v1"]
        );
        let bare = keeper_command(Path::new("/x/herdr"), None);
        assert_eq!(
            bare.get_args().collect::<Vec<_>>(),
            ["remote-client-bridge", "--x-mosh-keeper-v1"]
        );

        let script = env.base.join("fake-keeper.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nreadlink /proc/$$/fd/0 /proc/$$/fd/1 /proc/$$/fd/2 > \"$0.out\"\nps -o sid= -p $$ >> \"$0.out\"\necho $$ >> \"$0.out\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = keeper_command(&script, None);
        // Parent stdio is inherited unless the command nulls it; the test's own are not null.
        let mut child = child.spawn().unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        let out = std::fs::read_to_string(env.base.join("fake-keeper.sh.out")).unwrap();
        let lines: Vec<_> = out.lines().map(str::trim).collect();
        assert_eq!(lines[..3], ["/dev/null", "/dev/null", "/dev/null"], "{out}");
        assert_eq!(lines[3], pid.to_string(), "setsid: sid == pid: {out}");
        assert_eq!(lines[4], pid.to_string());
    }
}
