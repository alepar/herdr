//! End-to-end resume behaviour of `herdr --remote` over a fake ssh (mosh-dqw.9).
//!
//! Each test starts its own remote server and fake ssh (see `mosh_harness`), so they run in
//! parallel. Every wait is bounded.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;
pub mod support;

use std::time::{Duration, Instant};

use mosh_harness::{knob, Client, ClientOptions, Remote};

const READY: Duration = Duration::from_secs(30);

/// window::INITIAL (src/mosh/window.rs).
const KEEPER_RESET_WINDOW: u64 = 256 * 1024;

/// Floods the pane until Ctrl-C; the split `MA RKER` keeps the echoed command line from matching.
const FLOOD_PROBE: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; yes)"#;
const MARKER: &str = "MARKER";

fn attached(remote: &Remote, opts: &ClientOptions) -> Client {
    let client = remote.spawn_remote_client(opts);
    client.wait_attached(remote, READY);
    assert!(
        remote.wait_leg_count(&client.id, 1, READY),
        "no session leg started"
    );
    client
}

fn type_mark(client: &mut Client, n: u32) {
    client.type_mark(&n.to_string());
}

fn marks(remote: &Remote) -> Vec<String> {
    remote.marks()
}

fn mark_count(remote: &Remote, n: u32) -> usize {
    marks(remote)
        .iter()
        .filter(|line| **line == format!("MK{n}"))
        .count()
}

fn fail_context(remote: &Remote, client: &Client) -> String {
    format!(
        "screen:\n{}\nlocal: {:?}\nkeeper: {:?}\nargv: {:?}",
        client.screen(),
        client.events(),
        remote.keeper_events(),
        remote.session_legs(&client.id)
    )
}

#[test]
fn keys_around_a_leg_kill_arrive_once_in_order() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(60));

    type_mark(&mut client, 1);
    assert!(
        wait(|| mark_count(&remote, 1) == 1),
        "{}",
        fail_context(&remote, &client)
    );

    // Flood the pane so the kill lands mid-stream and mid-frame, then type through the outage.
    remote.pane_run("yes ABCDEFGH | head -c 3000000; printf 'FLOOD_%s\\n' DONE");
    assert!(client.wait_screen("ABCDEFGH", READY), "{}", client.screen());
    remote.signal_leg(&client.id, libc::SIGKILL);
    type_mark(&mut client, 2);
    assert!(
        remote.wait_leg_count(&client.id, 2, READY),
        "no reconnect leg"
    );
    assert!(
        wait(|| remote.pane_text().contains("FLOOD_DONE")),
        "flood never finished"
    );
    assert!(
        client.wait_event("welcome", 2, READY),
        "{}",
        fail_context(&remote, &client)
    );
    type_mark(&mut client, 3);

    assert!(
        wait(|| (1..=3).all(|n| mark_count(&remote, n) == 1)),
        "marks not delivered exactly once: {}",
        marks(&remote).join("|")
    );
    assert_eq!(
        marks(&remote),
        ["MK1", "MK2", "MK3"],
        "out of order or duplicated"
    );

    assert!(
        client.has_welcome("resumed"),
        "{}",
        fail_context(&remote, &client)
    );
    assert_eq!(
        remote.keeper_event_count("server_connect"),
        1,
        "a resume must not open a second server connection: {}",
        fail_context(&remote, &client)
    );
    assert_eq!(client.event_count("session_end"), 0);
    assert!(!client.exited());
}

#[test]
fn stopped_leg_is_detected_and_resumed() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(60));
    type_mark(&mut client, 1);
    assert!(wait(|| mark_count(&remote, 1) == 1));

    let stopped = Instant::now();
    remote.signal_leg(&client.id, libc::SIGSTOP);
    assert!(
        client.wait_event("leg_down", 1, Duration::from_secs(30)),
        "silence was never detected: {}",
        fail_context(&remote, &client)
    );
    assert!(
        // The bridge measures silence from its last inbound byte (session.rs
        // SILENCE_TIMEOUT = 8 s), not from the SIGSTOP. That byte is at most one
        // keeper HEARTBEAT_INTERVAL (2 s) older than the stop, so the earliest
        // legitimate verdict is 8 s - 2 s after it; 500 ms is scheduling slack.
        stopped.elapsed() >= Duration::from_secs(8) - Duration::from_secs(2) - Duration::from_millis(500),
        "dead-leg detection fired after {:?}, before the silence bound (8 s from last inbound, minus one 2 s heartbeat)",
        stopped.elapsed()
    );
    assert!(
        client.has_welcome_after_resume(READY),
        "{}",
        fail_context(&remote, &client)
    );
    type_mark(&mut client, 2);
    assert!(
        wait(|| mark_count(&remote, 2) == 1),
        "{}",
        fail_context(&remote, &client)
    );
    assert_eq!(remote.keeper_event_count("server_connect"), 1);
}

#[test]
fn outage_banner_counts_up_and_clears_on_resume() {
    let remote = Remote::start();
    let client = attached(&remote, &ClientOptions::grace(120));

    // Every reconnect leg answers EOF, so the outage lasts until the knob is cleared.
    remote.set_knob(knob::EOF_WELCOME, "");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_screen("unreachable — last contact", READY),
        "no outage banner: {}",
        fail_context(&remote, &client)
    );
    let first = banner_seconds(&client.screen()).expect("banner shows seconds");
    thread_sleep(1200);
    assert!(
        wait(|| banner_seconds(&client.screen()).is_some_and(|n| n > first)),
        "last-contact time did not advance from {first}: {}",
        client.screen()
    );

    remote.clear_knob(knob::EOF_WELCOME);
    assert!(
        client.wait_event("welcome", 2, Duration::from_secs(30)),
        "never resumed"
    );
    assert!(
        mosh_harness::wait_until(Duration::from_secs(2), Duration::from_millis(20), || {
            !client.screen().contains("unreachable")
        }),
        "banner survived the resume: {}",
        client.screen()
    );
    assert!(client.has_welcome("resumed"));
}

#[test]
fn grace_expiry_exits_with_the_reattach_hint() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(5));

    remote.set_knob(knob::EOF_WELCOME, "");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_exit(Duration::from_secs(45)),
        "client hung after grace expiry: {}",
        fail_context(&remote, &client)
    );
    let text = client.output_text();
    assert!(text.contains("to reattach"), "no reattach hint: {text}");
    assert!(client.event_count("session_end") >= 1);
}

fn banner_seconds(screen: &str) -> Option<u64> {
    let tail = screen.split("last contact").nth(1)?;
    tail.trim_start().split('s').next()?.trim().parse().ok()
}

fn wait(predicate: impl FnMut() -> bool) -> bool {
    mosh_harness::wait_for(READY, predicate)
}

fn thread_sleep(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn live_handoff(remote: &Remote) {
    let reply = remote.request(serde_json::json!({
        "id": "handoff", "method": "server.live_handoff", "params": {}
    }));
    assert!(reply.get("error").is_none(), "{reply}");
}

#[test]
fn live_handoff_with_the_leg_up_delivers_server_shutdown() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(60));
    live_handoff(&remote);
    // Upstream behaviour for `--remote`: the client exits on the handoff ServerShutdown. The
    // resumable session must carry that message intact instead of reporting a lost connection.
    assert!(
        client.wait_exit(READY),
        "{}",
        fail_context(&remote, &client)
    );
    let text = client.output_text();
    assert!(
        text.contains("server shut down: live update in progress"),
        "{text}"
    );
    assert!(
        !text.contains("lost connection"),
        "handoff surfaced as ConnectionLost: {text}"
    );
    assert_eq!(remote.keeper_event_count("server_connect"), 1);
}

#[test]
fn live_handoff_while_the_leg_is_down_is_delivered_on_resume() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(120));
    remote.set_knob(knob::EOF_WELCOME, "");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(client.wait_event("leg_down", 1, READY));
    live_handoff(&remote);
    // The keeper sees the server hang up while no leg is attached.
    assert!(
        wait(|| remote
            .keeper_events()
            .iter()
            .any(|e| e["event"] == "leg_down" && e["reason"] == "eof")),
        "keeper never saw the server EOF: {:?}",
        remote.keeper_events()
    );
    assert!(!client.exited(), "client ended while the leg was down");
    remote.clear_knob(knob::EOF_WELCOME);
    assert!(
        client.wait_exit(READY),
        "{}",
        fail_context(&remote, &client)
    );
    let text = client.output_text();
    assert!(
        client.has_welcome("resumed"),
        "{}",
        fail_context(&remote, &client)
    );
    assert!(
        text.contains("server shut down: live update in progress"),
        "{text}"
    );
    assert!(!text.contains("lost connection"), "{text}");
}

#[test]
fn resume_with_a_lost_token_is_unknown_and_opens_no_server_connection() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(60));
    let keepers = remote.keeper_pids();
    assert_eq!(keepers.len(), 1, "{keepers:?}");

    // The keeper dies with the session parked in it: the next leg reaches a fresh keeper.
    // SAFETY: the pid was found by command line and environment as this remote's keeper.
    assert_eq!(unsafe { libc::kill(keepers[0], libc::SIGKILL) }, 0);
    assert!(
        wait(|| remote
            .keeper_events()
            .iter()
            .any(|e| e["event"] == "welcome" && e["status"] == "unknown")),
        "{}",
        fail_context(&remote, &client)
    );
    assert!(
        client.wait_exit(READY),
        "session should end: {}",
        fail_context(&remote, &client)
    );
    assert_eq!(
        remote.keeper_event_count("server_connect"),
        1,
        "an unknown resume opened a server connection: {}",
        fail_context(&remote, &client)
    );
    let new_welcomes = remote
        .keeper_events()
        .iter()
        .filter(|e| e["event"] == "welcome" && e["status"] == "new")
        .count();
    assert_eq!(new_welcomes, 1, "a resume HELLO was answered new");
    assert!(client.output_text().contains("to reattach"));
}

#[test]
fn reconnect_leg_that_stalls_before_welcome_is_retried() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(120));
    // The next leg prints its ready marker and then never answers; the 20 s pre-WELCOME bound
    // must replace it.
    remote.set_knob(knob::STALL_WELCOME, "1");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_event("welcome", 2, Duration::from_secs(60)),
        "{}",
        fail_context(&remote, &client)
    );
    assert!(client.has_welcome("resumed"));
    assert!(
        remote.leg_pids(&client.id).len() >= 3,
        "{:?}",
        remote.leg_pids(&client.id)
    );
    type_mark(&mut client, 1);
    assert!(wait(|| mark_count(&remote, 1) == 1));
}

#[test]
fn reconnect_leg_that_hangs_up_before_welcome_is_retried() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(120));
    remote.set_knob(knob::EOF_WELCOME, "2");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(
        client.wait_event("welcome", 2, Duration::from_secs(60)),
        "{}",
        fail_context(&remote, &client)
    );
    assert!(client.has_welcome("resumed"));
    assert!(
        remote.leg_pids(&client.id).len() >= 4,
        "{:?}",
        remote.leg_pids(&client.id)
    );
    type_mark(&mut client, 1);
    assert!(wait(|| mark_count(&remote, 1) == 1));
}

#[test]
fn idle_healthy_link_keeps_its_leg() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(60));
    let legs = remote.leg_pids(&client.id);
    // Longer than the 8 s silence bound with nothing to say: heartbeats must keep the link up.
    thread_sleep(11_000);
    assert_eq!(
        client.event_count("leg_down"),
        0,
        "{}",
        fail_context(&remote, &client)
    );
    assert_eq!(remote.leg_pids(&client.id), legs, "the leg was replaced");
    type_mark(&mut client, 1);
    assert!(wait(|| mark_count(&remote, 1) == 1));
    assert_eq!(client.event_count("leg_down"), 0);
}

/// One bracketed paste of `bytes` bytes of newline-terminated lines.
///
/// A single multi-megabyte paste is quadratic in the client TUI's own input handling (1 MiB
/// takes about 1 s, 4 MiB about 20 s, 16 MiB minutes) with or without the resumable bridge, so
/// large volumes go in as a series of 1 MiB pastes.
fn paste(client: &mut Client, bytes: usize) {
    let line = format!("{}\n", "0123456789abcdef".repeat(4));
    let block = line.repeat(1024);
    client.type_bytes(b"\x1b[200~");
    let mut sent = 0;
    while sent < bytes {
        let take = block.len().min(bytes - sent);
        client.type_bytes(&block.as_bytes()[..take]);
        sent += take;
    }
    client.type_bytes(b"\x1b[201~");
}

#[test]
fn slow_live_leg_carries_16_mib_of_pastes_without_ending_the_session() {
    let remote = Remote::start();
    // ~1.3 MB/s each way: slower than the pty-to-bridge path, so the 2 MiB window is the limit.
    remote.set_knob(knob::THROTTLE, "");
    let mut client = attached(&remote, &ClientOptions::grace(60));
    const MIB: usize = 1024 * 1024;
    remote.pane_run("stty -echo; cat > pasted");
    thread_sleep(500);
    let started = Instant::now();
    for _ in 0..16 {
        paste(&mut client, MIB);
    }
    let pasted = || std::fs::metadata(remote.base.join("pasted")).map_or(0, |m| m.len() as usize);
    // The line discipline drops a few bytes per paste; the volume must otherwise arrive whole.
    assert!(
        mosh_harness::wait_for(Duration::from_secs(120), || pasted() >= 16 * MIB - MIB / 4),
        "only {} of {} bytes arrived after {:?}: {}",
        pasted(),
        16 * MIB,
        started.elapsed(),
        fail_context(&remote, &client)
    );
    assert_eq!(
        client.event_count("session_end"),
        0,
        "{}",
        fail_context(&remote, &client)
    );
    assert_eq!(
        client.event_count("leg_down"),
        0,
        "{}",
        fail_context(&remote, &client)
    );
    assert!(!client.exited());
    assert_eq!(
        remote.leg_pids(&client.id).len(),
        1,
        "the leg was replaced under load"
    );
}

#[test]
fn a_long_outage_under_a_flood_keeps_the_session_and_ctrl_c_answers_after_resume() {
    let remote = Remote::start();
    let mut client = attached(&remote, &ClientOptions::grace(600));
    remote.set_knob(knob::EOF_WELCOME, "");
    remote.signal_leg(&client.id, libc::SIGKILL);
    assert!(client.wait_event("leg_down", 1, READY));

    // The flood backs up in the server socket, not in the keeper ring.
    remote.pane_run(FLOOD_PROBE);
    thread_sleep(15_000);
    let ended: Vec<_> = remote
        .keeper_events()
        .into_iter()
        .filter(|e| e["event"] == "session_end")
        .collect();
    assert!(
        ended.is_empty(),
        "{ended:?}\n{}",
        fail_context(&remote, &client)
    );
    assert!(!client.exited(), "{}", fail_context(&remote, &client));

    remote.clear_knob(knob::EOF_WELCOME);
    assert!(
        wait(|| client.has_welcome("resumed")),
        "{}",
        fail_context(&remote, &client)
    );
    // Ring bound: at most the reset window queued while down (the slack covers the up-rule read
    // rounding on a leg that came up and died under the EOF_WELCOME knob).
    for event in remote.keeper_events() {
        if event["event"] == "resume" {
            let replayed = event["replayed_down"].as_u64().unwrap_or(u64::MAX);
            assert!(
                replayed <= KEEPER_RESET_WINDOW + 4096,
                "ring held {replayed} while down\n{}",
                fail_context(&remote, &client)
            );
        }
    }

    // The flood reaches the client, and Ctrl-C answers.
    let before = client.output_len();
    assert!(
        wait(|| client.output_len() >= before + 64 * 1024),
        "{}",
        fail_context(&remote, &client)
    );
    client.type_bytes(b"\x03");
    assert!(
        client.wait_screen(MARKER, READY),
        "{}",
        fail_context(&remote, &client)
    );
    assert!(!client.exited(), "{}", fail_context(&remote, &client));
}
