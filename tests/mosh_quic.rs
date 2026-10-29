//! QUIC leg acceptance tests (mosh-19g.1.5): the QUIC leg runs over a userspace UDP relay
//! (`UdpRelay`), with the attacher's bootstrap line rewritten by the fake ssh so the client
//! dials the relay instead of the keeper. Spec: 3a QUIC leg design, sections 6 and 7.
//!
//! The functional tests run in the default suite. Every wall-clock test is `#[ignore]`d (run
//! with `--run-ignored all`), takes the host-wide `timing_lock()` and drives the link at 150 ms
//! one-way where a bound mentions RTT.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use mosh_harness::{
    bootstrap_line_fingerprint, bootstrap_line_port, knob, timing_lock, wait_for, wait_until,
    write_quic_port_filter, Client, ClientOptions, Remote, UdpRelay,
};

const READY: Duration = Duration::from_secs(30);
const ONE_WAY: Duration = Duration::from_millis(150);
const RTT: Duration = Duration::from_millis(300);
/// One server render plus the client's repaint and our 10 ms polling. Measured echo times
/// (spec 3a, Post-Implementation Notes) leave well over 100 ms of this unused.
const RENDER: Duration = Duration::from_millis(500);
/// The QUIC handshake bound of one attempt (`QUIC_HANDSHAKE_BOUND`).
const HANDSHAKE_BOUND: Duration = Duration::from_secs(3);
/// The longest reconnect backoff step (`reconnect_backoff`: 1, 2, 4, 8, 8... seconds).
const BACKOFF_STEP: Duration = Duration::from_secs(8);
/// A dense flood: `base64 /dev/urandom` redraws every cell, so herdr sends full screen diffs.
const DENSE_FLOOD: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)"#;
const DENSE_SIZE: (u16, u16) = (400, 100);

fn context(remote: &Remote, client: &Client, relay: Option<&UdpRelay>) -> String {
    format!(
        "screen:\n{}\nlocal: {:?}\nkeeper: {:?}\nsession legs: {:?}\nssh alive: {}\nrelay: {:?}",
        client.screen(),
        client.events(),
        remote.keeper_events(),
        remote.session_legs(&client.id),
        remote.ssh_session_children_alive(&client.id),
        relay.map(UdpRelay::stats),
    )
}

/// A client attached over QUIC: the session is up, leg 0's ssh child is gone and the relay
/// carried datagrams. The relay's delay is set by the caller beforehand.
fn connect_quic(remote: &Remote, relay: &UdpRelay, opts: &ClientOptions) -> Client {
    let client = remote.spawn_remote_client(opts);
    client.wait_attached(remote, READY);
    assert!(
        wait_for(READY, || remote.ssh_session_children_alive(&client.id) == 0),
        "the leg-0 ssh child is still alive: {}",
        context(remote, &client, Some(relay))
    );
    let stats = relay.stats();
    assert!(
        stats.forwarded_up > 0 && stats.forwarded_down > 0,
        "the session did not use QUIC: {}",
        context(remote, &client, Some(relay))
    );
    client
}

/// Type `probe` and return the time until the client screen shows it (`None` after
/// `timeout`), then clear the input line.
fn echo_latency(client: &mut Client, probe: &str, timeout: Duration) -> Option<Duration> {
    client.type_keys(probe);
    let t0 = Instant::now();
    let seen = wait_until(timeout, Duration::from_millis(10), || {
        client.screen().contains(probe)
    });
    let latency = t0.elapsed();
    // Clear the input line and wait for it, so a later command is not typed onto the probe.
    client.type_keys("\x15");
    wait_for(READY, || !client.screen().contains(probe));
    seen.then_some(latency)
}

fn assert_echo(remote: &Remote, client: &mut Client, relay: Option<&UdpRelay>, probe: &str) {
    assert!(
        echo_latency(client, probe, READY).is_some(),
        "no echo of {probe}: {}",
        context(remote, client, relay)
    );
}

fn assert_output_works(remote: &Remote, client: &Client, relay: Option<&UdpRelay>, tag: &str) {
    remote.pane_run(&format!("printf 'OUT_%s\\n' {tag}"));
    assert!(
        client.wait_screen(&format!("OUT_{tag}"), READY),
        "no pane output {tag}: {}",
        context(remote, client, relay)
    );
}

// ---- harness self-tests -----------------------------------------------------------------

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("mq-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run the port filter over `input` (what the wrapped command prints) with port 4242.
fn filtered(scratch: &Scratch, input: &[u8]) -> Vec<u8> {
    let filter = write_quic_port_filter(&scratch.0);
    let data = scratch.0.join("input");
    fs::write(&data, input).expect("write input");
    let out = Command::new("perl")
        .arg(&filter)
        .arg("4242")
        .args(["cat"])
        .arg(&data)
        .output()
        .expect("run the filter");
    assert!(out.status.success(), "filter failed: {:?}", out.status);
    out.stdout
}

#[test]
fn port_filter_rewrites_only_the_first_bootstrap_line() {
    let scratch = Scratch::new("filter");
    let fp = "ab".repeat(32);
    let tok = "cd".repeat(32);
    let marker = "\nherdr-remote-output-ready:1\n";

    // The bootstrap line after the marker: port rewritten, everything else untouched,
    // including a later line that merely looks like one.
    let input = format!(
        "{marker}herdr-mosh-quic:1 40123 {fp} {tok}\n\x01session herdr-mosh-quic:1 7 {fp} {tok}\n"
    );
    let expected = format!(
        "{marker}herdr-mosh-quic:1 4242 {fp} {tok}\n\x01session herdr-mosh-quic:1 7 {fp} {tok}\n"
    );
    assert_eq!(filtered(&scratch, input.as_bytes()), expected.as_bytes());

    // A `none` line passes untouched.
    let none = format!("{marker}herdr-mosh-quic:1 none\nRAW");
    assert_eq!(filtered(&scratch, none.as_bytes()), none.as_bytes());

    // No bootstrap line (a leg without the QUIC flag): session bytes pass raw, binary or not.
    let raw = [
        marker.as_bytes(),
        &[1u8, 0, 255, b'h', b'e', b'r', b'd', 10, 2],
    ]
    .concat();
    assert_eq!(filtered(&scratch, &raw), raw);

    // The line is rewritten through the production parser's view of it, too.
    let line = format!("herdr-mosh-quic:1 40123 {fp} {tok}");
    assert_eq!(bootstrap_line_port(&line), Some(40123));
    assert_eq!(bootstrap_line_port("herdr-mosh-quic:1 none"), None);
}

#[test]
fn bootstrap_port_rewrite_produces_a_parseable_line() {
    let remote = Remote::start();
    let relay = remote.start_quic_relay(1);

    // The knob is set now: the same attacher run through the fake ssh reports the relay.
    let rewritten = remote.quic_bootstrap_line();
    assert_eq!(
        bootstrap_line_port(&rewritten),
        Some(relay.local_addr().port()),
        "rewritten line: {rewritten}"
    );

    // Without the knob the keeper's own port shows, with the same fingerprint (one keeper).
    remote.clear_knob(knob::QUIC_PORT);
    let direct = remote.quic_bootstrap_line();
    let direct_port = bootstrap_line_port(&direct).expect("direct line parses");
    assert_ne!(direct_port, relay.local_addr().port());
    assert_eq!(
        bootstrap_line_fingerprint(&direct),
        bootstrap_line_fingerprint(&rewritten)
    );
}

// ---- functional tests -------------------------------------------------------------------

#[test]
fn quic_in_use_no_ssh_child_and_echo_works() {
    let remote = Remote::start();
    let relay = remote.start_quic_relay(2);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::default());
    assert_eq!(
        remote.session_legs(&client.id).len(),
        1,
        "only leg 0 ran ssh: {}",
        context(&remote, &client, Some(&relay))
    );
    assert!(
        remote.session_legs(&client.id)[0].contains("--x-mosh-quic-v1"),
        "leg 0 ran in bootstrap mode: {:?}",
        remote.session_legs(&client.id)
    );
    assert_echo(&remote, &mut client, Some(&relay), "qecho1");
    assert_output_works(&remote, &client, Some(&relay), "QUICOUT");
    // The ssh child stays gone while the session is used.
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        0,
        "{}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
}

#[test]
fn strip_quic_capability_gives_ssh_only_session() {
    let remote = Remote::start();
    remote.set_knob(knob::QUIC, "");
    remote.set_knob(knob::STRIP_CAPS, "x-mosh-quic-v1");
    let mut client = remote.spawn_remote_client(&ClientOptions::default());
    client.wait_attached(&remote, READY);
    assert!(
        client.wait_event("welcome", 1, READY),
        "no mosh session: {}",
        context(&remote, &client, None)
    );
    let legs = remote.session_legs(&client.id);
    assert_eq!(legs.len(), 1, "{legs:?}");
    assert!(
        !legs[0].contains("--x-mosh-quic-v1"),
        "the QUIC flag was sent to a remote that does not advertise it: {legs:?}"
    );
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        1,
        "the ssh leg is the session: {}",
        context(&remote, &client, None)
    );
    assert_echo(&remote, &mut client, None, "qstrip1");
}

/// Stands in for a pre-Phase-3 keeper at the rendezvous socket: it holds the `.live` lock,
/// closes every connection whose first byte is `Q` (a bootstrap query the old keeper parses as
/// a broken HELLO) and proxies every other connection to a real keeper started under an
/// alternate `TMPDIR` (same binary, so the same hash and socket name).
struct OldKeeperStub {
    stop: Arc<AtomicBool>,
    queries: Arc<AtomicUsize>,
    accept: Option<thread::JoinHandle<()>>,
    real_attacher: Child,
    _live: fs::File,
}

impl OldKeeperStub {
    fn start(remote: &Remote) -> OldKeeperStub {
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        let alt_tmp = remote.base.join("alt-tmp");
        fs::create_dir_all(&alt_tmp).expect("alt tmp");
        let remote_dir = remote.base.join("remote");
        let real_attacher = Command::new(remote.base.join("bin/herdr"))
            .args(["remote-client-bridge", "--x-mosh-session-v1"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", remote_dir.join("home"))
            .env("TMPDIR", &alt_tmp)
            .env("XDG_CONFIG_HOME", remote_dir.join("config"))
            .env("XDG_RUNTIME_DIR", remote_dir.join("runtime"))
            .env("XDG_STATE_HOME", remote_dir.join("runtime/state"))
            .env("HERDR_SOCKET_PATH", &remote.api)
            .env("HERDR_MOSH_TRACE", remote.trace_dir().join("keeper"))
            .env("SHELL", "/bin/sh")
            // Held open: the attacher stays attached until the stub is dropped.
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the real keeper through an attacher");
        let alt_dir = alt_tmp.join(format!("herdr-mosh-{euid}"));
        let real_socket = {
            let mut found = None;
            assert!(
                wait_for(READY, || {
                    found = fs::read_dir(&alt_dir).ok().and_then(|entries| {
                        entries.flatten().map(|e| e.path()).find(|p| {
                            p.extension().is_some_and(|ext| ext == "sock")
                                && UnixStream::connect(p).is_ok()
                        })
                    });
                    found.is_some()
                }),
                "the alternate keeper never listened"
            );
            found.expect("socket")
        };
        let name = real_socket.file_name().expect("socket name").to_owned();

        let dir = remote.rendezvous_dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("rendezvous dir");
        let live = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(Path::new(&dir).join(Path::new(&name).with_extension("live")))
            .expect("live lock file");
        // SAFETY: flock(2) on a descriptor this function owns.
        let rc = unsafe { libc::flock(live.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the stub could not take the .live lock");
        let listener = UnixListener::bind(dir.join(&name)).expect("bind the stub socket");
        listener.set_nonblocking(true).expect("nonblocking");

        let stop = Arc::new(AtomicBool::new(false));
        let queries = Arc::new(AtomicUsize::new(0));
        let accept = {
            let (stop, queries) = (stop.clone(), queries.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((conn, _)) => {
                            let (real_socket, queries) = (real_socket.clone(), queries.clone());
                            thread::spawn(move || serve_old_keeper(conn, &real_socket, &queries));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(20)),
                    }
                }
            })
        };
        OldKeeperStub {
            stop,
            queries,
            accept: Some(accept),
            real_attacher,
            _live: live,
        }
    }
}

impl Drop for OldKeeperStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let _ = self.real_attacher.kill();
        let _ = self.real_attacher.wait();
    }
}

fn serve_old_keeper(mut conn: UnixStream, real_socket: &Path, queries: &AtomicUsize) {
    let _ = conn.set_nonblocking(false);
    let mut first = [0u8; 1];
    if conn.read_exact(&mut first).is_err() {
        return;
    }
    if first[0] == b'Q' {
        queries.fetch_add(1, Ordering::SeqCst);
        return; // closed without a reply
    }
    let Ok(mut upstream) = UnixStream::connect(real_socket) else {
        return;
    };
    if upstream.write_all(&first).is_err() {
        return;
    }
    let (Ok(mut conn_in), Ok(mut upstream_in)) = (conn.try_clone(), upstream.try_clone()) else {
        return;
    };
    let up = thread::spawn(move || {
        let _ = std::io::copy(&mut conn_in, &mut upstream);
        let _ = upstream.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream_in, &mut conn);
    let _ = conn.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

#[test]
fn old_keeper_bootstrap_none_plain_ssh_mosh_leg() {
    let remote = Remote::start();
    remote.set_knob(knob::QUIC, "");
    let stub = OldKeeperStub::start(&remote);
    let mut client = remote.spawn_remote_client(&ClientOptions::default());
    client.wait_attached(&remote, READY);
    assert!(
        client.wait_event("welcome", 1, READY),
        "no mosh session: {}",
        context(&remote, &client, None)
    );
    assert!(
        stub.queries.load(Ordering::SeqCst) >= 1,
        "the attacher never sent a bootstrap query to the old keeper"
    );
    let legs = remote.session_legs(&client.id);
    assert_eq!(legs.len(), 1, "{legs:?}");
    assert!(
        legs[0].contains("--x-mosh-quic-v1"),
        "leg 0 asked for a bootstrap: {legs:?}"
    );
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        1,
        "the plain ssh mosh leg is the session: {}",
        context(&remote, &client, None)
    );
    assert_echo(&remote, &mut client, None, "qold1");
    assert_output_works(&remote, &client, None, "OLDOUT");
}

// ---- wall-clock acceptance tests (run with --run-ignored all) ---------------------------

/// The keeper-side `leg_up` events so far (one per admitted leg, QUIC or ssh).
fn keeper_legs_up(remote: &Remote) -> usize {
    remote.keeper_event_count("leg_up")
}

/// `true` once `client`'s output grows by 64 KiB within `window`.
fn flood_flows(client: &Client, window: Duration) -> bool {
    let from = client.output_len();
    wait_for(window, || client.output_len() >= from + 64 * 1024)
}

fn bound(rtts: u32) -> Duration {
    RTT * rtts + RENDER
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn fallback_at_full_loss_within_ssh_time_plus_3_5s() {
    let _lock = timing_lock();
    // What a session costs over plain ssh on this host: the median of three attaches, each on
    // a primed keeper like the QUIC run below (`ssh -G` shows no hostname, so no QUIC attempt).
    let mut ssh_times = Vec::new();
    for _ in 0..3 {
        let remote = Remote::start();
        remote.quic_bootstrap_line();
        let t0 = Instant::now();
        let client = remote.spawn_remote_client(&ClientOptions::default());
        client.wait_attached(&remote, READY);
        ssh_times.push(t0.elapsed());
    }
    ssh_times.sort();
    let ssh_time = ssh_times[1];

    let remote = Remote::start();
    let relay = remote.start_quic_relay(3);
    relay.set_loss(1.0, 1.0);
    let t0 = Instant::now();
    let mut client = remote.spawn_remote_client(&ClientOptions::default());
    client.wait_attached(&remote, READY);
    let fallback = t0.elapsed();
    eprintln!("ssh attach {ssh_times:?}; attach at 100% loss {fallback:?}");
    assert!(
        fallback <= ssh_time + Duration::from_millis(3500),
        "attach at 100% loss took {fallback:?}, ssh time {ssh_time:?} + 3.5 s: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        1,
        "the session runs on the ssh leg: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(
        relay.stats().forwarded_down,
        0,
        "nothing got through the relay"
    );
    // The session tried QUIC first: the relay dropped its handshake datagrams, and leg 0 ran in
    // QUIC bootstrap mode.
    assert!(
        relay.stats().dropped_up > 0,
        "no QUIC attempt reached the relay: {}",
        context(&remote, &client, Some(&relay))
    );
    assert!(
        remote.session_legs(&client.id)[0].contains("--x-mosh-quic-v1"),
        "leg 0 did not bootstrap QUIC: {:?}",
        remote.session_legs(&client.id)
    );
    assert_echo(&remote, &mut client, Some(&relay), "qfall1");
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn address_change_echo_within_2rtt_plus_render() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(4);
    relay.set_delay(ONE_WAY);
    let opts = ClientOptions {
        size: DENSE_SIZE,
        ..ClientOptions::default()
    };
    let mut client = connect_quic(&remote, &relay, &opts);
    assert_echo(&remote, &mut client, Some(&relay), "qbase1");
    let upstream = relay.upstream_addr();

    relay.rebind();
    assert_ne!(relay.upstream_addr(), upstream, "the relay did not rebind");
    let latency = echo_latency(&mut client, "qmove1", READY).unwrap_or_else(|| {
        panic!(
            "no echo after the rebind: {}",
            context(&remote, &client, Some(&relay))
        )
    });
    eprintln!(
        "echo after the address change: {latency:?} (bound {:?})",
        bound(2)
    );
    assert!(
        latency <= bound(2),
        "echo {latency:?} > 2 RTT + render {:?}: {}",
        bound(2),
        context(&remote, &client, Some(&relay))
    );
    // The same QUIC connection migrated: no new leg, no ssh.
    assert_eq!(remote.session_legs(&client.id).len(), 1);
    assert_eq!(client.event_count("welcome"), 1, "{:?}", client.events());
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(remote.keeper_event_count("leg_down"), 0);

    // Under a dense flood the stream keeps flowing across a second address change.
    remote.pane_run(DENSE_FLOOD);
    assert!(
        flood_flows(&client, READY),
        "the flood never reached the client: {}",
        context(&remote, &client, Some(&relay))
    );
    relay.rebind();
    assert!(
        flood_flows(&client, Duration::from_secs(10)),
        "the flood stalled after the address change: {}",
        context(&remote, &client, Some(&relay))
    );
    // Stop the flood; a keystroke typed after the rebind can only echo over the migrated path.
    client.type_keys("\x03");
    assert!(
        client.wait_screen("MARKER", READY),
        "the flood did not stop: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_echo(&remote, &mut client, Some(&relay), "qmove2");
    // Still the same QUIC connection: no new leg, no ssh, no leg_down on either side.
    assert_eq!(remote.session_legs(&client.id).len(), 1);
    assert_eq!(client.event_count("welcome"), 1, "{:?}", client.events());
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(remote.keeper_event_count("leg_down"), 0);
    assert_eq!(remote.ssh_session_children_alive(&client.id), 0);
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn short_idle_gap_same_quic_leg() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(5);
    relay.set_delay(ONE_WAY);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::grace(60));
    assert_echo(&remote, &mut client, Some(&relay), "qidle0");

    relay.set_loss(1.0, 1.0);
    thread::sleep(Duration::from_secs(20));
    relay.set_loss(0.0, 0.0);
    let t_restore = Instant::now();
    client.type_keys("qidle1");
    let seen = wait_until(Duration::from_secs(60), Duration::from_millis(10), || {
        client.screen().contains("qidle1")
    });
    let latency = t_restore.elapsed();
    // Parked escalation (design roast r1): QUIC PTO backoff against the 2 RTT bound.
    eprintln!(
        "short idle gap: restore to first echo {latency:?} (2 RTT + render = {:?})",
        bound(2)
    );
    assert!(
        seen,
        "no echo after the gap: {}",
        context(&remote, &client, Some(&relay))
    );
    assert!(
        latency <= bound(2),
        "echo {latency:?} > 2 RTT + render {:?}: {}",
        bound(2),
        context(&remote, &client, Some(&relay))
    );
    // The same leg survived: no new leg, no ssh process, and the keeper never saw the leg go.
    assert_eq!(
        remote.session_legs(&client.id).len(),
        1,
        "{}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(remote.ssh_session_children_alive(&client.id), 0);
    assert_eq!(client.event_count("welcome"), 1, "{:?}", client.events());
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(
        remote.keeper_event_count("leg_down"),
        0,
        "the keeper entered grace: {:?}",
        remote.keeper_events()
    );
    assert_eq!(keeper_legs_up(&remote), 1);
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn long_idle_gap_redials_quic_with_cached_bootstrap() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(6);
    relay.set_delay(ONE_WAY);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::grace(120));
    assert_echo(&remote, &mut client, Some(&relay), "qlong0");
    let legs_before = keeper_legs_up(&remote);

    // A dead network: UDP is dropped and ssh cannot connect either, so the only way back is a
    // QUIC redial from the cached bootstrap and token (an ssh leg would have to fail).
    remote.set_knob(knob::SSH_DOWN, "");
    relay.set_loss(1.0, 1.0);
    thread::sleep(Duration::from_secs(30));
    relay.rebind();
    thread::sleep(Duration::from_secs(30));
    relay.set_loss(0.0, 0.0);
    let t_restore = Instant::now();
    client.type_keys("qlong1");
    let seen = wait_until(Duration::from_secs(60), Duration::from_millis(10), || {
        client.screen().contains("qlong1")
    });
    let latency = t_restore.elapsed();
    let limit = bound(3) + BACKOFF_STEP + HANDSHAKE_BOUND;
    eprintln!("long idle gap: restore to first echo {latency:?} (limit {limit:?})");
    assert!(
        seen,
        "no echo after the gap: {}",
        context(&remote, &client, Some(&relay))
    );
    assert!(
        latency <= limit,
        "echo {latency:?} > 3 RTT + backoff step + handshake bound + render {limit:?}: {}",
        context(&remote, &client, Some(&relay))
    );
    // A new QUIC leg was admitted (ssh was down the whole time), and no ssh child lives.
    assert!(
        keeper_legs_up(&remote) > legs_before,
        "no new leg: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(remote.ssh_session_children_alive(&client.id), 0);
    assert!(client.event_count("leg_down") >= 1, "{:?}", client.events());
    assert!(client.has_welcome("resumed"), "{:?}", client.events());
    assert!(
        !format!("{:?}", remote.keeper_events()).contains("quic-token"),
        "the redial was refused: {:?}",
        remote.keeper_events()
    );
}

/// Block UDP until the session continues on an ssh reconnect leg (the second session leg is
/// alive); `keep_typing` runs on every poll.
fn block_udp_until_ssh_fallback(
    remote: &Remote,
    relay: &UdpRelay,
    client: &mut Client,
    mut keep_typing: impl FnMut(&mut Client),
) {
    relay.set_loss(1.0, 1.0);
    let deadline = Instant::now() + Duration::from_secs(90);
    while !(remote.session_legs(&client.id).len() >= 2
        && remote.ssh_session_children_alive(&client.id) == 1
        && client.has_welcome("resumed"))
    {
        assert!(
            Instant::now() < deadline,
            "no ssh fallback after the UDP block: {}",
            context(remote, client, Some(relay))
        );
        keep_typing(client);
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn mid_session_udp_block_falls_back_then_returns_to_quic() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(7);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::grace(120));
    assert_echo(&remote, &mut client, Some(&relay), "qblk0");

    // UDP is blocked for longer than the silence window: the reconnect falls back to ssh.
    block_udp_until_ssh_fallback(&remote, &relay, &mut client, |_| {});
    assert_echo(&remote, &mut client, Some(&relay), "qblk1");
    let legs = remote.session_legs(&client.id);
    assert!(
        legs[1].contains("--x-mosh-quic-v1"),
        "the fallback leg refreshes the bootstrap: {legs:?}"
    );

    // UDP returns; the next reconnect dial goes back to QUIC with the bound token.
    relay.set_loss(0.0, 0.0);
    let up_before = keeper_legs_up(&remote);
    let relay_before = relay.stats().forwarded_up;
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        wait_for(READY, || keeper_legs_up(&remote) > up_before
            && remote.ssh_session_children_alive(&client.id) == 0),
        "the session did not return to QUIC: {}",
        context(&remote, &client, Some(&relay))
    );
    assert!(relay.stats().forwarded_up > relay_before);
    assert_eq!(
        remote.session_legs(&client.id).len(),
        2,
        "the redial started an ssh leg: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_echo(&remote, &mut client, Some(&relay), "qblk2");
    assert!(
        !format!("{:?}", remote.keeper_events()).contains("quic-token"),
        "the bound token was refused: {:?}",
        remote.keeper_events()
    );
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn exactly_once_input_across_fallback_and_redial() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(8);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::grace(120));
    let file = remote.base.join("seq.txt");
    remote.pane_run("cat > seq.txt");
    assert!(
        wait_for(READY, || file.exists()),
        "cat never started: {}",
        context(&remote, &client, Some(&relay))
    );

    let mut sent = 0u32;
    let mut type_line = |client: &mut Client| {
        sent += 1;
        client.type_keys(&format!("L{sent:05}\r"));
    };
    for _ in 0..20 {
        type_line(&mut client);
        thread::sleep(Duration::from_millis(100));
    }
    // Across the QUIC -> ssh fallback: input keeps flowing while the UDP path is dead.
    block_udp_until_ssh_fallback(&remote, &relay, &mut client, |client| {
        type_line(client);
        thread::sleep(Duration::from_millis(50));
    });
    for _ in 0..30 {
        type_line(&mut client);
        thread::sleep(Duration::from_millis(100));
    }
    // Across the redial: the ssh leg is killed and the session goes back to QUIC.
    relay.set_loss(0.0, 0.0);
    let up_before = keeper_legs_up(&remote);
    remote.signal_leg(&client.id, libc::SIGKILL);
    let deadline = Instant::now() + READY;
    while !(keeper_legs_up(&remote) > up_before
        && remote.ssh_session_children_alive(&client.id) == 0)
    {
        assert!(
            Instant::now() < deadline,
            "the session did not return to QUIC: {}",
            context(&remote, &client, Some(&relay))
        );
        type_line(&mut client);
        thread::sleep(Duration::from_millis(100));
    }
    for _ in 0..30 {
        type_line(&mut client);
        thread::sleep(Duration::from_millis(100));
    }
    let total = sent;
    client.type_keys("\x04");

    let expected: String = (1..=total).map(|n| format!("L{n:05}\n")).collect();
    let converged = wait_for(READY, || {
        fs::read_to_string(&file).is_ok_and(|got| got == expected)
    });
    let got = fs::read_to_string(&file).unwrap_or_default();
    assert!(
        converged,
        "the remote did not receive the {total} lines exactly once: got {} bytes, expected {}\nfirst difference at {:?}\n{}",
        got.len(),
        expected.len(),
        got.bytes().zip(expected.bytes()).position(|(a, b)| a != b),
        context(&remote, &client, Some(&relay))
    );
}
