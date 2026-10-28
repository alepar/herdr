//! 3b supersede integration and timing tests (mosh-19g.2.7): render frames on cancellable QUIC
//! streams, the resync path and the mixed-version cases, end to end through the fake ssh, a
//! real keeper and a real herdr server. Spec: 3b supersede design, section 6.
//!
//! The functional tests run in the default suite. Every wall-clock test is `#[ignore]`d (run
//! with `--run-ignored all`), takes the host-wide `timing_lock()` and loads the link with a dense
//! flood (`base64 /dev/urandom` at a large pane: herdr sends screen diffs, so `yes` would not).
//!
//! Observability used here: the bridge traces `resync {reason}` and `render_gap {cause}` into the
//! client's `local.jsonl`; the keeper's `welcome` trace carries `render_split`.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;
pub mod support;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use mosh_harness::{knob, timing_lock, wait_for, Client, ClientOptions, Remote, UdpRelay};
use support::{
    client_shell_handshake, drain_messages, read_server_message,
    CURRENT_ENDPOINT_PROTOCOL_GENERATION, SERVER_MESSAGE_PANE_SURFACE,
    SERVER_MESSAGE_PANE_SURFACE_PATCH,
};

const READY: Duration = Duration::from_secs(30);
const ONE_WAY: Duration = Duration::from_millis(150);
/// A dense flood: `base64 /dev/urandom` redraws every cell, so herdr sends full screen diffs.
/// ^C prints MARKER (split in the source, so the typed command line does not contain it).
const DENSE_FLOOD: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)"#;
const DENSE_SIZE: (u16, u16) = (400, 100);
const SUPERSEDE_CAP: &str = "x-mosh-supersede-v1";
const QUIC_CAP: &str = "x-mosh-quic-v1";

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

/// Client trace lines named `event` whose `key` field equals `value`.
fn events_where(client: &Client, event: &str, key: &str, value: &str) -> usize {
    client
        .events()
        .iter()
        .filter(|line| line["event"] == event && line[key] == value)
        .count()
}

/// `render_split` of every keeper `welcome` that carried the field (one per admitted leg).
fn keeper_welcome_splits(remote: &Remote) -> Vec<bool> {
    remote
        .keeper_events()
        .iter()
        .filter(|line| line["event"] == "welcome")
        .filter_map(|line| line["render_split"].as_bool())
        .collect()
}

/// A client attached over QUIC: the session is up, leg 0's ssh child is gone and the relay
/// carried datagrams. Connect with the relay's loss still at 0: the handshake bound is 3 s.
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

/// A client on an ssh leg.
fn connect_ssh(remote: &Remote, opts: &ClientOptions) -> Client {
    let client = remote.spawn_remote_client(opts);
    client.wait_attached(remote, READY);
    assert!(
        client.wait_event("welcome", 1, READY),
        "no mosh session: {}",
        context(remote, &client, None)
    );
    client
}

/// Run a burst of small in-place updates (a patch stream) in the pane and wait for its last
/// frame and end marker on the client screen.
fn run_burst(remote: &Remote, client: &Client, relay: Option<&UdpRelay>, tag: &str) {
    remote.pane_run(&format!(
        "i=0; while [ $i -lt 300 ]; do printf '\\rPATCH%04d' $i; i=$((i+1)); sleep 0.02; done; \
         echo; echo BURST\"\"DONE_{tag}"
    ));
    assert!(
        client.wait_screen(&format!("BURSTDONE_{tag}"), Duration::from_secs(60)),
        "burst {tag} never finished on the client: {}",
        context(remote, client, relay)
    );
    assert!(
        client.screen().contains("PATCH0299"),
        "the last patch of burst {tag} is missing: {}",
        context(remote, client, relay)
    );
}

fn assert_no_disconnect(remote: &Remote, client: &mut Client, relay: Option<&UdpRelay>) {
    assert!(
        !client.exited(),
        "the client exited: {}",
        context(remote, client, relay)
    );
    assert_eq!(
        client.event_count("session_end"),
        0,
        "{}",
        context(remote, client, relay)
    );
}

fn assert_echo(remote: &Remote, client: &mut Client, relay: Option<&UdpRelay>, probe: &str) {
    client.type_keys(probe);
    assert!(
        client.wait_screen(probe, READY),
        "no echo of {probe}: {}",
        context(remote, client, relay)
    );
    // Clear the input line and wait for it, so a later command is not typed onto the probe.
    client.type_keys("\x15");
    assert!(
        wait_for(READY, || !client.screen().contains(probe)),
        "the input line was not cleared: {}",
        context(remote, client, relay)
    );
}

// ---- default suite ----------------------------------------------------------------------

#[test]
fn patch_burst_under_20pct_loss_on_quic_no_disconnect() {
    let remote = Remote::start();
    let relay = remote.start_quic_relay(21);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::default());
    relay.set_loss(0.2, 0.2);
    run_burst(&remote, &client, Some(&relay), "q");
    let stats = relay.stats();
    assert!(
        stats.dropped_down > 0 && stats.forwarded_down > 0,
        "the burst did not run through a lossy QUIC path: {stats:?}"
    );
    eprintln!(
        "quic burst at 20% loss: {stats:?}; resync {}, render_gap {}",
        client.event_count("resync"),
        client.event_count("render_gap")
    );
    assert_no_disconnect(&remote, &mut client, Some(&relay));
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        0,
        "the session left QUIC: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(keeper_welcome_splits(&remote), vec![true]);
    relay.set_loss(0.0, 0.0);
    assert_echo(&remote, &mut client, Some(&relay), "qburst");
}

#[test]
fn patch_burst_under_20pct_loss_on_ssh_no_disconnect() {
    // An ssh leg has no lane and no UDP: the relay's loss does not touch it. What this covers
    // is the render-split keeper path on a lane-less leg (frames go through the ring).
    let remote = Remote::start();
    let mut client = connect_ssh(&remote, &ClientOptions::default());
    run_burst(&remote, &client, None, "s");
    assert_no_disconnect(&remote, &mut client, None);
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        1,
        "the ssh leg is the session: {}",
        context(&remote, &client, None)
    );
    assert_eq!(keeper_welcome_splits(&remote), vec![true]);
    assert_echo(&remote, &mut client, None, "sburst");
}

/// Block UDP until the session continues on an ssh reconnect leg (the second session leg is
/// alive and welcomed as resumed).
fn block_udp_until_ssh_fallback(remote: &Remote, relay: &UdpRelay, client: &Client) {
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
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn quic_ssh_quic_under_loss_one_resync_per_resume() {
    let remote = Remote::start();
    let relay = remote.start_quic_relay(22);
    let mut client = connect_quic(&remote, &relay, &ClientOptions::grace(120));
    relay.set_loss(0.2, 0.2);
    run_burst(&remote, &client, Some(&relay), "a");
    assert_eq!(
        client.event_count("resync"),
        events_where(&client, "resync", "reason", "gap")
    );

    // QUIC -> ssh: UDP dies; the pane changes while the session is between legs.
    relay.set_loss(1.0, 1.0);
    remote.pane_run("echo OUTAGE\"\"CHANGE_b");
    block_udp_until_ssh_fallback(&remote, &relay, &client);
    assert!(
        client.wait_screen("OUTAGECHANGE_b", READY),
        "the change made during the outage never reached the client: {}",
        context(&remote, &client, Some(&relay))
    );
    run_burst(&remote, &client, Some(&relay), "b");

    // ssh -> QUIC: the ssh leg dies and the redial goes back over the lossy relay. The
    // handshake runs on a clean path (its bound is 3 s); loss returns for the burst.
    relay.set_loss(0.0, 0.0);
    let up_before = remote.keeper_event_count("leg_up");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        wait_for(READY, || remote.keeper_event_count("leg_up") > up_before
            && remote.ssh_session_children_alive(&client.id) == 0),
        "the session did not return to QUIC: {}",
        context(&remote, &client, Some(&relay))
    );
    relay.set_loss(0.2, 0.2);
    run_burst(&remote, &client, Some(&relay), "c");

    assert_no_disconnect(&remote, &mut client, Some(&relay));
    // Two resumes (QUIC -> ssh, ssh -> QUIC), and exactly one resync for each.
    let resumes = events_where(&client, "welcome", "status", "resumed");
    assert_eq!(resumes, 2, "{}", context(&remote, &client, Some(&relay)));
    assert_eq!(
        events_where(&client, "resync", "reason", "resume"),
        resumes,
        "one resync per resume: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(keeper_welcome_splits(&remote), vec![true, true, true]);
    eprintln!(
        "leg change: resync resume {}, gap {}; render_gap {}",
        resumes,
        events_where(&client, "resync", "reason", "gap"),
        client.event_count("render_gap")
    );
    relay.set_loss(0.0, 0.0);
    assert_echo(&remote, &mut client, Some(&relay), "qsq");
}

#[test]
fn stripped_supersede_capability_no_split() {
    // QUIC leg, capability stripped: a lane exists but nothing asks for the split.
    {
        let remote = Remote::start();
        let relay = remote.start_quic_relay(23);
        remote.set_knob(knob::STRIP_CAPS, SUPERSEDE_CAP);
        let mut client = connect_quic(&remote, &relay, &ClientOptions::default());
        relay.set_loss(0.2, 0.2);
        run_burst(&remote, &client, Some(&relay), "q");
        relay.set_loss(0.0, 0.0);
        assert_echo(&remote, &mut client, Some(&relay), "qstripped");
        assert_eq!(keeper_welcome_splits(&remote), vec![false]);
        assert_eq!(client.event_count("resync"), 0, "{:?}", client.events());
        assert_eq!(client.event_count("render_gap"), 0, "{:?}", client.events());
        assert_no_disconnect(&remote, &mut client, Some(&relay));
    }
    // ssh leg through a resume: Phase 1 behaviour, no resync even on the resume.
    {
        let remote = Remote::start();
        remote.set_knob(knob::STRIP_CAPS, SUPERSEDE_CAP);
        let mut client = connect_ssh(&remote, &ClientOptions::default());
        run_burst(&remote, &client, None, "s");
        remote.signal_leg(&client.id, libc::SIGKILL);
        assert!(
            client.has_welcome_after_resume(READY),
            "no resume: {}",
            context(&remote, &client, None)
        );
        assert_echo(&remote, &mut client, None, "sstripped");
        assert_eq!(keeper_welcome_splits(&remote), vec![false, false]);
        assert_eq!(client.event_count("resync"), 0, "{:?}", client.events());
        assert_eq!(client.event_count("render_gap"), 0, "{:?}", client.events());
        assert_no_disconnect(&remote, &mut client, None);
    }
}

#[test]
fn old_hello_without_render_split_admitted_as_non_split() {
    let remote = Remote::start();
    let token = "00112233445566778899aabbccddeeff";
    let json =
        format!(r#"{{"v":1,"token":"{token}","resume":false,"recv_offset":0,"grace_ms":60000}}"#);
    assert!(!json.contains("render_split"));
    let mut hello = vec![1u8];
    hello.extend_from_slice(&(json.len() as u32).to_le_bytes());
    hello.extend_from_slice(json.as_bytes());

    // The attacher is the session leg a pre-3b bridge would run; its stdin takes the old HELLO.
    let herdr = remote.base.join("bin/herdr");
    let mut child = Command::new(remote.base.join("bin/ssh"))
        .args(["-T", "test-only"])
        .arg(format!(
            "exec '{}' remote-client-bridge --x-mosh-session-v1",
            herdr.display()
        ))
        .env("FAKE_SSH_CLIENT", "old")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the attacher");
    let mut stdin = child.stdin.take().expect("attacher stdin");
    let mut stdout = child.stdout.take().expect("attacher stdout");
    let received = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let received = received.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                received.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
    }
    stdin.write_all(&hello).expect("write the HELLO");
    stdin.flush().expect("flush the HELLO");
    let welcomed = wait_for(READY, || {
        String::from_utf8_lossy(&received.lock().unwrap()).contains(r#""status":"new""#)
    });
    let answer = String::from_utf8_lossy(&received.lock().unwrap()).into_owned();
    let welcomes = remote
        .keeper_events()
        .into_iter()
        .filter(|line| line["event"] == "welcome" && line["token"] == token)
        .collect::<Vec<_>>();
    let _ = child.kill();
    let _ = child.wait();
    assert!(welcomed, "no WELCOME(new) for the old HELLO: {answer:?}");
    assert_eq!(welcomes.len(), 1, "{welcomes:?}");
    assert_eq!(welcomes[0]["status"], "new");
    assert_eq!(
        welcomes[0]["render_split"], false,
        "an old HELLO must be admitted as non-split: {welcomes:?}"
    );
}

#[test]
fn plain_client_receives_no_fork_frames() {
    let remote = Remote::start();
    remote.set_knob(knob::TAP_PLAIN, "");
    // A mosh client with the split on, and a plain (kill-switch) bridge on the same server.
    let mut mosh = connect_ssh(&remote, &ClientOptions::default());
    let plain = remote.spawn_remote_client(&ClientOptions {
        resume: false,
        ..ClientOptions::default()
    });
    plain.wait_attached(&remote, READY);
    assert!(
        !remote.plain_bridges(&plain.id).is_empty() && remote.session_legs(&plain.id).is_empty(),
        "the second client is not a plain bridge: {:?}",
        remote.ssh_argv(&plain.id)
    );

    // Drive the fork's frames on the mosh client: a patch burst, then a leg kill whose resume
    // makes the bridge send x-mosh.surface-resync (and the server answer it).
    run_burst(&remote, &mosh, None, "p");
    remote.signal_leg(&mosh.id, libc::SIGKILL);
    assert!(
        mosh.has_welcome_after_resume(READY),
        "no resume: {}",
        context(&remote, &mosh, None)
    );
    assert!(
        wait_for(READY, || events_where(&mosh, "resync", "reason", "resume")
            == 1),
        "the mosh client never resynced: {}",
        context(&remote, &mosh, None)
    );
    assert_echo(&remote, &mut mosh, None, "mproof");
    remote.pane_run("echo PLAIN\"\"PROOF");
    assert!(
        plain.wait_screen("PLAINPROOF", READY),
        "the plain client stopped rendering: {}",
        plain.screen()
    );

    let tap = remote.plain_tap();
    // The tap must be capturing the plain client's protocol stream: its endpoint handshake
    // answer is in clear text.
    assert!(
        tap.windows(b"endpoint.welcome.v1".len())
            .any(|w| w == b"endpoint.welcome.v1"),
        "the tap holds no plain client handshake ({} bytes)",
        tap.len()
    );
    for kind in ["x-mosh.surface-resync", "x-mosh.echo-ack"] {
        assert!(
            !tap.windows(kind.len()).any(|w| w == kind.as_bytes()),
            "the plain client received a {kind} frame"
        );
    }
}

#[test]
fn resync_brings_a_full_surface() {
    // A render-split session resumes after the pane changed under it; the resume resync makes
    // the server repaint the whole client surface, so the change shows and nothing breaks.
    let remote = Remote::start();
    let mut client = connect_ssh(&remote, &ClientOptions::grace(120));
    assert_echo(&remote, &mut client, None, "rsbase");
    remote.signal_leg(&client.id, libc::SIGKILL);
    remote.pane_run("echo CHANGED\"\"WHILE_AWAY");
    assert!(
        client.has_welcome_after_resume(READY),
        "no resume: {}",
        context(&remote, &client, None)
    );
    assert!(
        client.wait_screen("CHANGEDWHILE_AWAY", READY),
        "the repaint after the resync never showed the change: {}",
        context(&remote, &client, None)
    );
    assert!(
        wait_for(READY, || events_where(
            &client, "resync", "reason", "resume"
        ) == 1),
        "{}",
        context(&remote, &client, None)
    );
    assert_echo(&remote, &mut client, None, "rsafter");
    assert_no_disconnect(&remote, &mut client, None);
    assert_eq!(keeper_welcome_splits(&remote), vec![true, true]);
}

/// First server message of `variants` within `timeout`.
fn next_of(stream: &mut UnixStream, variants: &[u32], timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    while let Some(left) = deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
    {
        // The read timeout spans a whole frame's wait, so a frame is never torn by a timeout.
        stream.set_read_timeout(Some(left)).expect("read timeout");
        if let Ok((variant, _)) = read_server_message(stream) {
            if variants.contains(&variant) {
                return Some(variant);
            }
        }
    }
    None
}

/// The `x-mosh.surface-resync.v1` client frame: `EndpointControl { kind, data: "{}" }`
/// (variant 20), length prefixed, bincode varints.
fn wire_resync_frame() -> Vec<u8> {
    let kind = b"x-mosh.surface-resync.v1";
    let mut payload = vec![20u8, kind.len() as u8];
    payload.extend_from_slice(kind);
    payload.extend_from_slice(&[2, b'{', b'}']);
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(&payload);
    frame
}

#[test]
fn wire_resync_frame_yields_a_pane_surface() {
    // The bead-2.2 acceptance at the wire: a client sends the resync frame and its next render
    // is a full PaneSurface (tag 13), not a patch (tag 19).
    let remote = Remote::start();
    let mut stream = UnixStream::connect(remote.base.join("remote/runtime/herdr-client.sock"))
        .expect("connect the client socket");
    let (_, refused) =
        client_shell_handshake(&mut stream, CURRENT_ENDPOINT_PROTOCOL_GENERATION, 100, 30)
            .expect("shell handshake");
    assert_eq!(refused, None, "the server refused the endpoint hello");
    assert!(
        next_of(
            &mut stream,
            &[
                SERVER_MESSAGE_PANE_SURFACE,
                SERVER_MESSAGE_PANE_SURFACE_PATCH
            ],
            READY
        )
        .is_some(),
        "the server never rendered to the shell client"
    );
    drain_messages(&mut stream);

    // Without a resync a small pane change arrives as a patch.
    remote.pane_run("echo WIRE\"\"ONE");
    let before = next_of(
        &mut stream,
        &[
            SERVER_MESSAGE_PANE_SURFACE,
            SERVER_MESSAGE_PANE_SURFACE_PATCH,
        ],
        READY,
    );
    assert_eq!(
        before,
        Some(SERVER_MESSAGE_PANE_SURFACE_PATCH),
        "the baseline render is not a patch, so the test could not tell a resync from none"
    );
    drain_messages(&mut stream);

    stream
        .write_all(&wire_resync_frame())
        .expect("send the resync frame");
    let after = next_of(
        &mut stream,
        &[
            SERVER_MESSAGE_PANE_SURFACE,
            SERVER_MESSAGE_PANE_SURFACE_PATCH,
        ],
        READY,
    );
    assert_eq!(
        after,
        Some(SERVER_MESSAGE_PANE_SURFACE),
        "the render after the resync is not a full PaneSurface"
    );
}

// ---- wall-clock tests (run with --run-ignored all) --------------------------------------

fn attach_dense(remote: &Remote, relay: Option<&UdpRelay>) -> Client {
    let opts = ClientOptions {
        size: DENSE_SIZE,
        ..ClientOptions::grace(120)
    };
    match relay {
        Some(relay) => connect_quic(remote, relay, &opts),
        None => connect_ssh(remote, &opts),
    }
}

/// Start the dense flood and wait (up to 60 s) for its first frames to reach the client. A flood
/// that never arrives is not asserted here: the measurements that follow report it.
fn start_flood(remote: &Remote, client: &Client) {
    remote.pane_run(DENSE_FLOOD);
    let from = client.output_len();
    if !wait_for(Duration::from_secs(60), || {
        client.output_len() >= from + 16 * 1024
    }) {
        eprintln!("TIMING warning: the flood had not reached the client after 60 s");
    }
}

/// Over `secs` seconds: seconds in which the client output did not grow, and seconds in which
/// a fresh screen snapshot equalled the previous one. Returns (stalled, seconds).
fn watch_screen_changes(client: &Client, secs: usize) -> (usize, usize) {
    let mut stalled = 0;
    let mut previous = client.screen();
    for _ in 0..secs {
        let second = Instant::now();
        let from = client.output_len();
        let mut grew = false;
        while second.elapsed() < Duration::from_secs(1) {
            grew |= client.output_len() > from;
            thread::sleep(Duration::from_millis(20));
        }
        let now = client.screen();
        eprintln!(
            "TIMING second: output +{} bytes, screen changed: {}",
            client.output_len() - from,
            now != previous
        );
        if !grew || now == previous {
            stalled += 1;
        }
        previous = now;
    }
    (stalled, secs)
}

/// Type ^C into the flood and return the time until the client screen shows MARKER. Output
/// length is sampled every 2 ms; once a (slow) screen check sees the marker, a binary search
/// over output prefixes finds the first prefix that shows it, so parse time never inflates
/// the measurement.
fn marker_latency(client: &mut Client, timeout: Duration) -> Option<Duration> {
    let from = client.output_len();
    let t0 = Instant::now();
    client.type_keys("\x03");
    let mut samples: Vec<(Duration, usize)> = Vec::new();
    let mut next_check = Instant::now() + Duration::from_millis(100);
    while t0.elapsed() < timeout {
        samples.push((t0.elapsed(), client.output_len()));
        if Instant::now() >= next_check {
            if client.screen().contains("MARKER") {
                let (mut lo, mut hi) = (from, client.output_len());
                while lo + 1 < hi {
                    let mid = lo + (hi - lo) / 2;
                    if client.screen_prefix(mid).contains("MARKER") {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                return samples
                    .iter()
                    .find(|(_, len)| *len >= hi)
                    .map(|(at, _)| *at);
            }
            next_check = Instant::now() + Duration::from_millis(100);
        }
        thread::sleep(Duration::from_millis(2));
    }
    None
}

fn trace_numbers(client: &Client) -> (usize, usize, usize) {
    (
        events_where(client, "resync", "reason", "gap"),
        events_where(client, "resync", "reason", "resume"),
        client.event_count("render_gap"),
    )
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn typed_marker_visible_within_2s_while_flooding() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(31);
    let mut client = attach_dense(&remote, Some(&relay));
    relay.set_delay(ONE_WAY);
    relay.set_loss(0.1, 0.1);
    start_flood(&remote, &client);
    // Let the flood and any loss recovery settle before the marker is typed.
    thread::sleep(Duration::from_secs(5));
    let latency = marker_latency(&mut client, Duration::from_secs(20));
    let (gap, resume, gaps) = trace_numbers(&client);
    // Parked escalation (design roast r1): the full-PaneSurface delivery after a gap shows in
    // this latency when a resync is pending; log it with the gap activity of the run.
    eprintln!(
        "TIMING marker echo under flood, QUIC 10% loss / 150 ms one-way: {latency:?} (bound 2 s); \
         resync gap {gap}, resume {resume}, render_gap {gaps}; relay {:?}",
        relay.stats()
    );
    assert!(
        latency.is_some_and(|l| l <= Duration::from_secs(2)),
        "marker echo {latency:?} > 2 s: {}",
        context(&remote, &client, Some(&relay))
    );
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn screen_changes_every_second_over_20s() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(32);
    let mut client = attach_dense(&remote, Some(&relay));
    relay.set_delay(ONE_WAY);
    relay.set_loss(0.1, 0.1);
    start_flood(&remote, &client);
    let (stalled, seconds) = watch_screen_changes(&client, 20);
    let (gap, resume, gaps) = trace_numbers(&client);
    eprintln!(
        "TIMING screen changes, QUIC 10% loss / 150 ms one-way: {stalled} of {seconds} seconds \
         without a change; resync gap {gap}, resume {resume}, render_gap {gaps}; relay {:?}",
        relay.stats()
    );
    client.type_keys("\x03");
    assert_eq!(
        stalled,
        0,
        "{stalled} of {seconds} seconds had no screen change: {}",
        context(&remote, &client, Some(&relay))
    );
}

#[test]
#[ignore = "wall-clock comparison; run with --run-ignored all"]
fn ssh_only_comparison_logged() {
    // The same scenario on a Phase 1 ssh-only leg (no QUIC, no split), 150 ms one-way and a
    // 1.3 MB/s bottleneck. Numbers are logged for comparison; nothing is asserted on them.
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_knob(knob::STRIP_CAPS, &format!("{QUIC_CAP} {SUPERSEDE_CAP}"));
    remote.set_link(ONE_WAY.as_millis() as u64, 1_300_000);
    let mut client = attach_dense(&remote, None);
    start_flood(&remote, &client);
    let (stalled, seconds) = watch_screen_changes(&client, 20);
    thread::sleep(Duration::from_secs(5));
    let latency = marker_latency(&mut client, Duration::from_secs(20));
    eprintln!("TIMING ssh-only comparison (Phase 1 leg, 150 ms one-way, 1.3 MB/s): marker echo {latency:?}; \
         {stalled} of {seconds} seconds without a screen change");
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn zero_overdue_gaps_at_0pct_loss_over_20s() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(33);
    let mut client = attach_dense(&remote, Some(&relay));
    relay.set_delay(ONE_WAY);
    start_flood(&remote, &client);
    let from = client.output_len();
    thread::sleep(Duration::from_secs(20));
    let grown = client.output_len() - from;
    let overdue = events_where(&client, "render_gap", "cause", "overdue");
    let (gap, resume, gaps) = trace_numbers(&client);
    eprintln!(
        "TIMING 0% loss flood over QUIC, 20 s: {grown} bytes; overdue gaps {overdue}; \
         render_gap {gaps} (resync gap {gap}, resume {resume}); relay {:?}",
        relay.stats()
    );
    client.type_keys("\x03");
    assert!(grown > 64 * 1024, "the flood did not flow: {grown} bytes");
    assert_eq!(relay.stats().dropped_down, 0);
    assert_eq!(
        overdue,
        0,
        "overdue-declared gaps at 0% loss: {}",
        context(&remote, &client, Some(&relay))
    );
}
