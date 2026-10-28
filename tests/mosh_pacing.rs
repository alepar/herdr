//! Phase 2 pacing tests. This file starts with the link-emulation harness self-test
//! (mosh-hex.1); the fixed-window baseline measurement (mosh-hex.3) and the acceptance tests
//! (mosh-hex.5) are added here later.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use mosh_harness::{ClientOptions, Remote};

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
