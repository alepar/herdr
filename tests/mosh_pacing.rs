//! Phase 2 pacing tests. This file starts with the link-emulation harness self-test
//! (mosh-hex.1), the fixed-window baseline measurement helpers and ignored evidence tests
//! (mosh-hex.3), and the Phase 2 acceptance tests (mosh-hex.5).

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use mosh_harness::{Client, ClientOptions, Remote};

const READY: Duration = Duration::from_secs(30);
const MIB: u64 = 1024 * 1024;

/// A scratch directory for the delay-line script, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("mosh-link-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// `/bin/sh -c <pipeline>` with piped stdin and stdout.
fn spawn_pipeline(pipeline: &str) -> Child {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(pipeline)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn delay_line_round_trip_pays_both_one_way_delays() {
    let scratch = Scratch::new("rtt");
    let line = quoted(&mosh_harness::write_delay_line(&scratch.0));
    let mut child = spawn_pipeline(&format!("perl {line} 150 {MIB} | perl {line} 150 {MIB}"));
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut byte = [0u8; 1];
    for i in 0..3 {
        let sent = Instant::now();
        stdin.write_all(b"x").unwrap();
        stdin.flush().unwrap();
        stdout.read_exact(&mut byte).unwrap();
        let rtt = sent.elapsed();
        assert!(
            rtt >= Duration::from_millis(300),
            "round trip {i} took {rtt:?}"
        );
        // The first ping also pays perl start-up; later ones are only the line.
        if i > 0 {
            assert!(
                rtt < Duration::from_millis(600),
                "round trip {i} took {rtt:?}"
            );
        }
    }
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn delay_line_sustains_its_configured_rate() {
    let scratch = Scratch::new("rate");
    let line = quoted(&mosh_harness::write_delay_line(&scratch.0));
    let total = 3 * MIB as usize;
    let mut child = spawn_pipeline(&format!("perl {line} 150 {MIB}"));
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let writer = thread::spawn(move || {
        let block = vec![b'y'; 64 * 1024];
        for _ in 0..total / block.len() {
            stdin.write_all(&block).unwrap();
        }
        // stdin drops here: EOF lets the line drain and exit.
    });
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = 0;
    let mut first: Option<(Instant, usize)> = None;
    while received < total {
        let n = stdout.read(&mut buf).unwrap();
        assert!(n > 0, "delay line closed after {received} bytes");
        received += n;
        if first.is_none() {
            first = Some((Instant::now(), received));
        }
    }
    let last = Instant::now();
    writer.join().unwrap();
    let (first_at, first_bytes) = first.unwrap();
    let measured = (total - first_bytes) as f64 / last.duration_since(first_at).as_secs_f64();
    let rate = MIB as f64;
    assert!(
        (measured - rate).abs() <= 0.2 * rate,
        "measured {measured:.0} B/s, configured {rate:.0} B/s"
    );
    let _ = child.wait();
}

#[test]
fn legs_run_through_the_link_knob_and_clear_link_goes_direct() {
    let remote = Remote::start();
    remote.set_link(150, MIB);

    // A resumable session leg attaches and carries input through the delay line.
    let mut session = remote.spawn_remote_client(&ClientOptions::grace(60));
    session.wait_attached(&remote, READY);
    assert!(
        remote.wait_leg_count(&session.id, 1, READY),
        "no session leg started"
    );
    assert!(
        remote.wait_mark(&mut session, "1", READY),
        "typing did not cross the delayed leg"
    );
    let linked = remote.linked_legs(&session.id);
    assert!(
        linked.len() == 1 && linked[0].starts_with("session "),
        "the session leg bypassed the delay line: {linked:?}"
    );

    // The plain bridge (resume kill switch) goes through it too.
    let mut plain = remote.spawn_remote_client(&ClientOptions {
        resume: false,
        ..ClientOptions::default()
    });
    plain.wait_attached(&remote, READY);
    assert!(
        remote.wait_mark(&mut plain, "2", READY),
        "typing did not cross the delayed bridge"
    );
    assert!(
        remote
            .linked_legs(&plain.id)
            .iter()
            .any(|l| l.starts_with("plain ")),
        "the plain bridge bypassed the delay line: {:?}",
        remote.linked_legs(&plain.id)
    );

    // clear_link: legs started from now on go direct.
    remote.clear_link();
    let mut direct = remote.spawn_remote_client(&ClientOptions::grace(60));
    direct.wait_attached(&remote, READY);
    assert!(
        remote.wait_leg_count(&direct.id, 1, READY),
        "no session leg started"
    );
    assert!(
        remote.wait_mark(&mut direct, "3", READY),
        "typing did not cross the direct leg"
    );
    assert!(
        remote.linked_legs(&direct.id).is_empty(),
        "a leg started after clear_link went through the delay line"
    );
}

// ---- Phase 2 measurement helpers (mosh-hex.3; reused unchanged by mosh-hex.5) ----------

/// The pane command of every Ctrl-C probe. SIGINT ends `yes`, and the subshell's own trap
/// prints the marker: a job-control shell signals only the foreground job, so a shell-level
/// trap never fires. The marker is built by expansion, so the typed line never contains it.
pub const FLOOD_PROBE: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; yes)"#;
pub const MARKER: &str = "MARKER";
/// One-way propagation delay of the slow profile (300 ms RTT).
pub const ONE_WAY_DELAY_MS: u64 = 150;
/// Slow-profile link rate (bytes/s). PROVISIONAL: evidence/phase2-baseline.md (b) records that
/// no rate satisfies the spec rule with the spec probe; 256 KiB/s is the lowest supported rate.
pub const PROFILE_RATE: u64 = 262_144;
/// Terminal size (cols, rows) of every pacing test, chosen in phase2-baseline.md (b).
pub const PROFILE_SIZE: (u16, u16) = (80, 24);

#[test]
fn flood_probe_prints_the_marker_after_ctrl_c_under_interactive_sh() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-i");
    cmd.env_clear();
    cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
    cmd.env("TERM", "dumb");
    cmd.env("PS1", "$ ");
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let output: Arc<Mutex<Vec<u8>>> = Arc::default();
    let sink = Arc::clone(&output);
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let len = || output.lock().unwrap().len();
    writer
        .write_all(format!("{FLOOD_PROBE}\r").as_bytes())
        .unwrap();
    writer.flush().unwrap();
    assert!(
        mosh_harness::wait_for(READY, || len() > 64 * 1024),
        "the flood never started"
    );
    let before = len();
    writer.write_all(b"\x03").unwrap();
    writer.flush().unwrap();
    let marker_after = || {
        let out = output.lock().unwrap();
        String::from_utf8_lossy(&out[before..]).contains(MARKER)
    };
    assert!(
        mosh_harness::wait_for(Duration::from_secs(10), marker_after),
        "no marker after ^C"
    );
    // The typed line (echoed before the flood) must not contain the marker literally.
    let typed = String::from_utf8_lossy(&output.lock().unwrap()[..4096.min(before)]).into_owned();
    assert!(
        !typed.contains(MARKER),
        "the echoed probe line contains the marker: {typed:?}"
    );
    if let Some(pid) = child.process_id() {
        // SAFETY: kill(2) on the shell this test spawned.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
    let _ = child.wait();
}

/// Timing tests take this host-wide lock so concurrent floods (nextest runs every test in
/// its own process) do not skew each other's clocks. Held until dropped.
pub struct TimingLock(#[allow(dead_code)] std::fs::File);

pub fn timing_lock() -> TimingLock {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("herdr-mosh-pacing.lock"))
        .unwrap();
    // SAFETY: flock(2) on a file descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock failed");
    TimingLock(file)
}

/// A client attached to `remote` (its first leg up) with terminal `size`.
pub fn attach(remote: &Remote, resume: bool, size: (u16, u16)) -> Client {
    let client = remote.spawn_remote_client(&ClientOptions {
        resume,
        size,
        ..ClientOptions::default()
    });
    client.wait_attached(remote, READY);
    if resume {
        assert!(
            remote.wait_leg_count(&client.id, 1, READY),
            "no session leg started"
        );
    }
    client
}

/// Wait until `client`'s output has grown for `settle` of continuous flood.
pub fn settle_flood(client: &Client, settle: Duration) {
    let start = client.output_len();
    assert!(
        mosh_harness::wait_for(READY, || client.output_len() >= start + 64 * 1024),
        "the flood is not reaching the client"
    );
    let grown = client.output_len();
    thread::sleep(settle);
    assert!(
        client.output_len() > grown,
        "the flood stalled while settling"
    );
}

/// Start the probe flood in the remote pane (through the API, not the client) and settle it.
/// The client must already be attached.
pub fn start_flood(remote: &Remote, client: &Client, settle: Duration) {
    remote.pane_run(FLOOD_PROBE);
    settle_flood(client, settle);
}

/// Client-side delivered bytes/s (terminal output growth) over `period`.
pub fn client_rate(client: &Client, period: Duration) -> f64 {
    let (bytes, at) = (client.output_len(), Instant::now());
    thread::sleep(period);
    (client.output_len() - bytes) as f64 / at.elapsed().as_secs_f64()
}

/// Write `\x03` into the client pty and return the time until the client screen first shows
/// MARKER. Output length is sampled every 2 ms while the screen is checked every 50 ms; once
/// it shows the marker, a binary search over output prefixes finds the first prefix that
/// shows it, and the result is the first sample at or past that prefix, so parse time never
/// inflates the measurement. Panics after `timeout`.
pub fn ctrl_c_latency(client: &mut Client, timeout: Duration) -> Duration {
    let from = client.output_len();
    assert!(
        !client.screen_prefix(from).contains(MARKER),
        "a previous marker is still on screen"
    );
    client.type_bytes(b"\x03");
    let t0 = Instant::now();
    let mut samples = vec![(Duration::ZERO, from)];
    let mut checked = t0;
    let found = loop {
        let len = client.output_len();
        samples.push((t0.elapsed(), len));
        if checked.elapsed() >= Duration::from_millis(50) {
            if client.screen_prefix(len).contains(MARKER) {
                break len;
            }
            checked = Instant::now();
        }
        assert!(
            t0.elapsed() < timeout,
            "no {MARKER} within {timeout:?}:\n{}",
            client.screen()
        );
        thread::sleep(Duration::from_millis(2));
    };
    let (mut lo, mut hi) = (from, found);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if client.screen_prefix(mid).contains(MARKER) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    samples
        .iter()
        .find(|(_, len)| *len >= hi)
        .map(|(at, _)| *at)
        .expect("the last sample reached the marker prefix")
}

/// `ss -xpn` rows of the `keepers` processes: (state, recv_q, send_q, local address).
fn keeper_unix_sockets(keepers: &[libc::pid_t]) -> Vec<(String, u64, u64, String)> {
    let tags: Vec<String> = keepers.iter().map(|pid| format!("pid={pid},")).collect();
    let Ok(out) = Command::new("ss").arg("-xpn").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| tags.iter().any(|tag| line.contains(tag.as_str())))
        .filter_map(|line| {
            // Netid State Recv-Q Send-Q Local-Address Local-Inode Peer-Address Peer-Inode Process
            let f: Vec<&str> = line.split_whitespace().collect();
            Some((
                f.get(1)?.to_string(),
                f.get(2)?.parse().ok()?,
                f.get(3)?.parse().ok()?,
                f.get(4)?.to_string(),
            ))
        })
        .collect()
}

/// The backlog metric: Recv-Q of the keeper's server stream (its connected, unbound `*`
/// socket to the herdr server; the keeper's leg sockets are accepted on its rendezvous path):
/// bytes the server wrote that the keeper has not read, the same quantity as FIONREAD.
pub fn keeper_server_backlog(keepers: &[libc::pid_t]) -> Option<u64> {
    keeper_unix_sockets(keepers)
        .into_iter()
        .filter(|(state, _, _, local)| state == "ESTAB" && local == "*")
        .map(|(_, recv_q, _, _)| recv_q)
        .max()
}

/// Send-Q of the keeper's leg sockets: bytes the keeper wrote that the attacher has not read
/// (a lower bound on the downstream bytes the keeper holds unacked).
pub fn keeper_leg_send_q(keepers: &[libc::pid_t]) -> Option<u64> {
    keeper_unix_sockets(keepers)
        .into_iter()
        .filter(|(state, _, _, local)| state == "ESTAB" && local != "*")
        .map(|(_, _, send_q, _)| send_q)
        .max()
}

/// Sample `probe` every 100 ms on a background thread until the returned guard is stopped.
pub struct Sampler {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<Vec<u64>>>,
}

impl Sampler {
    pub fn start(probe: impl Fn() -> Option<u64> + Send + 'static) -> Sampler {
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut samples = Vec::new();
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                samples.extend(probe());
                thread::sleep(Duration::from_millis(100));
            }
            samples
        });
        Sampler {
            stop,
            handle: Some(handle),
        }
    }

    pub fn finish(mut self) -> Vec<u64> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle
            .take()
            .map(|h| h.join().unwrap())
            .unwrap_or_default()
    }
}

/// p-th percentile (0..=100) of `values` (nearest rank); 0 when empty.
pub fn percentile(values: &[u64], p: usize) -> u64 {
    let mut v = values.to_vec();
    v.sort_unstable();
    v.get((v.len().max(1) - 1) * p / 100).copied().unwrap_or(0)
}

/// (a) and (a2): LAN flood rate (client-side bytes/s, fixed window) per terminal size, and
/// the keeper's leg Send-Q under it. Evidence only:
/// `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture baseline_lan`
#[test]
#[ignore = "evidence only (mosh-hex.3); run by hand"]
fn baseline_lan_flood_rate_per_terminal_size() {
    let _lock = timing_lock();
    for size in [(80, 24), (120, 40), (200, 60)] {
        let remote = Remote::start();
        let mut client = attach(&remote, true, size);
        let keepers = remote.keeper_pids();
        assert!(!keepers.is_empty(), "no keeper after attach");
        let leg_q = Sampler::start(move || keeper_leg_send_q(&keepers));
        start_flood(&remote, &client, Duration::from_secs(2));
        let rate = client_rate(&client, Duration::from_secs(10));
        let leg = leg_q.finish();
        let _ = ctrl_c_latency(&mut client, Duration::from_secs(30));
        eprintln!(
            "size {size:?}: flood {rate:.0} B/s; keeper leg Send-Q max {} p95 {}",
            leg.iter().max().copied().unwrap_or(0),
            percentile(&leg, 95)
        );
    }
}

/// (c) and (d): Ctrl-C latency at the profile with the Phase 1 fixed window (>= 3 runs) and
/// the keeper's server-stream Recv-Q sampled during each flood. Evidence only:
/// `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture baseline_ctrl_c`
#[test]
#[ignore = "evidence only (mosh-hex.3); run by hand"]
fn baseline_ctrl_c_latency_at_the_profile() {
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_link(ONE_WAY_DELAY_MS, PROFILE_RATE);
    let mut client = attach(&remote, true, PROFILE_SIZE);
    for run in 1..=3 {
        let keepers = remote.keeper_pids();
        let backlog = Sampler::start(move || keeper_server_backlog(&keepers));
        start_flood(&remote, &client, Duration::from_secs(2));
        let latency = ctrl_c_latency(&mut client, Duration::from_secs(60));
        let samples = backlog.finish();
        eprintln!(
            "run {run}: ctrl-c {:.0} ms; server backlog p50 {} p95 {} max {} bytes ({} samples)",
            latency.as_secs_f64() * 1000.0,
            percentile(&samples, 50),
            percentile(&samples, 95),
            samples.iter().max().copied().unwrap_or(0),
            samples.len()
        );
        thread::sleep(Duration::from_secs(2));
    }
}

// ---- Phase 2 acceptance (mosh-hex.5) ----------------------------------------------------

/// Mirrors of `src/mosh/window.rs` (`FLOOR`, `INITIAL`) and `session::MAX_DATA_CHUNK`; test
/// crates cannot import crate-private items. Test (3) asserts the first traced window equals
/// `INITIAL`, which catches drift.
const FLOOR: u64 = 128 * 1024;
const INITIAL: u64 = 256 * 1024;
const CHUNK: u64 = 64 * 1024;
/// Amended bound (bead mosh-hex.5 amendment 2026-10-28): the spec's 1.5 s goal is recorded as
/// an agent check in the evidence file, not asserted.
const CTRL_C_BOUND: Duration = Duration::from_millis(2500);
/// Median Ctrl-C latency with the fixed 2 MiB window at the same profile, measured once at
/// `b9ea542` (evidence/phase2-acceptance.md). Test (1) requires <= 0.35 x this.
const FIXED_WINDOW_CTRL_C_MS: u64 = 9182;
/// Terminal size of the dense-flood tests: wide enough that herdr's screen diffs exceed R.
const DENSE_SIZE: (u16, u16) = (400, 100);

/// A content-dense flood that saturates the profile link (about 508 KiB/s on a LAN at
/// `DENSE_SIZE`, about 1.98 x R), unlike `FLOOD_PROBE` (`yes` redraws a uniform screen, so
/// herdr sends about 3 KB/s). Same trap construction as `FLOOD_PROBE`.
const SATURATING_FLOOD: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)"#;

/// Keeper `window` events, oldest first.
fn window_events(remote: &Remote) -> Vec<serde_json::Value> {
    remote
        .keeper_events()
        .into_iter()
        .filter(|line| line["event"] == "window")
        .collect()
}

/// The convergence bound: `max(FLOOR, 2 * R * 0.3 s)` plus one chunk.
fn window_bound(rate: u64) -> u64 {
    FLOOR.max(rate * 6 / 10) + CHUNK
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// `wchar` of `/proc/<pid>/io`.
#[cfg(target_os = "linux")]
fn wchar(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/io"))
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("wchar: ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// The fake ssh of the client's newest linked leg.
#[cfg(target_os = "linux")]
fn linked_ssh_pid(remote: &Remote, client: &str) -> u32 {
    remote
        .linked_legs(client)
        .last()
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .expect("no linked leg")
}

/// Direct children of `parent`.
#[cfg(target_os = "linux")]
fn children_of(parent: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            stat.rsplit(')')
                .next()
                .and_then(|r| r.split_whitespace().nth(1))
                == Some(parent.to_string().as_str())
        })
        .collect()
}

/// The downstream delay-line process of the client's newest linked leg: the child of the
/// fake ssh whose stdout is the same pipe as the ssh's own stdout.
#[cfg(target_os = "linux")]
fn downstream_line_pid(remote: &Remote, client: &str) -> u32 {
    let ssh = linked_ssh_pid(remote, client);
    let out = std::fs::read_link(format!("/proc/{ssh}/fd/1")).expect("ssh stdout");
    children_of(ssh)
        .into_iter()
        .find(|pid| {
            std::fs::read_link(format!("/proc/{pid}/fd/1"))
                .ok()
                .as_ref()
                == Some(&out)
        })
        .expect("no downstream delay line")
}

/// SIGKILL the client's newest linked leg: the pipeline's processes first, then the fake ssh.
/// `Remote::signal_leg` alone would kill only the ssh shell and leave the delay lines holding
/// the client's pipe open, so the client would never see the leg die.
#[cfg(target_os = "linux")]
fn kill_linked_leg(remote: &Remote, client: &str) {
    let ssh = linked_ssh_pid(remote, client);
    for pid in children_of(ssh).into_iter().chain([ssh]) {
        // SAFETY: kill(2) on a process of this test's own fake ssh pipeline.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }
}

/// Start the dense flood in the remote pane (through the API) and settle it.
fn start_dense_flood(remote: &Remote, client: &Client, settle: Duration) {
    remote.pane_run(SATURATING_FLOOD);
    settle_flood(client, settle);
}

#[test]
fn ctrl_c_under_a_dense_flood_is_far_faster_than_the_fixed_window_at_300_ms_rtt() {
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_link(ONE_WAY_DELAY_MS, PROFILE_RATE);
    let mut client = attach(&remote, true, DENSE_SIZE);
    let keepers = remote.keeper_pids();
    let mut latencies = Vec::new();
    for run in 1..=3 {
        start_dense_flood(&remote, &client, Duration::from_secs(2));
        let latency = ctrl_c_latency(&mut client, Duration::from_secs(60));
        let backlog: Vec<u64> = window_events(&remote)
            .iter()
            .filter_map(|l| l["server_backlog"].as_u64())
            .collect();
        eprintln!(
            "run {run}: ctrl-c {:.0} ms; window events {}; server_backlog p95 {} B (staleness {:.0} ms); ss now {:?}",
            latency.as_secs_f64() * 1000.0,
            window_events(&remote).len(),
            percentile(&backlog, 95),
            percentile(&backlog, 95) as f64 * 1000.0 / PROFILE_RATE as f64,
            keeper_server_backlog(&keepers),
        );
        latencies.push(latency.as_millis() as u64);
        thread::sleep(Duration::from_secs(2));
    }
    latencies.sort_unstable();
    let median = latencies[1];
    eprintln!("ctrl-c median {median} ms of {latencies:?} (spec goal 1500 ms)");
    assert!(
        median <= CTRL_C_BOUND.as_millis() as u64,
        "median ctrl-c {median} ms > {CTRL_C_BOUND:?} ({latencies:?})"
    );
    assert!(
        median * 100 <= FIXED_WINDOW_CTRL_C_MS * 35,
        "median ctrl-c {median} ms > 0.35 x fixed-window {FIXED_WINDOW_CTRL_C_MS} ms ({latencies:?})"
    );
}

#[test]
fn lan_dense_flood_throughput_matches_the_plain_bridge() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let mut rates = [Vec::new(), Vec::new()]; // [plain, adaptive]
    for round in 0..2 {
        for (i, resume) in [false, true].into_iter().enumerate() {
            let mut client = attach(&remote, resume, DENSE_SIZE);
            start_dense_flood(&remote, &client, Duration::from_secs(2));
            rates[i].push(client_rate(&client, Duration::from_secs(5)));
            let _ = ctrl_c_latency(&mut client, Duration::from_secs(60));
            eprintln!(
                "round {round} resume={resume}: {:.0} B/s",
                rates[i].last().copied().unwrap_or(0.0)
            );
        }
    }
    let mean = |v: &Vec<f64>| v.iter().sum::<f64>() / v.len() as f64;
    let (plain, adaptive) = (mean(&rates[0]), mean(&rates[1]));
    eprintln!("plain {plain:.0} B/s, adaptive {adaptive:.0} B/s");
    assert!(
        adaptive >= 0.9 * plain,
        "adaptive {adaptive:.0} B/s < 0.9 x plain {plain:.0} B/s"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_window_converges_near_the_bandwidth_delay_product_under_a_dense_flood() {
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_link(ONE_WAY_DELAY_MS, PROFILE_RATE);
    let mut client = attach(&remote, true, DENSE_SIZE);
    let start_ms = now_ms();
    remote.pane_run(SATURATING_FLOOD);
    settle_flood(&client, Duration::from_secs(1));
    let line = downstream_line_pid(&remote, &client.id);
    thread::sleep(Duration::from_secs(10));
    let (w0, t0) = (wchar(line), Instant::now());
    thread::sleep(Duration::from_secs(10));
    let goodput = (wchar(line) - w0) as f64 / t0.elapsed().as_secs_f64();
    let end_ms = now_ms();
    let _ = ctrl_c_latency(&mut client, Duration::from_secs(60));

    let events = window_events(&remote);
    assert_eq!(
        events.first().and_then(|l| l["window"].as_u64()),
        Some(INITIAL)
    );
    let flood: Vec<(u64, u64)> = events
        .iter()
        .filter_map(|l| Some((l["ms"].as_u64()?, l["window"].as_u64()?)))
        .filter(|(ms, _)| *ms >= start_ms && *ms <= end_ms)
        .collect();
    let mut last_5s: Vec<u64> = flood
        .iter()
        .filter(|(ms, _)| *ms + 5_000 >= end_ms)
        .map(|(_, w)| *w)
        .collect();
    last_5s.sort_unstable();
    let median = last_5s.get(last_5s.len() / 2).copied();
    let bound = window_bound(PROFILE_RATE);
    eprintln!(
        "median window {median:?} (bound {bound}); goodput {goodput:.0} B/s of {PROFILE_RATE} ({:.2} R; spec 0.8 R agent check); {} epochs, {} in the last 5 s",
        goodput / PROFILE_RATE as f64,
        flood.len(),
        last_5s.len()
    );
    let median = median.expect("no window event in the last 5 s of the flood");
    assert!(median <= bound, "median window {median} > {bound}");
    assert!(
        events
            .iter()
            .filter_map(|l| l["window"].as_u64())
            .all(|w| w >= FLOOR),
        "window below FLOOR: {events:?}"
    );
    assert!(
        goodput >= 0.6 * PROFILE_RATE as f64,
        "goodput {goodput:.0} B/s < 0.6 R ({})",
        0.6 * PROFILE_RATE as f64
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_leg_killed_mid_dense_flood_resumes_at_initial_and_ctrl_c_still_answers_in_time() {
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_link(ONE_WAY_DELAY_MS, PROFILE_RATE);
    let mut client = attach(&remote, true, DENSE_SIZE);
    start_dense_flood(&remote, &client, Duration::from_secs(2));
    let first_leg = window_events(&remote).last().map(|l| l["leg"].clone());
    kill_linked_leg(&remote, &client.id);
    assert!(
        remote.wait_leg_count(&client.id, 2, READY),
        "no reconnect leg"
    );
    assert!(client.has_welcome_after_resume(READY), "no resumed welcome");
    assert!(
        mosh_harness::wait_for(READY, || window_events(&remote)
            .iter()
            .any(|l| Some(l["leg"].clone()) != first_leg)),
        "no window event on the new leg"
    );
    let new_leg = window_events(&remote)
        .into_iter()
        .find(|l| Some(l["leg"].clone()) != first_leg)
        .expect("a window event on the new leg");
    assert_eq!(new_leg["window"].as_u64(), Some(INITIAL), "{new_leg}");
    settle_flood(&client, Duration::from_secs(2));
    let latency = ctrl_c_latency(&mut client, Duration::from_secs(60));
    eprintln!(
        "after resume: ctrl-c {:.0} ms",
        latency.as_secs_f64() * 1000.0
    );
    assert!(
        latency <= CTRL_C_BOUND,
        "after resume: ctrl-c took {latency:?}"
    );
}

/// Evidence only (mosh-hex.5, closes the mosh-hex.3 "unmeasured saturating backlog" gap):
/// Ctrl-C latency, link goodput and server backlog at the profile rate under a flood that
/// does saturate the link, at 400x100. Run by hand, against either code version:
/// `cargo nextest run --locked --test mosh_pacing --run-ignored ignored-only --no-capture saturating_flood`
#[cfg(target_os = "linux")]
#[test]
#[ignore = "evidence only (mosh-hex.5); run by hand"]
fn saturating_flood_evidence() {
    let _lock = timing_lock();
    let remote = Remote::start();
    remote.set_link(ONE_WAY_DELAY_MS, PROFILE_RATE);
    let mut client = attach(&remote, true, (400, 100));
    let keepers = remote.keeper_pids();
    for run in 1..=3 {
        let from_ms = now_ms();
        let backlog = Sampler::start({
            let keepers = keepers.clone();
            move || keeper_server_backlog(&keepers)
        });
        remote.pane_run(SATURATING_FLOOD);
        settle_flood(&client, Duration::from_secs(2));
        let line = downstream_line_pid(&remote, &client.id);
        let (w0, t0) = (wchar(line), Instant::now());
        thread::sleep(Duration::from_secs(8));
        let goodput = (wchar(line) - w0) as f64 / t0.elapsed().as_secs_f64();
        let latency = ctrl_c_latency(&mut client, Duration::from_secs(60));
        let ss = backlog.finish();
        let traced: Vec<u64> = window_events(&remote)
            .iter()
            .filter(|l| l["ms"].as_u64() >= Some(from_ms))
            .filter_map(|l| l["server_backlog"].as_u64())
            .collect();
        let windows: Vec<u64> = window_events(&remote)
            .iter()
            .filter(|l| l["ms"].as_u64() >= Some(from_ms))
            .filter_map(|l| l["window"].as_u64())
            .collect();
        eprintln!(
            "run {run}: ctrl-c {:.0} ms; goodput {goodput:.0} B/s of {PROFILE_RATE}; window p50 {} max {} ({} epochs); \
             trace backlog p50 {} p95 {} max {}; ss backlog p50 {} p95 {} max {} ({} samples)",
            latency.as_secs_f64() * 1000.0,
            percentile(&windows, 50),
            percentile(&windows, 100),
            windows.len(),
            percentile(&traced, 50),
            percentile(&traced, 95),
            percentile(&traced, 100),
            percentile(&ss, 50),
            percentile(&ss, 95),
            percentile(&ss, 100),
            ss.len(),
        );
        thread::sleep(Duration::from_secs(2));
    }
}
