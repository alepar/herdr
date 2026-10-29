//! Self-test of the harness's userspace UDP relay.

#![cfg(unix)]

#[path = "mosh_harness.rs"]
pub mod mosh_harness;

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use mosh_harness::{timing_lock, UdpRelay};

const N: usize = 2000;

fn socket() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind");
    s.set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    s
}

/// One lossy, delayed run; returns (dropped_up, forwarded_up, delays of received packets).
fn run_lossy(seed: u64) -> (u64, u64, Vec<Duration>) {
    let target = socket();
    target
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    let client = socket();
    let relay = UdpRelay::start(target.local_addr().expect("addr"), seed);
    relay.set_delay(Duration::from_millis(100));
    relay.set_loss(0.10, 0.0);

    let receiver = std::thread::spawn(move || {
        let mut recv_at: Vec<Option<Instant>> = vec![None; N];
        let mut buf = [0u8; 16];
        while let Ok((n, _)) = target.recv_from(&mut buf) {
            if n == 4 {
                let idx = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                recv_at[idx] = Some(Instant::now());
            }
        }
        recv_at
    });
    let mut sent_at = Vec::with_capacity(N);
    for i in 0..N as u32 {
        sent_at.push(Instant::now());
        client
            .send_to(&i.to_le_bytes(), relay.local_addr())
            .expect("send");
        std::thread::sleep(Duration::from_micros(500));
    }
    let recv_at = receiver.join().expect("receiver");
    let delays = recv_at
        .iter()
        .zip(&sent_at)
        .filter_map(|(r, s)| r.map(|r| r.duration_since(*s)))
        .collect();
    let stats = relay.stats();
    (stats.dropped_up, stats.forwarded_up, delays)
}

#[test]
fn relay_delay_and_loss_statistics() {
    let _lock = timing_lock();
    let (dropped, forwarded, mut delays) = run_lossy(0x5eed);
    assert_eq!(dropped + forwarded, N as u64);
    assert_eq!(delays.len() as u64, forwarded, "no kernel loss expected");
    let loss = 1.0 - delays.len() as f64 / N as f64;
    assert!((0.05..=0.15).contains(&loss), "loss {loss}");
    delays.sort();
    let median = delays[delays.len() / 2];
    assert!(
        (Duration::from_millis(80)..=Duration::from_millis(120)).contains(&median),
        "median delay {median:?}"
    );
    assert!(
        delays[0] >= Duration::from_millis(80),
        "min {:?}",
        delays[0]
    );

    let (dropped_again, _, _) = run_lossy(0x5eed);
    assert_eq!(dropped, dropped_again, "same seed, same drop count");
}

#[test]
fn relay_rebind_changes_source_and_traffic_continues() {
    let target = socket();
    let client = socket();
    let relay = UdpRelay::start(target.local_addr().expect("addr"), 1);
    let mut buf = [0u8; 16];

    client.send_to(b"a", relay.local_addr()).expect("send");
    let (n, s1) = target.recv_from(&mut buf).expect("target recv a");
    assert_eq!(&buf[..n], b"a");
    assert_eq!(s1, relay.upstream_addr());
    target.send_to(b"A", s1).expect("reply");
    let (n, from) = client.recv_from(&mut buf).expect("client recv A");
    assert_eq!(&buf[..n], b"A");
    assert_eq!(from, relay.local_addr());

    relay.rebind();
    client.send_to(b"b", relay.local_addr()).expect("send");
    let (n, s2) = target.recv_from(&mut buf).expect("target recv b");
    assert_eq!(&buf[..n], b"b");
    assert_ne!(s2, s1);
    assert_eq!(s2, relay.upstream_addr());
    target.send_to(b"B", s2).expect("reply");
    let (n, _) = client.recv_from(&mut buf).expect("client recv B");
    assert_eq!(&buf[..n], b"B");
}

#[test]
fn relay_blocks_udp_at_full_loss() {
    let target = socket();
    target
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("timeout");
    let client = socket();
    let relay = UdpRelay::start(target.local_addr().expect("addr"), 2);
    relay.set_loss(1.0, 1.0);
    for i in 0..50u8 {
        client.send_to(&[i], relay.local_addr()).expect("send");
    }
    let mut buf = [0u8; 16];
    assert!(target.recv_from(&mut buf).is_err(), "datagram got through");
    assert_eq!(relay.stats().dropped_up, 50);
}
