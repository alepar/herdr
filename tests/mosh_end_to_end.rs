//! Phases 3-4 end-to-end scenario (mosh-19g.7, R15): one QUIC leg through `UdpRelay` at
//! 150 ms one way and 10 % loss, supersede on (the default capabilities) and
//! `HERDR_MOSH_PREDICT=on`. Typing shows predictions, a dense flood keeps the screen
//! advancing, a relay rebind mid-run resumes on the same QUIC connection, and the client
//! never disconnects.
//!
//! Wall-clock test: `#[ignore]`d (run with `--run-ignored all`) and takes `timing_lock()`.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::thread;
use std::time::Duration;

use serde_json::Value;

use mosh_harness::{timing_lock, wait_for, Client, ClientOptions, Remote, UdpRelay};

const READY: Duration = Duration::from_secs(30);
const ONE_WAY: Duration = Duration::from_millis(150);
const RTT: Duration = Duration::from_millis(300);
/// A dense flood: `base64 /dev/urandom` redraws every cell, so herdr sends full screen diffs.
/// ^C prints MARKER (split in the source, so the typed command line does not contain it).
const DENSE_FLOOD: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)"#;
const DENSE_SIZE: (u16, u16) = (400, 100);

enum Attempt {
    Done,
    /// The no-ack timeout turned prediction off for the connection (by design at 10 % loss).
    Disabled,
}

fn context(remote: &Remote, client: &Client, relay: &UdpRelay) -> String {
    let predict = client.predict_events();
    format!(
        "screen:\n{}\nlocal: {:?}\npredict (last 16): {:?}\nkeeper: {:?}\nsession legs: {:?}\nssh alive: {}\nrelay: {:?}",
        client.screen(),
        client.events(),
        &predict[predict.len().saturating_sub(16)..],
        remote.keeper_events(),
        remote.session_legs(&client.id),
        remote.ssh_session_children_alive(&client.id),
        relay.stats(),
    )
}

/// `true` once the bridge subscribed to echo acks and then reported a Linked frame with a
/// known SRTT: the facade has everything `on` mode needs to predict.
fn predicting_ready(client: &Client) -> bool {
    let events = client.events();
    let Some(subscribed) = events.iter().position(|e| e["event"] == "echo_subscribe") else {
        return false;
    };
    events[subscribed..]
        .iter()
        .any(|e| e["event"] == "inject" && e["state"] == "linked" && e["srtt_ms"].is_u64())
}

fn predict_count(client: &Client, event: &str, key: &str, value: Value) -> usize {
    client
        .predict_events()
        .iter()
        .filter(|e| e["event"] == event && e[key] == value)
        .count()
}

/// `true` once the client's output grows by 64 KiB within `window`.
fn flood_flows(client: &Client, window: Duration) -> bool {
    let from = client.output_len();
    wait_for(window, || client.output_len() >= from + 64 * 1024)
}

fn assert_no_disconnect(remote: &Remote, client: &mut Client, relay: &UdpRelay) {
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

fn attempt(seed: u64) -> Attempt {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(seed);
    relay.set_delay(ONE_WAY);
    let mut opts = ClientOptions {
        size: DENSE_SIZE,
        ..ClientOptions::default()
    };
    opts.env.push(("HERDR_MOSH_PREDICT".into(), "on".into()));
    let mut client = remote.spawn_remote_client(&opts);
    client.wait_attached(&remote, READY);

    // The session runs over QUIC: leg 0's ssh child is gone and the relay carries datagrams.
    assert!(
        wait_for(READY, || remote.ssh_session_children_alive(&client.id) == 0),
        "the leg-0 ssh child is still alive: {}",
        context(&remote, &client, &relay)
    );
    let stats = relay.stats();
    assert!(
        stats.forwarded_up > 0 && stats.forwarded_down > 0,
        "the session did not use QUIC: {}",
        context(&remote, &client, &relay)
    );
    assert!(
        wait_for(READY, || predicting_ready(&client)),
        "no echo + SRTT report: {}",
        context(&remote, &client, &relay)
    );
    thread::sleep(Duration::from_millis(300));
    relay.set_loss(0.1, 0.1);

    // Typing shows predictions: the first key is painted locally, well before 1 RTT.
    let t0 = std::time::Instant::now();
    client.type_keys("z");
    let seen = client.wait_screen("z", READY);
    let latency = t0.elapsed();
    assert!(seen, "no echo: {}", context(&remote, &client, &relay));
    if predict_count(&client, "judge", "result", "timeout".into()) > 0 {
        return Attempt::Disabled;
    }
    assert!(
        predict_count(&client, "tap", "predicted", true.into()) >= 1,
        "no predicted tap: {}",
        context(&remote, &client, &relay)
    );
    assert!(
        latency < RTT,
        "the first key took {latency:?}, not a local prediction: {}",
        context(&remote, &client, &relay)
    );
    client.type_keys("\x15");
    assert!(
        wait_for(READY, || !client.screen().contains('z')),
        "the line was not cleared: {}",
        context(&remote, &client, &relay)
    );

    // Output keeps advancing under a dense flood.
    remote.pane_run(DENSE_FLOOD);
    assert!(
        flood_flows(&client, READY),
        "the flood never reached the client: {}",
        context(&remote, &client, &relay)
    );

    // A relay rebind mid-run resumes on the same QUIC connection: no new leg, no ssh.
    let upstream = relay.upstream_addr();
    relay.rebind();
    assert_ne!(relay.upstream_addr(), upstream, "the relay did not rebind");
    assert!(
        flood_flows(&client, READY),
        "the flood stalled after the rebind: {}",
        context(&remote, &client, &relay)
    );
    assert_eq!(remote.session_legs(&client.id).len(), 1);
    assert_eq!(client.event_count("welcome"), 1, "{:?}", client.events());
    assert_eq!(client.event_count("leg_down"), 0, "{:?}", client.events());
    assert_eq!(remote.keeper_event_count("leg_down"), 0);
    assert_eq!(remote.ssh_session_children_alive(&client.id), 0);

    // Stop the flood; the session is still interactive after the rebind.
    client.type_keys("\x03");
    assert!(
        client.wait_screen("MARKER", READY),
        "the flood did not stop: {}",
        context(&remote, &client, &relay)
    );
    client.type_keys("e2e");
    assert!(
        client.wait_screen("e2e", READY),
        "no echo after the rebind: {}",
        context(&remote, &client, &relay)
    );
    assert_no_disconnect(&remote, &mut client, &relay);
    eprintln!(
        "end to end: relay {:?}; first-key echo {latency:?}; predicted taps {}",
        relay.stats(),
        predict_count(&client, "tap", "predicted", true.into())
    );
    Attempt::Done
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn quic_prediction_supersede_rebind_end_to_end() {
    // At 10 % loss a late ack can trip the no-ack timeout, which turns prediction off for the
    // connection by design; retry on a fresh session, as the Phase 4 loss test does.
    for seed in 0..6 {
        if matches!(attempt(71 + seed), Attempt::Done) {
            return;
        }
    }
    panic!("prediction was turned off by the no-ack timeout in all 6 attempts");
}
