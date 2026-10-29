//! `QuicLegDialer`: a `LegDialer` that prefers a QUIC leg and falls back to ssh (unix only).
//!
//! Owns: the QUIC attempt policy (leg 0 in ssh bootstrap mode, then a bounded pinned handshake;
//! reconnects try QUIC with the cached bootstrap, then ssh), the bootstrap cache and its token
//! binding, the QUIC half of leg classification, and the runtime plus client endpoints shared
//! with every QUIC `Leg`.
//! Hides: quinn. Must not: write HELLO, read session bytes, or decide its own registration
//! (`start_ui_bridge` does, mosh-19g.1.7).
//! Lifecycle: created by `start_ui_bridge`, owned by the `LocalBridge`; every QUIC `Leg` keeps
//! the shared client alive, so no `block_on` ever runs on a runtime that has shut down.
// Unused in non-test builds until mosh-19g.1.7 registers the dialer; it removes this.
#![cfg_attr(not(test), allow(dead_code))]

use super::quic::{
    client_config, client_endpoint, spawn_render_reader, BlockingRecv, BlockingSend, QuicRuntime,
    QUIC_SILENCE_WINDOW, SERVER_NAME,
};
use super::quic_bootstrap::QuicBootstrap;
use super::ssh_leg::{classify_leg, Leg, LegDialer, LegEndReport, LegError, LegKind, SshLegDialer};
use std::cell::{Cell, RefCell};
use std::future::Future;
use std::io;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long one QUIC attempt (handshake plus opening the session stream) may take.
pub(super) const QUIC_HANDSHAKE_BOUND: Duration = Duration::from_secs(3);

/// How often a running QUIC attempt looks at the stop flag (parity with the ssh dial).
const STOP_POLL: Duration = Duration::from_millis(100);

/// The keeper's `END` reason for a refused admission token.
const END_QUIC_TOKEN: &str = "quic-token";

/// One tokio runtime and the lazily created v4/v6 wildcard client endpoints. Shared by the
/// dialer and every QUIC `Leg` it returns.
struct QuicClient {
    // Endpoints first: they drop before the runtime.
    endpoints: Mutex<[Option<quinn::Endpoint>; 2]>,
    rt: Arc<QuicRuntime>,
}

/// Notes that the keeper answered on the stream. The keeper binds a QUIC token when its HELLO
/// check admits it, before it replies with WELCOME or any other END, so an answer other than
/// the `quic-token` refusal (which `classify` turns into a dropped cache) means the token is
/// bound. The bridge never tells the dialer when WELCOME arrives, so this is the dialer's
/// own evidence.
struct HeardRecv {
    inner: BlockingRecv,
    heard: Arc<AtomicBool>,
}

impl io::Read for HeardRecv {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.heard.store(true, Ordering::SeqCst);
        }
        Ok(n)
    }
}

type OpenedStream = (quinn::Connection, quinn::SendStream, quinn::RecvStream);

impl QuicClient {
    fn new() -> io::Result<Arc<QuicClient>> {
        Ok(Arc::new(QuicClient {
            endpoints: Mutex::new([None, None]),
            rt: QuicRuntime::new("mosh-quic-client")?,
        }))
    }

    fn endpoint(&self, v6: bool) -> io::Result<quinn::Endpoint> {
        let mut endpoints = self
            .endpoints
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let slot = &mut endpoints[usize::from(v6)];
        if let Some(endpoint) = slot {
            return Ok(endpoint.clone());
        }
        let endpoint = client_endpoint(&self.rt, v6)?;
        *slot = Some(endpoint.clone());
        Ok(endpoint)
    }

    /// Runs `attempt` on the client runtime for at most `bound`; abandons it once `stop` is set.
    fn bounded<T>(
        &self,
        bound: Duration,
        stop: &AtomicBool,
        attempt: impl Future<Output = io::Result<T>>,
    ) -> io::Result<T> {
        let stopped = async {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(STOP_POLL).await;
            }
        };
        self.rt.block_on(async move {
            tokio::select! {
                result = tokio::time::timeout(bound, attempt) => result.unwrap_or_else(|_| {
                    Err(io::Error::new(io::ErrorKind::TimedOut, "QUIC attempt timed out"))
                }),
                () = stopped => Err(io::Error::new(io::ErrorKind::Interrupted, "QUIC attempt stopped")),
            }
        })?
    }

    /// Resolves `host`, then a pinned handshake plus the session's bidi stream, all within
    /// `bound` and abandoned once `stop` is set.
    fn connect(
        &self,
        host: &str,
        bootstrap: &QuicBootstrap,
        grace: Duration,
        bound: Duration,
        stop: &AtomicBool,
    ) -> io::Result<OpenedStream> {
        let deadline = Instant::now() + bound;
        let target = (host.to_owned(), bootstrap.port);
        // getaddrinfo cannot be cancelled: on the blocking pool it is abandoned with the attempt.
        let lookup = async move {
            tokio::task::spawn_blocking(move || target.to_socket_addrs().map(|mut a| a.next()))
                .await
                .map_err(io::Error::other)?
        };
        let addr = self
            .bounded(bound, stop, lookup)?
            .ok_or_else(|| io::Error::other("dial host resolved to no address"))?;
        let endpoint = self.endpoint(addr.is_ipv6())?;
        let config = client_config(bootstrap.fingerprint, grace)?;
        let attempt = async move {
            let conn = endpoint
                .connect_with(config, addr, SERVER_NAME)
                .map_err(io::Error::other)?
                .await
                .map_err(io::Error::other)?;
            let (send, recv) = conn.open_bi().await.map_err(io::Error::other)?;
            Ok::<_, io::Error>((conn, send, recv))
        };
        self.bounded(deadline.saturating_duration_since(Instant::now()), stop, attempt)
    }

    /// A `Leg` over one opened stream. Teardown closes the connection; the leg's end report
    /// carries the connection's close reason.
    fn leg(
        self: &Arc<Self>,
        opened: OpenedStream,
        token: &QuicBootstrap,
        heard: Arc<AtomicBool>,
    ) -> Leg {
        let (conn, send, recv) = opened;
        let handle = self.rt.handle();
        let close_reason = Arc::new(Mutex::new(None));
        let watched = conn.clone();
        let cell = Arc::clone(&close_reason);
        handle.spawn(async move {
            let reason = watched.closed().await.to_string();
            *cell.lock().unwrap_or_else(PoisonError::into_inner) = Some(reason);
        });
        let probe = conn.clone();
        let stats_conn = conn.clone();
        let render_lane = spawn_render_reader(&handle, conn.clone());
        let client = Arc::clone(self);
        Leg::new(
            Box::new(HeardRecv {
                inner: BlockingRecv::new(handle.clone(), recv),
                heard,
            }),
            Box::new(BlockingSend::new(handle, send)),
            Box::new(move |_kill, _deadline| {
                // Holding the client keeps the runtime alive until the leg is gone.
                let _client = &client;
                conn.close(0u32.into(), b"leg end");
                (None, None)
            }),
        )
        .with_admission_token(token.token.to_string())
        .with_rtt_probe(Box::new(move || Some(probe.rtt())))
        .with_stats_probe(Box::new(move || {
            let stats = stats_conn.stats();
            serde_json::json!({
                "cwnd": stats.path.cwnd,
                "rtt_ms": stats.path.rtt.as_millis() as u64,
                "lost_packets": stats.path.lost_packets,
                "congestion_events": stats.path.congestion_events,
                "udp_tx": stats.udp_tx.datagrams,
                "udp_rx": stats.udp_rx.datagrams,
            })
        }))
        .with_silence_timeout(QUIC_SILENCE_WINDOW)
        .with_close_reason(close_reason)
        .with_render_lane(render_lane)
    }
}

pub(super) struct QuicLegDialer {
    ssh: SshLegDialer,
    /// What `ssh -G` resolved the target to; QUIC dials `<dial_host>:<bootstrap port>`.
    dial_host: String,
    /// The client's `max_idle_timeout`.
    grace: Duration,
    client: Arc<QuicClient>,
    /// `None`: no usable bootstrap, so QUIC is off until an ssh leg brings one.
    cache: Option<QuicBootstrap>,
    /// A QUIC leg reached WELCOME with the cached token (the keeper then keeps it for the session).
    token_bound: Cell<bool>,
    /// Set once the last QUIC leg returned heard from the keeper; folded into `token_bound` at
    /// the next dial.
    last_heard: RefCell<Option<Arc<AtomicBool>>>,
    /// The last QUIC leg died before WELCOME: the next dial goes straight to ssh.
    skip_quic_next: Cell<bool>,
    /// A reconnect QUIC leg got `END quic-token`: drop the cache at the next dial.
    clear_cache: Cell<bool>,
    /// The leg the last dial returned is a QUIC leg (what `classify` is judging).
    last_leg_quic: Cell<bool>,
}

impl QuicLegDialer {
    pub(super) fn new(
        ssh: SshLegDialer,
        dial_host: String,
        grace: Duration,
    ) -> io::Result<QuicLegDialer> {
        Ok(QuicLegDialer {
            ssh,
            dial_host,
            grace,
            client: QuicClient::new()?,
            cache: None,
            token_bound: Cell::new(false),
            last_heard: RefCell::new(None),
            skip_quic_next: Cell::new(false),
            clear_cache: Cell::new(false),
            last_leg_quic: Cell::new(false),
        })
    }

    /// Fold a bootstrap an ssh leg brought into the cache: before the first binding it replaces
    /// the cache; once the token is bound only port and fingerprint follow the keeper.
    fn refresh_cache(&mut self, fresh: QuicBootstrap) {
        self.cache = Some(match self.cache {
            Some(old) if self.token_bound.get() => QuicBootstrap {
                token: old.token,
                ..fresh
            },
            _ => fresh,
        });
    }

    fn try_quic(
        &self,
        bootstrap: &QuicBootstrap,
        bound: Duration,
        stop: &AtomicBool,
    ) -> io::Result<Leg> {
        let opened = self
            .client
            .connect(&self.dial_host, bootstrap, self.grace, bound, stop)?;
        let heard = Arc::new(AtomicBool::new(false));
        *self.last_heard.borrow_mut() = Some(Arc::clone(&heard));
        Ok(self.client.leg(opened, bootstrap, heard))
    }

    /// The handshake bound, shortened to what the setup deadline leaves; `None` when none is left.
    fn quic_bound(setup_deadline: Option<Instant>, stop: &AtomicBool) -> Option<Duration> {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        let left = setup_deadline.map_or(QUIC_HANDSHAKE_BOUND, |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let bound = left.min(QUIC_HANDSHAKE_BOUND);
        (!bound.is_zero()).then_some(bound)
    }

    /// The ssh leg in bootstrap mode; a bootstrap it brings refreshes the cache.
    fn dial_ssh(
        &mut self,
        kind: LegKind,
        setup_deadline: Option<Instant>,
        stop: &AtomicBool,
    ) -> Result<Leg, LegError> {
        let (leg, bootstrap) = self.ssh.dial_bootstrap(kind, setup_deadline, stop)?;
        if let Some(bootstrap) = bootstrap {
            self.refresh_cache(bootstrap);
        }
        Ok(leg)
    }
}

impl LegDialer for QuicLegDialer {
    fn dial(
        &mut self,
        kind: LegKind,
        setup_deadline: Option<Instant>,
        stop: &AtomicBool,
    ) -> Result<Leg, LegError> {
        // Before the refusal check below: a refused token also "answered", and clearing the
        // cache then resets the binding.
        let heard = self.last_heard.borrow_mut().take();
        if heard.is_some_and(|heard| heard.load(Ordering::SeqCst)) {
            self.token_bound.set(true);
        }
        if self.clear_cache.take() {
            self.cache = None;
            self.token_bound.set(false);
        }
        let skip = self.skip_quic_next.take();
        self.last_leg_quic.set(false);
        match kind {
            LegKind::Initial => {
                // The ssh leg stays alive as the QUIC leg's standby until WELCOME.
                let ssh_leg = self.dial_ssh(kind, setup_deadline, stop)?;
                let Some(bootstrap) = self.cache else {
                    return Ok(ssh_leg);
                };
                let Some(bound) = Self::quic_bound(None, stop) else {
                    return Ok(ssh_leg);
                };
                match self.try_quic(&bootstrap, bound, stop) {
                    Ok(quic) => {
                        self.last_leg_quic.set(true);
                        Ok(quic.with_standby(ssh_leg))
                    }
                    Err(err) => {
                        tracing::debug!(%err, "mosh QUIC attempt failed on leg 0; using ssh");
                        Ok(ssh_leg)
                    }
                }
            }
            LegKind::Reconnect => {
                if let (Some(bootstrap), false) = (self.cache, skip) {
                    if let Some(bound) = Self::quic_bound(setup_deadline, stop) {
                        match self.try_quic(&bootstrap, bound, stop) {
                            Ok(quic) => {
                                self.last_leg_quic.set(true);
                                return Ok(quic);
                            }
                            Err(err) => {
                                tracing::debug!(%err, "mosh QUIC reconnect failed; trying ssh");
                            }
                        }
                    }
                }
                self.dial_ssh(kind, setup_deadline, stop)
            }
        }
    }

    fn classify(&self, kind: LegKind, end: &LegEndReport) -> LegError {
        // A leg-0 QUIC leg's standby is an ssh leg while `last_leg_quic` still says QUIC; only
        // ssh legs have an exit status.
        if !self.last_leg_quic.get() || end.exit_status.is_some() {
            return classify_leg(kind, end);
        }
        if end.welcome_reached {
            self.token_bound.set(true);
            return LegError::Retryable(io::Error::other(format!(
                "QUIC leg ended after WELCOME{}",
                close_note(end)
            )));
        }
        match end.pre_welcome_end.as_deref() {
            Some(END_QUIC_TOKEN) => {
                self.clear_cache.set(true);
                LegError::Retryable(io::Error::other(
                    "keeper refused the QUIC token before WELCOME",
                ))
            }
            // Refusals an ssh leg would get too (protocol-version, token-in-use, ...).
            Some(_) => classify_leg(kind, end),
            None => {
                self.skip_quic_next.set(true);
                LegError::Retryable(io::Error::other(format!(
                    "QUIC leg ended before WELCOME{}",
                    close_note(end)
                )))
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        self.ssh.backoff(attempt)
    }
}

fn close_note(end: &LegEndReport) -> String {
    end.close_reason
        .as_deref()
        .map_or_else(String::new, |reason| format!(" ({reason})"))
}

#[cfg(test)]
mod tests {
    use super::super::super::{RemoteHerdr, RemotePlatform};
    use super::super::quic::{server_config, server_endpoint, ServerIdentity, KEEPER_IDLE_TIMEOUT};
    use super::super::quic_bootstrap::{Fingerprint, QuicToken};
    use super::*;
    use std::io::{Read, Write};
    use std::net::UdpSocket;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, ExitStatus};

    const MARKER_PRINTF: &str = "printf '\\nherdr-remote-output-ready:1\\n'";
    const GRACE: Duration = Duration::from_secs(30);

    /// What the in-process server does with each accepted session stream.
    #[derive(Clone, Copy)]
    enum Serve {
        Echo,
        /// Read the first bytes, then close the connection like a keeper refusing the token.
        Refuse,
    }

    struct TestServer {
        _rt: Arc<QuicRuntime>,
        _endpoint: quinn::Endpoint,
        port: u16,
        fingerprint: Fingerprint,
        accepted: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn server(serve: Serve) -> TestServer {
        let rt = QuicRuntime::new("quic-leg-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        socket.set_nonblocking(true).expect("nonblocking");
        let port = socket.local_addr().expect("addr").port();
        let endpoint = server_endpoint(
            &rt,
            server_config(&identity, KEEPER_IDLE_TIMEOUT).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let accept_endpoint = endpoint.clone();
        let counter = Arc::clone(&accepted);
        rt.handle().spawn(async move {
            while let Some(incoming) = accept_endpoint.accept().await {
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                        return;
                    };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0u8; 4096];
                    match serve {
                        Serve::Echo => {
                            while let Ok(Some(n)) = recv.read(&mut buf).await {
                                if send.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Serve::Refuse => {
                            let _ = recv.read(&mut buf).await;
                            conn.close(1u32.into(), b"quic-token");
                        }
                    }
                });
            }
        });
        TestServer {
            fingerprint: identity.fingerprint,
            _rt: rt,
            _endpoint: endpoint,
            port,
            accepted,
        }
    }

    impl TestServer {
        fn bootstrap(&self, token: u8) -> QuicBootstrap {
            QuicBootstrap {
                port: self.port,
                fingerprint: self.fingerprint,
                token: QuicToken([token; 32]),
            }
        }
    }

    /// An ssh leg that prints the ready marker and `bootstrap`'s line (or `none`), then echoes
    /// whatever it is sent.
    fn ssh_script(bootstrap: Option<QuicBootstrap>) -> Command {
        let line = QuicBootstrap::encode_line(bootstrap.as_ref());
        let mut command = Command::new("sh");
        command.args(["-c", &format!("{MARKER_PRINTF}; echo '{line}'; exec cat")]);
        command
    }

    /// A dialer whose ssh legs are scripted; each ssh dial answers `bootstrap`.
    fn dialer(bootstrap: Option<QuicBootstrap>) -> QuicLegDialer {
        let mut ssh = SshLegDialer::new(
            "user@host".into(),
            &RemoteHerdr::for_platform(RemotePlatform {
                os: "linux",
                arch: "x86_64",
            }),
            "work",
            None,
            false,
            false,
        );
        ssh.scripted = Some(Box::new(move |_| ssh_script(bootstrap)));
        QuicLegDialer::new(ssh, "127.0.0.1".into(), GRACE).expect("dialer")
    }

    fn dial(dialer: &mut QuicLegDialer, kind: LegKind) -> Leg {
        dialer
            .dial(kind, None, &AtomicBool::new(false))
            .expect("dial")
    }

    fn quic_end(welcome: bool, pre_welcome_end: Option<&str>) -> LegEndReport {
        LegEndReport {
            welcome_reached: welcome,
            pre_welcome_end: pre_welcome_end.map(str::to_owned),
            ..LegEndReport::default()
        }
    }

    fn assert_retryable(err: LegError) {
        assert!(matches!(err, LegError::Retryable(_)), "{err:?}");
    }

    /// Send `text` through the leg and read the same number of bytes back.
    fn echo(leg: &mut Leg, text: &str) -> String {
        let mut writer = leg.take_writer().expect("writer");
        writer.write_all(text.as_bytes()).expect("write");
        let mut reader = leg.take_reader().expect("reader");
        let mut buf = vec![0u8; text.len()];
        reader.read_exact(&mut buf).expect("read");
        String::from_utf8(buf).expect("utf8")
    }

    fn dummy_status(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    // ---- classification -----------------------------------------------------------------

    #[test]
    fn post_welcome_quic_end_is_retryable_and_binds_token() {
        let d = dialer(None);
        d.last_leg_quic.set(true);
        assert!(!d.token_bound.get());
        assert_retryable(d.classify(LegKind::Reconnect, &quic_end(true, None)));
        assert!(d.token_bound.get());
        assert!(!d.skip_quic_next.get() && !d.clear_cache.get());
    }

    #[test]
    fn reconnect_quic_token_end_clears_cache() {
        let mut d = dialer(None);
        d.cache = Some(QuicBootstrap {
            port: 1,
            fingerprint: Fingerprint([1; 32]),
            token: QuicToken([1; 32]),
        });
        d.last_leg_quic.set(true);
        assert_retryable(d.classify(LegKind::Reconnect, &quic_end(false, Some("quic-token"))));
        assert!(d.clear_cache.get());
        assert!(!d.token_bound.get());
        // Applied at the next dial: no QUIC attempt, and the ssh leg brings no bootstrap.
        let leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(leg.admission_token(), None);
        assert!(d.cache.is_none());
    }

    #[test]
    fn pre_welcome_quic_death_skips_quic_next_dial() {
        let server = server(Serve::Echo);
        let bootstrap = server.bootstrap(3);
        let mut d = dialer(Some(bootstrap));
        d.cache = Some(bootstrap);
        d.last_leg_quic.set(true);
        assert_retryable(d.classify(LegKind::Reconnect, &quic_end(false, None)));
        assert!(d.skip_quic_next.get());
        // The skipped dial uses ssh although a live QUIC server is cached.
        let leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(leg.admission_token(), None);
        assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
        // The skip lasts one dial only.
        let mut leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(
            leg.admission_token(),
            Some(bootstrap.token.to_string().as_str())
        );
        assert_eq!(echo(&mut leg, "back on quic\n"), "back on quic\n");
    }

    #[test]
    fn other_pre_welcome_quic_refusals_use_the_phase1_table() {
        let d = dialer(None);
        d.last_leg_quic.set(true);
        let err = d.classify(
            LegKind::Reconnect,
            &quic_end(false, Some("protocol-version")),
        );
        assert!(matches!(err, LegError::Fatal(_)), "{err:?}");
        assert!(!d.skip_quic_next.get() && !d.clear_cache.get());
    }

    #[test]
    fn ssh_leg_ends_use_phase1_table() {
        let d = dialer(None);
        // Last leg was ssh.
        let mut ssh_end = quic_end(false, Some("protocol-version"));
        ssh_end.exit_status = Some(dummy_status(0));
        let err = d.classify(LegKind::Initial, &ssh_end);
        assert!(matches!(err, LegError::Unsupported(_)), "{err:?}");
        let err = d.classify(LegKind::Reconnect, &ssh_end);
        assert!(matches!(err, LegError::Fatal(_)), "{err:?}");
        let welcomed = LegEndReport {
            welcome_reached: true,
            exit_status: Some(dummy_status(0)),
            ..LegEndReport::default()
        };
        assert_retryable(d.classify(LegKind::Reconnect, &welcomed));
        assert!(!d.token_bound.get(), "an ssh WELCOME binds no QUIC token");
        // An ssh standby that failed after a QUIC leg-0 attempt is judged as ssh too.
        d.last_leg_quic.set(true);
        let err = d.classify(LegKind::Initial, &ssh_end);
        assert!(matches!(err, LegError::Unsupported(_)), "{err:?}");
        assert!(!d.skip_quic_next.get() && !d.clear_cache.get());
    }

    #[test]
    fn fingerprint_mismatch_falls_through_to_ssh_same_dial() {
        let server = server(Serve::Echo);
        let mut pinned_wrong = server.bootstrap(4);
        pinned_wrong.fingerprint = Fingerprint([0xee; 32]);
        let fresh = server.bootstrap(5);
        let mut d = dialer(Some(fresh));
        d.cache = Some(pinned_wrong);
        let started = Instant::now();
        let mut leg = dial(&mut d, LegKind::Reconnect);
        assert!(
            started.elapsed() < QUIC_HANDSHAKE_BOUND,
            "mismatch fails fast"
        );
        assert_eq!(leg.admission_token(), None, "an ssh leg, not an error");
        assert_eq!(echo(&mut leg, "ssh\n"), "ssh\n");
        assert_eq!(
            d.cache,
            Some(fresh),
            "the ssh leg's bootstrap replaced the cache"
        );
        assert_eq!(server.accepted.load(Ordering::SeqCst), 0);
    }

    // ---- loopback -----------------------------------------------------------------------

    #[test]
    fn leg0_returns_quic_leg_with_live_ssh_standby() {
        let server = server(Serve::Echo);
        let bootstrap = server.bootstrap(6);
        let mut d = dialer(Some(bootstrap));
        let mut leg = dial(&mut d, LegKind::Initial);
        assert_eq!(
            leg.admission_token(),
            Some(bootstrap.token.to_string().as_str())
        );
        assert_eq!(leg.silence_timeout(), Some(QUIC_SILENCE_WINDOW));
        let mut standby = leg.take_standby().expect("ssh standby");
        assert_eq!(echo(&mut leg, "over quic\n"), "over quic\n");
        // The standby is a live ssh leg, not yet torn down.
        assert_eq!(echo(&mut standby, "over ssh\n"), "over ssh\n");
        assert_eq!(standby.admission_token(), None);
        assert!(leg.take_render_lane().is_some(), "QUIC legs carry a render lane");
        assert!(standby.take_render_lane().is_none());
        assert!(d.last_leg_quic.get());
        standby.finish(true, Instant::now() + Duration::from_secs(5));
    }

    #[test]
    fn leg0_blackholed_port_returns_ssh_leg_within_3_5s() {
        let blackhole = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let bootstrap = QuicBootstrap {
            port: blackhole.local_addr().expect("addr").port(),
            fingerprint: Fingerprint([2; 32]),
            token: QuicToken([2; 32]),
        };
        let mut d = dialer(Some(bootstrap));
        let started = Instant::now();
        let mut leg = dial(&mut d, LegKind::Initial);
        let took = started.elapsed();
        assert!(took < Duration::from_millis(3500), "took {took:?}");
        assert!(
            took >= Duration::from_secs(2),
            "the bound, not an early error: {took:?}"
        );
        assert_eq!(leg.admission_token(), None);
        assert!(leg.take_standby().is_none());
        assert!(!d.last_leg_quic.get());
        assert_eq!(echo(&mut leg, "ssh\n"), "ssh\n");
    }

    /// A future that blocks a pool thread for `d`, standing in for a hung resolver.
    async fn hung_lookup(d: Duration) -> io::Result<()> {
        tokio::task::spawn_blocking(move || std::thread::sleep(d))
            .await
            .map_err(io::Error::other)
    }

    #[test]
    fn a_hung_lookup_is_cut_off_at_the_bound() {
        let client = QuicClient::new().expect("client");
        let started = Instant::now();
        let err = client
            .bounded(
                Duration::from_millis(300),
                &AtomicBool::new(false),
                hung_lookup(Duration::from_secs(10)),
            )
            .expect_err("bounded");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    }

    #[test]
    fn stop_ends_an_attempt_promptly() {
        let client = QuicClient::new().expect("client");
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                stop.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let err = client
            .bounded(QUIC_HANDSHAKE_BOUND, &stop, hung_lookup(Duration::from_secs(10)))
            .expect_err("bounded");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        setter.join().expect("setter");
    }

    #[test]
    fn stop_ends_a_blackholed_quic_attempt_promptly() {
        let blackhole = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let bootstrap = QuicBootstrap {
            port: blackhole.local_addr().expect("addr").port(),
            fingerprint: Fingerprint([2; 32]),
            token: QuicToken([2; 32]),
        };
        let d = dialer(Some(bootstrap));
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                stop.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        assert!(d.try_quic(&bootstrap, QUIC_HANDSHAKE_BOUND, &stop).is_err());
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        setter.join().expect("setter");
    }

    #[test]
    fn leg0_refused_token_leaves_standby_alive_and_usable() {
        let server = server(Serve::Refuse);
        let mut d = dialer(Some(server.bootstrap(7)));
        let mut leg = dial(&mut d, LegKind::Initial);
        let mut standby = leg.take_standby().expect("ssh standby");
        // The bridge's HELLO reaches the keeper, which refuses and closes.
        leg.take_writer()
            .expect("writer")
            .write_all(b"HELLO\n")
            .expect("write");
        let mut reader = leg.take_reader().expect("reader");
        let mut buf = [0u8; 16];
        let ended = match reader.read(&mut buf) {
            Ok(0) | Err(_) => true,
            Ok(_) => false,
        };
        assert!(ended, "the refused QUIC leg ends before WELCOME");
        assert_eq!(echo(&mut standby, "still here\n"), "still here\n");
        standby.finish(true, Instant::now() + Duration::from_secs(5));
    }

    #[test]
    fn quic_leg_rtt_probe_is_some_after_traffic() {
        let server = server(Serve::Echo);
        let mut d = dialer(Some(server.bootstrap(8)));
        let mut leg = dial(&mut d, LegKind::Initial);
        let mut writer = leg.take_writer().expect("writer");
        let mut reader = leg.take_reader().expect("reader");
        for round in 0..5 {
            let text = format!("ping {round}\n");
            writer.write_all(text.as_bytes()).expect("write");
            let mut buf = vec![0u8; text.len()];
            reader.read_exact(&mut buf).expect("read");
            assert_eq!(buf, text.as_bytes());
        }
        let rtt = leg.rtt().expect("a QUIC leg has an RTT probe");
        // quinn's pre-sample estimate is 333 ms; a loopback sample is far below it.
        assert!(rtt < Duration::from_millis(200), "{rtt:?}");
        let ssh = leg.take_standby().expect("standby");
        assert_eq!(ssh.rtt(), None, "ssh legs have no probe");
        ssh.finish(true, Instant::now() + Duration::from_secs(5));
    }

    #[test]
    fn ssh_reconnect_bootstrap_is_tried_next_dial() {
        let server = server(Serve::Echo);
        let bootstrap = server.bootstrap(9);
        let mut d = dialer(Some(bootstrap));
        assert!(d.cache.is_none());
        // An ssh reconnect leg brings a bootstrap: QUIC was off, the cache now holds it.
        let ssh_leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(ssh_leg.admission_token(), None);
        assert_eq!(d.cache, Some(bootstrap));
        // The next dial tries QUIC with it.
        let mut leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(
            leg.admission_token(),
            Some(bootstrap.token.to_string().as_str())
        );
        assert!(
            leg.take_standby().is_none(),
            "reconnect QUIC legs have no standby"
        );
        assert_eq!(echo(&mut leg, "reconnected\n"), "reconnected\n");
    }

    #[test]
    fn bound_token_survives_refresh() {
        let server = server(Serve::Echo);
        let old = server.bootstrap(10);
        let fresh = QuicBootstrap {
            port: server.port,
            fingerprint: Fingerprint([0x33; 32]),
            token: QuicToken([11; 32]),
        };
        // The cached fingerprint does not match the server, so the dial falls through to ssh
        // and takes the refresh there.
        let mut stale = old;
        stale.fingerprint = Fingerprint([0x44; 32]);
        let mut d = dialer(Some(fresh));
        d.cache = Some(stale);
        d.last_leg_quic.set(true);
        assert_retryable(d.classify(LegKind::Reconnect, &quic_end(true, None)));
        dial(&mut d, LegKind::Reconnect);
        let cached = d.cache.expect("cache");
        assert_eq!(cached.token, stale.token, "a bound token is kept");
        assert_eq!(cached.fingerprint, fresh.fingerprint);
        assert_eq!(cached.port, fresh.port);
    }

    #[test]
    fn answered_quic_leg_binds_the_token_for_the_next_refresh() {
        let server = server(Serve::Echo);
        let first = server.bootstrap(12);
        let fresh = QuicBootstrap {
            port: server.port,
            fingerprint: Fingerprint([0x55; 32]),
            token: QuicToken([13; 32]),
        };
        let mut d = dialer(Some(fresh));
        d.cache = Some(first);
        // A QUIC leg that the keeper answers, then ends without the bridge classifying it
        // (the bridge only classifies legs that end before WELCOME).
        let mut leg = dial(&mut d, LegKind::Reconnect);
        assert_eq!(echo(&mut leg, "answered\n"), "answered\n");
        drop(leg);
        // The next dial cannot pin the server (wrong fingerprint), so ssh brings a refresh.
        d.cache = Some(QuicBootstrap {
            fingerprint: Fingerprint([0x66; 32]),
            ..first
        });
        dial(&mut d, LegKind::Reconnect);
        let cached = d.cache.expect("cache");
        assert_eq!(cached.token, first.token, "the bound token is kept");
        assert_eq!(cached.fingerprint, fresh.fingerprint);
        assert_eq!(cached.port, fresh.port);
    }

    #[test]
    fn silent_quic_leg_leaves_the_token_unbound() {
        let server = server(Serve::Echo);
        let first = server.bootstrap(14);
        let fresh = QuicBootstrap {
            port: server.port,
            fingerprint: Fingerprint([0x77; 32]),
            token: QuicToken([15; 32]),
        };
        let mut d = dialer(Some(fresh));
        d.cache = Some(first);
        // The leg is dialed but the keeper never answers on it.
        let leg = dial(&mut d, LegKind::Reconnect);
        drop(leg);
        d.cache = Some(QuicBootstrap {
            fingerprint: Fingerprint([0x88; 32]),
            ..first
        });
        dial(&mut d, LegKind::Reconnect);
        assert_eq!(d.cache, Some(fresh), "an unanswered token is replaced");
    }

    #[test]
    fn unbound_token_is_replaced_by_refresh() {
        let mut d = dialer(None);
        let first = QuicBootstrap {
            port: 1,
            fingerprint: Fingerprint([1; 32]),
            token: QuicToken([1; 32]),
        };
        let second = QuicBootstrap {
            port: 2,
            fingerprint: Fingerprint([2; 32]),
            token: QuicToken([2; 32]),
        };
        d.cache = Some(first);
        d.refresh_cache(second);
        assert_eq!(d.cache, Some(second));
    }
}
