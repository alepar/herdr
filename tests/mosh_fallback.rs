//! Plain-bridge paths and keeper lifecycle over a fake ssh (mosh-dqw.9): capability-less
//! remote, kill switch, leg-0 fallbacks, ControlMaster bypass and cross-version keepers.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;
pub mod support;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use mosh_harness::{knob, Client, ClientOptions, Remote};

const READY: Duration = Duration::from_secs(30);

fn wait(predicate: impl FnMut() -> bool) -> bool {
    mosh_harness::wait_for(READY, predicate)
}

fn context(remote: &Remote, client: &Client) -> String {
    format!(
        "screen:\n{}\nlocal: {:?}\nkeeper: {:?}\nsession legs: {:?}\nplain: {:?}",
        client.screen(),
        client.events(),
        remote.keeper_events(),
        remote.session_legs(&client.id),
        remote.plain_bridges(&client.id)
    )
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Remote,
    Saved,
}

const MODES: [Mode; 2] = [Mode::Remote, Mode::Saved];

fn spawn(remote: &Remote, mode: Mode, opts: &ClientOptions) -> Client {
    match mode {
        Mode::Remote => remote.spawn_remote_client(opts),
        Mode::Saved => remote.spawn_saved_client(opts),
    }
}

/// Connect, then prove input reaches the remote pane.
fn connect(remote: &Remote, mode: Mode, opts: &ClientOptions) -> Client {
    let mut client = spawn(remote, mode, opts);
    client.wait_attached(remote, READY);
    let tag = format!("{mode:?}-{}", client.id);
    assert!(
        remote.wait_mark(&mut client, &tag, READY),
        "{mode:?}: {}",
        context(remote, &client)
    );
    client
}

/// Position of the first ssh argv line satisfying `pick`.
fn first(lines: &[String], pick: impl Fn(&str) -> bool) -> Option<usize> {
    lines.iter().position(|line| pick(line))
}

/// The connection tried a session leg first and then served itself through the plain bridge.
fn assert_fell_back(remote: &Remote, client: &mut Client, mode: Mode) {
    let argv = remote.ssh_argv(&client.id);
    let session = first(&argv, |l| l.contains("--x-mosh-session-v1")).unwrap_or_else(|| {
        panic!(
            "{mode:?}: no session leg attempted: {}",
            context(remote, client)
        )
    });
    let plain = first(&argv, |l| {
        l.contains("remote-client-bridge") && !l.contains("--x-mosh-")
    })
    .unwrap_or_else(|| panic!("{mode:?}: no plain bridge: {}", context(remote, client)));
    assert!(
        session < plain,
        "{mode:?}: plain bridge ran before the session leg"
    );
    assert!(!client.exited(), "{mode:?}: {}", context(remote, client));
    assert_eq!(
        client.event_count("welcome"),
        0,
        "{mode:?}: a mosh session was established"
    );
}

#[test]
fn capability_less_remote_behaves_like_upstream() {
    for mode in MODES {
        let remote = Remote::start();
        remote.set_knob(knob::STRIP_CAPABILITY, "");
        let client = connect(&remote, mode, &ClientOptions::default());
        assert!(
            remote.session_legs(&client.id).is_empty(),
            "{mode:?}: {}",
            context(&remote, &client)
        );
        assert!(
            !remote.plain_bridges(&client.id).is_empty(),
            "{mode:?}: {}",
            context(&remote, &client)
        );
        assert!(
            client.events().is_empty(),
            "{mode:?}: mosh bridge ran: {:?}",
            client.events()
        );
        assert!(
            remote.keeper_pids().is_empty(),
            "{mode:?}: a keeper was started"
        );
        assert!(remote.keeper_events().is_empty());
    }
}

#[test]
fn kill_switch_behaves_like_upstream() {
    for mode in MODES {
        let remote = Remote::start();
        let opts = ClientOptions {
            resume: false,
            ..ClientOptions::default()
        };
        let client = connect(&remote, mode, &opts);
        assert!(
            remote.session_legs(&client.id).is_empty(),
            "{mode:?}: {}",
            context(&remote, &client)
        );
        assert!(
            !remote.plain_bridges(&client.id).is_empty(),
            "{mode:?}: {}",
            context(&remote, &client)
        );
        assert!(
            client.events().is_empty(),
            "{mode:?}: mosh bridge ran: {:?}",
            client.events()
        );
        assert!(
            remote.keeper_pids().is_empty(),
            "{mode:?}: a keeper was started"
        );
    }
}

#[test]
fn unwritable_rendezvous_dir_falls_back_to_the_plain_bridge() {
    for mode in MODES {
        let remote = Remote::start();
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        let dir = remote.remote_tmp().join(format!("herdr-mosh-{euid}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut client = connect(&remote, mode, &ClientOptions::default());
        assert_fell_back(&remote, &mut client, mode);
        assert!(remote.keeper_pids().is_empty(), "{mode:?}");
    }
}

#[test]
fn keeper_spawn_failure_falls_back_to_the_plain_bridge() {
    for mode in MODES {
        let remote = Remote::start();
        // The attacher starts with a process limit of 1, so it cannot fork the keeper.
        remote.set_knob(knob::SPAWN_FAIL, "");
        let mut client = connect(&remote, mode, &ClientOptions::default());
        assert_fell_back(&remote, &mut client, mode);
        assert!(remote.keeper_pids().is_empty(), "{mode:?}");
    }
}

#[test]
fn keeper_that_never_listens_falls_back_after_the_wait() {
    let remote = Remote::start();
    // Learn the rendezvous names from a first keeper, then leave its lock held by a process
    // that never listens: "a keeper is alive but not accepting".
    let mut first = connect(&remote, Mode::Remote, &ClientOptions::default());
    let keeper = remote.keeper_pids();
    assert_eq!(keeper.len(), 1);
    first.type_keys("\x02q");
    assert!(first.wait_exit(READY));
    for pid in &keeper {
        // SAFETY: pid found by command line and environment as this remote's keeper.
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    assert!(wait(|| remote.keeper_pids().is_empty()));
    let live = std::fs::read_dir(
        remote
            .remote_tmp()
            .join(format!("herdr-mosh-{}", unsafe { libc::geteuid() })),
    )
    .unwrap()
    .flatten()
    .map(|e| e.path())
    .find(|p| p.extension().is_some_and(|x| x == "live"))
    .expect("keeper live lock file");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&live)
        .unwrap();
    // SAFETY: flock on an fd this test owns; held until the end of the test.
    assert_eq!(
        unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&held),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        },
        0
    );

    let started = std::time::Instant::now();
    let mut client = connect(&remote, Mode::Remote, &ClientOptions::default());
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "fell back before the 5 s wait: {:?}",
        started.elapsed()
    );
    assert_fell_back(&remote, &mut client, Mode::Remote);
    drop(held);
}

#[test]
fn downgraded_remote_falls_back_to_the_plain_bridge() {
    for mode in MODES {
        let remote = Remote::start();
        // The remote still advertises the capability but rejects the session flag.
        remote.set_knob(knob::DOWNGRADE, "");
        let mut client = connect(&remote, mode, &ClientOptions::default());
        assert_fell_back(&remote, &mut client, mode);
    }
}

#[test]
fn keeper_refusals_before_welcome_fall_back_to_the_plain_bridge() {
    // `protocol-version`: a keeper speaking another session-protocol version (R20).
    // `server-connect-failed`: the keeper could not reach the server. The keeper side of both is
    // unit-tested (keeper.rs); a live keeper cannot be made to fail its server connect because
    // the attacher restarts a stopped server first, so the END is injected on the leg.
    for reason in ["protocol-version", "server-connect-failed"] {
        for mode in MODES {
            let remote = Remote::start();
            remote.arm_fake_end(reason);
            let mut client = connect(&remote, mode, &ClientOptions::default());
            assert_fell_back(&remote, &mut client, mode);
        }
    }
}

#[test]
fn reconnect_legs_never_use_the_control_master_and_auth_failure_stops_retrying() {
    reconnect_legs_bypass_the_master_then_fail(knob::FAIL255);
}

#[test]
fn host_key_failure_stops_retrying_like_an_auth_failure() {
    reconnect_legs_bypass_the_master_then_fail(knob::HOSTKEY255);
}

fn reconnect_legs_bypass_the_master_then_fail(failure: &str) {
    let remote = Remote::start();
    let client = {
        let client = remote.spawn_remote_client(&ClientOptions::grace(60));
        // A user ssh config that asks for a shared master must not leak into reconnect legs.
        client.wait_attached(&remote, READY);
        client
    };
    let ssh_dir = client.home().join(".ssh");
    std::fs::create_dir_all(&ssh_dir).unwrap();
    std::fs::write(
        ssh_dir.join("config"),
        "Host *\n  ControlMaster auto\n  ControlPath /tmp/mosh-test-%C\n  ControlPersist 600\n",
    )
    .unwrap();
    let mut client = client;
    assert!(remote.wait_leg_count(&client.id, 1, READY));
    let leg_zero = remote.session_legs(&client.id);
    assert_eq!(leg_zero.len(), 1);

    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_event("welcome", 2, READY),
        "{}",
        context(&remote, &client)
    );
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_event("welcome", 3, READY),
        "{}",
        context(&remote, &client)
    );
    let legs = remote.session_legs(&client.id);
    assert_eq!(legs.len(), 3, "{legs:?}");
    for leg in &legs[1..] {
        for needle in ["ControlPath=none", "ControlMaster=no", "BatchMode=yes"] {
            assert!(leg.contains(needle), "reconnect leg lacks {needle}: {leg}");
        }
        assert!(
            !leg.split_whitespace().any(|arg| arg == "-S"),
            "reconnect leg names a master socket: {leg}"
        );
        assert!(!leg.contains("ControlMaster=auto"), "{leg}");
    }

    // Authentication and host key failures are fatal at once: no retry storm, today's
    // disconnect path.
    remote.set_knob(failure, "");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_exit(Duration::from_secs(30)),
        "{}",
        context(&remote, &client)
    );
    let attempts = remote.session_legs(&client.id).len();
    assert_eq!(attempts, 4, "one failed attempt, then no retries");
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(
        remote.session_legs(&client.id).len(),
        attempts,
        "retries continued after the auth failure"
    );
    assert!(
        client.output_text().contains("to reattach"),
        "{}",
        client.output_text()
    );
}

// ---- keeper lifecycle (R16, R19) ----------------------------------------------------

#[test]
fn keeper_is_spawned_once_and_reused() {
    let remote = Remote::start();
    let first = connect(&remote, Mode::Remote, &ClientOptions::default());
    let keepers = remote.keeper_pids();
    assert_eq!(
        keepers.len(),
        1,
        "first attach starts the keeper: {:?}",
        remote.keeper_details()
    );
    let second = connect(&remote, Mode::Remote, &ClientOptions::default());
    assert_eq!(
        remote.keeper_pids(),
        keepers,
        "second attach reuses the keeper"
    );
    assert_eq!(remote.keeper_event_count("server_connect"), 2);
    assert!(!first.screen().is_empty() && !second.screen().is_empty());
}

#[test]
fn stale_keeper_socket_is_recovered_after_kill_nine() {
    let remote = Remote::start();
    let mut first = connect(&remote, Mode::Remote, &ClientOptions::default());
    let old = remote.keeper_pids();
    assert_eq!(old.len(), 1);
    // SAFETY: pid found by command line and environment as this remote's keeper.
    assert_eq!(unsafe { libc::kill(old[0], libc::SIGKILL) }, 0);
    assert!(
        first.wait_exit(READY),
        "the session should end with its keeper"
    );
    assert!(wait(|| remote.keeper_pids().is_empty()));

    let second = connect(&remote, Mode::Remote, &ClientOptions::default());
    let new = remote.keeper_pids();
    assert_eq!(
        new.len(),
        1,
        "{} {:?}",
        context(&remote, &second),
        remote.keeper_details()
    );
    assert_ne!(
        new, old,
        "the dead keeper's socket should have been replaced"
    );
    assert!(
        remote.session_legs(&second.id).len() == 1 && remote.plain_bridges(&second.id).is_empty()
    );
}

#[test]
fn concurrent_first_attaches_spawn_one_keeper() {
    let remote = Remote::start();
    let opts = ClientOptions::default();
    let mut clients: Vec<Client> = (0..2).map(|_| remote.spawn_remote_client(&opts)).collect();
    for client in &mut clients {
        client.wait_attached(&remote, READY);
        assert!(remote.wait_leg_count(&client.id, 1, READY));
    }
    assert_eq!(
        remote.keeper_pids().len(),
        1,
        "two first attaches raced into two keepers: {:?}",
        remote.keeper_details()
    );
    assert_eq!(remote.keeper_event_count("server_connect"), 2);
}

#[test]
fn keeper_exits_about_thirty_seconds_after_its_last_session_ends() {
    let remote = Remote::start();
    let mut client = connect(&remote, Mode::Remote, &ClientOptions::default());
    assert_eq!(
        remote.keeper_pids().len(),
        1,
        "{:?}",
        remote.keeper_details()
    );
    client.type_keys("\x02q");
    assert!(client.wait_exit(READY));
    assert!(wait(|| remote.keeper_event_count("session_end") == 1));
    let ended = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(20));
    assert_eq!(
        remote.keeper_pids().len(),
        1,
        "keeper exited before its idle period"
    );
    assert!(
        mosh_harness::wait_for(Duration::from_secs(25), || remote.keeper_pids().is_empty()),
        "keeper still alive {:?} after its last session ended",
        ended.elapsed()
    );
    assert!(
        ended.elapsed() >= Duration::from_secs(25),
        "{:?}",
        ended.elapsed()
    );
}
