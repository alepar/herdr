//! Phase 4 prediction integration and timing tests (mosh-19g.3.5): predictive local echo with
//! the server's echo-ack, end to end through the fake ssh, a real keeper, a real herdr server
//! and a real client in a pty. Spec: Phase 4 prediction design, section 5.
//!
//! The functional tests run in the default suite over the harness link knob (150 ms one way,
//! so RTT is 300 ms) with `HERDR_MOSH_PREDICT=on`. Every wall-clock test is `#[ignore]`d (run
//! with `--run-ignored all`) and takes the host-wide `timing_lock()`.
//!
//! Observability: the client facade traces `tap`, `paint` and `judge` into `predict.jsonl`
//! (`Client::predict_events`, wall-clock `ms`); the bridge traces `echo_subscribe` and
//! `inject {state, srtt_ms}` into `local.jsonl`.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use mosh_harness::{
    knob, timing_lock, wait_for, Client, ClientOptions, Remote, SCREEN_COLS, SCREEN_ROWS,
};

const READY: Duration = Duration::from_secs(30);
const ONE_WAY_MS: u64 = 150;
const RTT: Duration = Duration::from_millis(2 * ONE_WAY_MS);
/// Link serialization rate: fast enough that only the delay shapes the typing tests.
const LINK_RATE: u64 = 1024 * 1024;
/// A dense flood: `base64 /dev/urandom` redraws every cell, so herdr sends full screen diffs.
/// ^C prints MARKER (split in the source, so the typed command line does not contain it).
const DENSE_FLOOD: &str = r#"(trap 'printf "%s%s\n" MA RKER; exit' INT; base64 /dev/urandom)"#;
const DENSE_SIZE: (u16, u16) = (400, 100);
/// The server's budget between input receipt and its ack (spec section 2).
const ACK_DEADLINE: Duration = Duration::from_millis(50);
/// What "one render" costs in the clearing bounds: the server's worst case without a render
/// is a second 50 ms step (3.3 notes), plus the client recompose, over a loaded CI host.
const ONE_RENDER: Duration = Duration::from_millis(150);

fn predict_options(mode: Option<&str>, size: (u16, u16)) -> ClientOptions {
    let mut opts = ClientOptions {
        size,
        ..ClientOptions::default()
    };
    if let Some(mode) = mode {
        opts.env.push(("HERDR_MOSH_PREDICT".into(), mode.into()));
    }
    opts
}

fn context(remote: &Remote, client: &Client) -> String {
    format!(
        "screen:\n{}\npane:\n{}\nlocal: {:?}\npredict (last 16): {:?}\nkeeper events: {}",
        client.screen(),
        remote.pane_text(),
        client.events(),
        {
            let events = client.predict_events();
            events[events.len().saturating_sub(16)..].to_vec()
        },
        remote.keeper_events().len(),
    )
}

/// `true` once the bridge subscribed to echo acks and then reported a Linked frame with a
/// known SRTT: the facade now has everything `on` mode needs to predict.
fn predicting_ready(client: &Client) -> bool {
    let events = client.events();
    let Some(subscribed) = events.iter().position(|e| e["event"] == "echo_subscribe") else {
        return false;
    };
    events[subscribed..]
        .iter()
        .any(|e| e["event"] == "inject" && e["state"] == "linked" && e["srtt_ms"].is_u64())
}

/// A client attached over the 150 ms link with `mode`, and (when `mode` predicts) past the
/// first Linked frame that carries the echo flag and an SRTT.
fn attach_linked(remote: &Remote, mode: Option<&str>, size: (u16, u16)) -> Client {
    remote.set_link(ONE_WAY_MS, LINK_RATE);
    let client = remote.spawn_remote_client(&predict_options(mode, size));
    client.wait_attached(remote, READY);
    assert!(
        client.wait_event("welcome", 1, READY),
        "no mosh session: {}",
        context(remote, &client)
    );
    if mode == Some("on") {
        assert!(
            wait_for(READY, || predicting_ready(&client)),
            "the bridge never reported echo + SRTT: {}",
            context(remote, &client)
        );
        // The link frame crosses the in-process channel to the facade.
        thread::sleep(Duration::from_millis(300));
    }
    client
}

fn taps(client: &Client) -> Vec<Value> {
    client
        .predict_events()
        .into_iter()
        .filter(|e| e["event"] == "tap")
        .collect()
}

fn predicted_taps(client: &Client) -> usize {
    taps(client)
        .iter()
        .filter(|e| e["predicted"] == true)
        .count()
}

fn judgments(client: &Client, result: &str) -> usize {
    client
        .predict_events()
        .iter()
        .filter(|e| e["event"] == "judge" && e["result"] == result)
        .count()
}

fn paints(client: &Client) -> usize {
    client
        .predict_events()
        .iter()
        .filter(|e| e["event"] == "paint")
        .count()
}

/// Send `text` + Enter to `pane` through the API (not through any client).
fn pane_run_in(remote: &Remote, pane: &str, command: &str) {
    let reply = remote.request(json!({
        "id": "run", "method": "pane.send_input",
        "params": {"pane_id": pane, "text": command, "keys": ["Enter"]},
    }));
    assert_eq!(reply["result"]["type"], "ok", "{reply}");
}

/// Type `text` one key at a time with `gap` between keys.
fn type_slowly(client: &mut Client, text: &str, gap: Duration) {
    for c in text.chars() {
        client.type_keys(&c.to_string());
        thread::sleep(gap);
    }
}

// ---- default suite ----------------------------------------------------------------------

#[test]
fn typed_text_appears_and_stays_correct() {
    let remote = Remote::start();
    let mut client = attach_linked(&remote, Some("on"), (SCREEN_COLS, SCREEN_ROWS));
    assert_eq!(client.event_count("echo_subscribe"), 1);

    let text = "predicted";
    let started = Instant::now();
    client.type_keys(text);
    // The text shows before any server response could have reached the client: one RTT is
    // the earliest the real echo can arrive.
    assert!(
        wait_for(RTT, || client.screen().contains(text)),
        "the typed text did not appear within one RTT: {}",
        context(&remote, &client)
    );
    assert!(
        started.elapsed() < RTT,
        "the text appeared only after {:?}: {}",
        started.elapsed(),
        context(&remote, &client)
    );
    assert!(
        predicted_taps(&client) >= 1,
        "{}",
        context(&remote, &client)
    );

    // The server's echo arrives, the predictions are confirmed and the text stays.
    assert!(
        wait_for(READY, || judgments(&client, "confirmed") >= 1),
        "no prediction was ever confirmed: {}",
        context(&remote, &client)
    );
    assert!(
        wait_for(READY, || remote.pane_text().contains(text)),
        "the server never echoed the text: {}",
        remote.pane_text()
    );
    thread::sleep(RTT);
    assert_eq!(
        judgments(&client, "wrong"),
        0,
        "{}",
        context(&remote, &client)
    );
    assert_eq!(
        judgments(&client, "timeout"),
        0,
        "{}",
        context(&remote, &client)
    );
    assert_eq!(
        client.screen().matches(text).count(),
        1,
        "the text is doubled or gone: {}",
        context(&remote, &client)
    );

    // Run it: the executed result proves the server received exactly the typed bytes.
    client.type_keys("\x15");
    client.type_keys("printf 'RES%s\\n' ULT\r");
    assert!(
        client.wait_screen("RESULT", READY),
        "{}",
        context(&remote, &client)
    );
    assert_eq!(
        judgments(&client, "wrong"),
        0,
        "{}",
        context(&remote, &client)
    );
}

/// Switch the pane's tty to no-echo with `cat` reading: typed keys are consumed and never
/// echoed, so every prediction is wrong. Waits for the barrier line on the client screen.
fn echo_off_cat(remote: &Remote, client: &Client) {
    remote.pane_run("stty -echo; echo ECHO\"\"OFF; cat");
    assert!(
        client.wait_screen("ECHOOFF", READY),
        "the echo-off barrier never showed: {}",
        context(remote, client)
    );
    thread::sleep(Duration::from_millis(200));
}

/// Wall-clock `ms` of the first trace line of `event` (after index `from`) matching `pred`.
fn first_ms(
    client: &Client,
    from: usize,
    event: &str,
    pred: impl Fn(&Value) -> bool,
) -> Option<u64> {
    client
        .predict_events()
        .iter()
        .skip(from)
        .find(|e| e["event"] == event && pred(e))
        .and_then(|e| e["ms"].as_u64())
}

#[test]
fn stty_echo_off_prediction_painted_then_cleared() {
    let remote = Remote::start();
    let mut client = attach_linked(&remote, Some("on"), (SCREEN_COLS, SCREEN_ROWS));
    echo_off_cat(&remote, &client);
    assert!(!client.screen().contains("QZX"));

    let started = Instant::now();
    client.type_keys("QZX");
    assert!(
        wait_for(RTT, || client.screen().contains("QZX")),
        "the prediction was not painted within one RTT: {}",
        context(&remote, &client)
    );
    assert!(
        started.elapsed() < RTT,
        "painted only after {:?}",
        started.elapsed()
    );

    // Cleared: one RTT, the server's 50 ms ack deadline and one render later. The slack
    // beyond that is for a loaded CI host; the strict wall-clock bound is the opt-in R8 test.
    let bound = RTT + ACK_DEADLINE + ONE_RENDER + Duration::from_millis(500);
    assert!(
        wait_for(bound, || !client.screen().contains("QZX")),
        "the wrong prediction was still on screen after {bound:?}: {}",
        context(&remote, &client)
    );
    assert!(
        wait_for(READY, || judgments(&client, "wrong") >= 1),
        "no wrong judgment: {}",
        context(&remote, &client)
    );
    // No stuck overlay: it stays gone, and the pane never held the text.
    let paints_after = paints(&client);
    thread::sleep(RTT * 2);
    assert!(
        !client.screen().contains("QZX"),
        "{}",
        context(&remote, &client)
    );
    assert_eq!(paints(&client), paints_after, "predictions kept painting");
    assert!(
        !remote.pane_text().contains("QZX"),
        "{}",
        remote.pane_text()
    );
}

#[test]
fn old_remote_no_predictions_no_subscribe() {
    let remote = Remote::start();
    remote.set_knob(knob::STRIP_CAPS, "x-mosh-echo-v1");
    remote.set_link(ONE_WAY_MS, LINK_RATE);
    let mut client =
        remote.spawn_remote_client(&predict_options(Some("on"), (SCREEN_COLS, SCREEN_ROWS)));
    client.wait_attached(&remote, READY);
    assert!(client.wait_event("welcome", 1, READY));
    // The link is up with an SRTT, so only the missing capability keeps `on` from predicting.
    assert!(
        wait_for(READY, || client
            .events()
            .iter()
            .any(|e| e["event"] == "inject"
                && e["state"] == "linked"
                && e["srtt_ms"].is_u64())),
        "no Linked frame with an SRTT: {}",
        context(&remote, &client)
    );
    thread::sleep(Duration::from_millis(300));

    let text = "oldremote";
    client.type_keys(text);
    thread::sleep(RTT / 2);
    assert!(
        !client.screen().contains(text),
        "the text showed before any server echo could arrive: {}",
        context(&remote, &client)
    );
    assert!(
        client.wait_screen(text, READY),
        "the text never echoed: {}",
        context(&remote, &client)
    );
    assert_eq!(
        client.event_count("echo_subscribe"),
        0,
        "{}",
        context(&remote, &client)
    );
    assert_eq!(predicted_taps(&client), 0, "{}", context(&remote, &client));
    assert_eq!(paints(&client), 0, "{}", context(&remote, &client));
    assert!(!taps(&client).is_empty(), "the tap hook never ran");
    assert_eq!(judgments(&client, "wrong"), 0);
}

#[test]
fn adaptive_on_fast_link_predicts_nothing() {
    // No link knob: the SRTT stays far below the 60 ms threshold of `adaptive`.
    let remote = Remote::start();
    let mut client = remote.spawn_remote_client(&predict_options(
        Some("adaptive"),
        (SCREEN_COLS, SCREEN_ROWS),
    ));
    client.wait_attached(&remote, READY);
    assert!(client.wait_event("welcome", 1, READY));
    assert!(
        wait_for(READY, || predicting_ready(&client)),
        "no echo + SRTT report on the fast link: {}",
        context(&remote, &client)
    );
    thread::sleep(Duration::from_millis(300));

    let text = "adaptivefast";
    client.type_keys(text);
    assert!(
        client.wait_screen(text, READY),
        "the text never echoed: {}",
        context(&remote, &client)
    );
    thread::sleep(Duration::from_millis(300));
    // The subscription is made (mode is not off), but nothing is predicted or painted: the
    // surface-patch fast path is never blocked by an overlay.
    assert_eq!(client.event_count("echo_subscribe"), 1);
    assert!(!taps(&client).is_empty(), "the tap hook never ran");
    assert_eq!(predicted_taps(&client), 0, "{}", context(&remote, &client));
    assert_eq!(paints(&client), 0, "{}", context(&remote, &client));
    assert_eq!(
        judgments(&client, "wrong") + judgments(&client, "confirmed"),
        0
    );
}

#[test]
fn existing_suites_unaffected_with_prediction_unset() {
    // The default environment, over the same 150 ms link: no subscribe, no prediction, and
    // the typed text appears only when the server's own echo arrives.
    let remote = Remote::start();
    let mut client = attach_linked(&remote, None, (SCREEN_COLS, SCREEN_ROWS));
    thread::sleep(Duration::from_millis(300));
    let text = "unsetmode";
    client.type_keys(text);
    thread::sleep(RTT / 2);
    assert!(
        !client.screen().contains(text),
        "the text showed before the server echo (a prediction with the mode unset): {}",
        context(&remote, &client)
    );
    assert!(
        client.wait_screen(text, READY),
        "the text never echoed: {}",
        context(&remote, &client)
    );
    assert_eq!(client.event_count("echo_subscribe"), 0);
    assert_eq!(predicted_taps(&client), 0);
    assert_eq!(paints(&client), 0);
    assert_eq!(
        judgments(&client, "wrong") + judgments(&client, "confirmed"),
        0
    );
}

#[test]
#[ignore = "blocked by mosh-3t9: an echo-ack under a render flood can name a pre-echo revision, so a correct prediction is cleared as wrong (3 of 4 runs)"]
fn flood_plus_typing_never_clears_correct_predictions() {
    let _lock = timing_lock();
    let remote = Remote::start();
    // A saturated link keeps the render slot backpressured behind the flood.
    remote.set_link(ONE_WAY_MS, 1024 * 1024);
    let flood_pane = remote.pane.clone();
    let split = remote.request(json!({
        "id": "split", "method": "pane.split",
        "params": {"target_pane_id": flood_pane, "direction": "right", "focus": true},
    }));
    let shell_pane = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("pane.split: {split}"))
        .to_owned();
    let mut client = remote.spawn_remote_client(&predict_options(Some("on"), DENSE_SIZE));
    client.wait_attached(&remote, READY);
    assert!(client.wait_event("welcome", 1, READY));
    assert!(
        wait_for(READY, || predicting_ready(&client)),
        "{}",
        context(&remote, &client)
    );
    thread::sleep(Duration::from_millis(300));

    pane_run_in(&remote, &flood_pane, DENSE_FLOOD);
    let start = client.output_len();
    assert!(
        wait_for(READY, || client.output_len() >= start + 64 * 1024),
        "the flood is not reaching the client"
    );
    thread::sleep(Duration::from_secs(2));

    // Printable keys into the shell pane while the flood renders next to it.
    let text = "floodtypingcheck";
    type_slowly(&mut client, text, Duration::from_millis(150));
    assert!(
        wait_for(Duration::from_secs(60), || remote
            .pane_text_of(&shell_pane)
            .contains(text)),
        "the typed text never reached the shell pane: {}",
        remote.pane_text_of(&shell_pane)
    );
    // Let every ack and its surface arrive through the backpressured link.
    thread::sleep(Duration::from_secs(10));
    let (confirmed, wrong, timeout) = (
        judgments(&client, "confirmed"),
        judgments(&client, "wrong"),
        judgments(&client, "timeout"),
    );
    eprintln!(
        "flood + typing: predicted taps {}, confirmed {confirmed}, timeout {timeout}, wrong {wrong}",
        predicted_taps(&client)
    );
    assert!(
        predicted_taps(&client) >= text.len() / 2,
        "predictions were not created under the flood: {}",
        context(&remote, &client)
    );
    assert!(
        confirmed + timeout >= 1,
        "no prediction was ever judged (a stuck overlay): {}",
        context(&remote, &client)
    );
    assert_eq!(
        wrong,
        0,
        "a correctly echoed character was cleared as wrong: {}",
        context(&remote, &client)
    );
}

// ---- opt-in wall-clock tests (--run-ignored all) -------------------------------------------

/// Milliseconds from every predicted `tap` to the next `paint`, in trace order.
fn tap_to_paint_ms(client: &Client) -> Vec<u64> {
    let events = client.predict_events();
    let mut out = Vec::new();
    for (i, tap) in events.iter().enumerate() {
        if tap["event"] != "tap" || tap["predicted"] != true {
            continue;
        }
        let Some(tap_ms) = tap["ms"].as_u64() else {
            continue;
        };
        if let Some(paint_ms) = events[i + 1..]
            .iter()
            .find(|e| e["event"] == "paint")
            .and_then(|e| e["ms"].as_u64())
        {
            out.push(paint_ms.saturating_sub(tap_ms));
        }
    }
    out
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn median_keypress_to_paint_under_5ms() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let mut client = attach_linked(&remote, Some("on"), (200, 30));
    let keys = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWX";
    assert_eq!(keys.len(), 50);
    let mut typed = String::new();
    let mut shown = Vec::new();
    for c in keys.chars() {
        typed.push(c);
        let started = Instant::now();
        client.type_keys(&c.to_string());
        // Cross-check from the harness side: the character is on the emulated screen before
        // one RTT passed, so it cannot be the server's echo.
        let seen = wait_until_screen(&client, &typed, RTT);
        shown.push((seen, started.elapsed()));
        thread::sleep(Duration::from_millis(60));
    }
    assert!(
        wait_for(READY, || judgments(&client, "confirmed") >= 1),
        "{}",
        context(&remote, &client)
    );
    for (i, (seen, elapsed)) in shown.iter().enumerate() {
        assert!(
            *seen && *elapsed < RTT,
            "key {i} was not on screen within one RTT (seen {seen}, {elapsed:?})"
        );
    }
    let mut latencies = tap_to_paint_ms(&client);
    assert!(
        latencies.len() >= 45,
        "only {} of 50 keys were predicted: {}",
        latencies.len(),
        context(&remote, &client)
    );
    latencies.sort_unstable();
    let median = latencies[latencies.len() / 2];
    eprintln!(
        "keypress-to-paint: median {median} ms over {} keys (min {} max {} ms; screen cross-check max {:?})",
        latencies.len(),
        latencies[0],
        latencies[latencies.len() - 1],
        shown.iter().map(|(_, e)| *e).max().unwrap_or_default()
    );
    assert!(
        median < 5,
        "median keypress-to-paint {median} ms >= 5 ms: {latencies:?}"
    );
    assert_eq!(
        judgments(&client, "wrong"),
        0,
        "{}",
        context(&remote, &client)
    );
}

/// Poll the client screen every 5 ms until it holds `needle` or `timeout` passes.
fn wait_until_screen(client: &Client, needle: &str, timeout: Duration) -> bool {
    mosh_harness::wait_until(timeout, Duration::from_millis(5), || {
        client.screen().contains(needle)
    })
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn r8_wrong_prediction_cleared_within_rtt_plus_50ms_plus_render() {
    let _lock = timing_lock();
    let remote = Remote::start();
    let mut client = attach_linked(&remote, Some("on"), (SCREEN_COLS, SCREEN_ROWS));
    echo_off_cat(&remote, &client);
    let bound = RTT + ACK_DEADLINE + ONE_RENDER;
    let mut measured = Vec::new();
    for key in ["Q", "J", "Z"] {
        let from = client.predict_events().len();
        client.type_keys(key);
        assert!(
            wait_for(READY, || first_ms(&client, from, "judge", |e| e["result"]
                == "wrong")
            .is_some()),
            "no clearing paint for {key}: {}",
            context(&remote, &client)
        );
        let tap = first_ms(&client, from, "tap", |e| e["predicted"] == true)
            .unwrap_or_else(|| panic!("no predicted tap for {key}: {}", context(&remote, &client)));
        let clear = first_ms(&client, from, "judge", |e| e["result"] == "wrong").unwrap_or(0);
        measured.push(clear.saturating_sub(tap));
        assert!(!client.screen().contains(key) || remote.pane_text().contains(key));
        thread::sleep(Duration::from_millis(500));
    }
    eprintln!("R8 tap-to-clearing-paint: {measured:?} ms (bound {bound:?})");
    for ms in &measured {
        assert!(
            Duration::from_millis(*ms) <= bound,
            "cleared after {ms} ms > {bound:?} ({measured:?})"
        );
    }
}

/// How one QUIC attempt ended.
enum Attempt {
    /// Both phases ran with prediction enabled the whole time.
    Done,
    /// The no-ack timeout turned prediction off for the connection (by design, spec section
    /// 3 Failure); the overlay was checked to be gone.
    Disabled,
}

/// Wait for `cond`; `false` when the no-ack timeout turned prediction off first. Panics (with
/// `what` and the context) when neither happens within `READY`.
fn until_or_disabled(
    remote: &Remote,
    client: &Client,
    what: &str,
    cond: impl Fn() -> bool,
) -> bool {
    wait_for(READY, || cond() || judgments(client, "timeout") > 0);
    if cond() {
        return true;
    }
    assert!(
        judgments(client, "timeout") > 0,
        "{what}: {}",
        context(remote, client)
    );
    false
}

/// After a timeout: the overlay is gone and nothing paints predictions any more.
fn assert_overlay_gone_after_timeout(remote: &Remote, client: &Client) {
    thread::sleep(RTT * 2);
    let events = client.predict_events();
    let timeout_at = events
        .iter()
        .position(|e| e["event"] == "judge" && e["result"] == "timeout")
        .expect("a timeout judgment");
    assert!(
        events[timeout_at + 1..]
            .iter()
            .all(|e| e["event"] != "paint"),
        "predictions kept painting after the timeout: {}",
        context(remote, client)
    );
}

/// One QUIC session at 10 % loss: correct predictions stay, wrong ones clear, no stuck overlay.
fn quic_attempt(seed: u64) -> Attempt {
    let _lock = timing_lock();
    let remote = Remote::start();
    let relay = remote.start_quic_relay(seed);
    relay.set_delay(Duration::from_millis(ONE_WAY_MS));
    let mut client =
        remote.spawn_remote_client(&predict_options(Some("on"), (SCREEN_COLS, SCREEN_ROWS)));
    client.wait_attached(&remote, READY);
    assert!(
        wait_for(READY, || remote.ssh_session_children_alive(&client.id) == 0),
        "the leg-0 ssh child is still alive: {}",
        context(&remote, &client)
    );
    let stats = relay.stats();
    assert!(
        stats.forwarded_up > 0 && stats.forwarded_down > 0,
        "the session did not use QUIC: {stats:?}"
    );
    assert!(
        wait_for(READY, || predicting_ready(&client)),
        "no echo + SRTT report: {}",
        context(&remote, &client)
    );
    thread::sleep(Duration::from_millis(300));
    relay.set_loss(0.1, 0.1);

    // Correct predictions stay: typed text is confirmed and never judged wrong.
    let text = "quic";
    type_slowly(&mut client, text, Duration::from_millis(100));
    if !until_or_disabled(
        &remote,
        &client,
        "the typed text was never confirmed",
        || judgments(&client, "confirmed") >= 1 && remote.pane_text().contains(text),
    ) {
        assert_overlay_gone_after_timeout(&remote, &client);
        return Attempt::Disabled;
    }
    // Every prediction is judged (confirmed, or the whole set timed out) before moving on.
    if !until_or_disabled(
        &remote,
        &client,
        "predictions are still outstanding",
        || predictions_settled(&client),
    ) {
        assert_overlay_gone_after_timeout(&remote, &client);
        return Attempt::Disabled;
    }
    assert_eq!(
        judgments(&client, "wrong"),
        0,
        "{}",
        context(&remote, &client)
    );
    assert!(
        client.screen().contains(text),
        "{}",
        context(&remote, &client)
    );
    client.type_keys("\x15");
    assert!(
        wait_for(READY, || !remote.pane_text().contains(text)
            && !client.screen().contains(text)),
        "the line was not cleared: {}",
        context(&remote, &client)
    );

    // Wrong predictions clear. A prediction is only made once every earlier key is acked and
    // rendered, which can take several RTTs at 10 % loss, so probe with a key the echo-off
    // `cat` swallows until one is predicted (and then judged wrong).
    echo_off_cat(&remote, &client);
    let from = client.predict_events().len();
    let mut cleared = false;
    for _ in 0..20 {
        client.type_keys("Q");
        wait_for(Duration::from_millis(2000), || {
            first_ms(&client, from, "judge", |e| e["result"] != "confirmed").is_some()
        });
        if first_ms(&client, from, "judge", |e| e["result"] == "wrong").is_some() {
            cleared = true;
            break;
        }
        if judgments(&client, "timeout") > 0 {
            assert_overlay_gone_after_timeout(&remote, &client);
            return Attempt::Disabled;
        }
    }
    assert!(
        cleared,
        "no probe was predicted and judged wrong: {}",
        context(&remote, &client)
    );
    assert!(
        wait_for(READY, || !client.screen().contains('Q')),
        "a wrong prediction is stuck on screen: {}",
        context(&remote, &client)
    );
    let paints_after = paints(&client);
    thread::sleep(RTT * 4);
    assert!(
        !client.screen().contains('Q'),
        "{}",
        context(&remote, &client)
    );
    assert_eq!(paints(&client), paints_after, "predictions kept painting");
    assert_eq!(
        remote.ssh_session_children_alive(&client.id),
        0,
        "the session left QUIC: {}",
        context(&remote, &client)
    );
    assert_eq!(client.event_count("session_end"), 0);
    eprintln!(
        "quic prediction at 10% loss: relay {:?}; confirmed {}, wrong {}, timeout {}",
        relay.stats(),
        judgments(&client, "confirmed"),
        judgments(&client, "wrong"),
        judgments(&client, "timeout")
    );
    Attempt::Done
}

/// `true` when the last paint left no predictions (every one was judged).
fn predictions_settled(client: &Client) -> bool {
    client
        .predict_events()
        .iter()
        .rev()
        .find(|e| e["event"] == "paint" || e["event"] == "judge")
        .is_none_or(|e| e["event"] == "judge")
}

#[test]
#[ignore = "wall-clock acceptance test over an emulated link; run with --run-ignored all"]
fn prediction_over_quic_with_supersede_at_10pct_loss() {
    // At 10 % loss a late ack can trip the no-ack timeout, which turns prediction off for the
    // connection by design; each such attempt is checked for a clean overlay and retried on a
    // fresh session, and at least one attempt must run both phases.
    let mut disabled = 0;
    for attempt in 0..6 {
        match quic_attempt(35 + attempt) {
            Attempt::Done => {
                eprintln!(
                    "completed on attempt {} ({disabled} timed out before)",
                    attempt + 1
                );
                return;
            }
            Attempt::Disabled => disabled += 1,
        }
    }
    panic!("prediction was turned off by the no-ack timeout in all 6 attempts");
}
