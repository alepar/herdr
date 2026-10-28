//! QUIC transport plumbing for the resumable remote session (unix only).
//!
//! Owns: the lazily started tokio runtime, the blocking `Read`/`Write` adapters over quinn
//! streams, the keeper's self-signed identity and the fingerprint-pinning client verifier,
//! the transport/endpoint configuration, the render-stream header and the shared QUIC
//! constants. Does not own: admission, leg lifecycle or fallback policy.
// Unused in non-test builds until mosh-19g.1.3, 1.4 and 1.6 consume it; they remove this.
#![cfg_attr(not(test), allow(dead_code))]

use super::link_frames::FrameClass;
use super::quic_bootstrap::Fingerprint;
use super::render_reorder::RenderArrival;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use crate::protocol::MAX_GRAPHICS_FRAME_SIZE;
use tokio::runtime::{Builder, Handle, Runtime};

/// Capability string advertised in status when the QUIC leg is available.
pub(super) const QUIC_CAPABILITY: &str = "x-mosh-quic-v1";
/// How long a QUIC leg may stay silent before the caller treats it as dead.
pub(super) const QUIC_SILENCE_WINDOW: Duration = Duration::from_secs(30);
/// The keeper advertises this; the client advertises its grace, so the negotiated value is the grace.
pub(super) const KEEPER_IDLE_TIMEOUT: Duration = Duration::from_secs(3600);
pub(super) const ALPN: &[u8] = b"herdr-mosh/1";
pub(super) const SERVER_NAME: &str = "herdr-mosh";
/// Incoming unidirectional streams the client allows (the 3b render lanes).
pub(super) const CLIENT_INCOMING_UNI: u32 = 64;

fn other<E: Into<Box<dyn std::error::Error + Send + Sync>>>(e: E) -> io::Error {
    io::Error::other(e)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Owns one tokio runtime (multi-thread, 1 worker, enable_all). Dropping it shuts the runtime down.
pub(super) struct QuicRuntime {
    runtime: Option<Runtime>,
}

impl QuicRuntime {
    pub fn new(thread_name: &str) -> io::Result<Arc<QuicRuntime>> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(thread_name)
            .enable_all()
            .build()?;
        Ok(Arc::new(QuicRuntime {
            runtime: Some(runtime),
        }))
    }

    pub fn handle(&self) -> Handle {
        match &self.runtime {
            Some(rt) => rt.handle().clone(),
            // `runtime` is only taken in Drop, after which no `&self` exists.
            None => unreachable!("QuicRuntime used after drop"),
        }
    }

    /// Run `fut` from a plain std thread. Returns an error (never panics) when called from a
    /// runtime thread.
    pub fn block_on<F: Future>(&self, fut: F) -> io::Result<F::Output> {
        block_on_handle(&self.handle(), fut)
    }
}

impl Drop for QuicRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            // Never blocks, so dropping from any thread (even a runtime worker) is safe.
            rt.shutdown_background();
        }
    }
}

fn block_on_handle<F: Future>(handle: &Handle, fut: F) -> io::Result<F::Output> {
    if Handle::try_current().is_ok() {
        return Err(io::Error::other(
            "blocking QUIC call attempted from a runtime thread",
        ));
    }
    Ok(handle.block_on(fut))
}

/// Blocking `Read` over a quinn `RecvStream` (EOF -> Ok(0); reset or connection loss -> Err).
pub(super) struct BlockingRecv {
    handle: Handle,
    recv: quinn::RecvStream,
    timeout: Option<Duration>,
}

impl BlockingRecv {
    pub fn new(handle: Handle, recv: quinn::RecvStream) -> BlockingRecv {
        BlockingRecv {
            handle,
            recv,
            timeout: None,
        }
    }

    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }
}

impl Read for BlockingRecv {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let timeout = self.timeout;
        let recv = &mut self.recv;
        let outcome = block_on_handle(&self.handle, async move {
            match timeout {
                Some(limit) => tokio::time::timeout(limit, recv.read(buf))
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "QUIC read timed out"))?,
                None => recv.read(buf).await,
            }
            .map_err(io::Error::from)
        })??;
        Ok(outcome.unwrap_or(0))
    }
}

/// Blocking `Write` over a quinn `SendStream`; `flush` is a no-op.
pub(super) struct BlockingSend {
    handle: Handle,
    send: quinn::SendStream,
}

impl BlockingSend {
    pub fn new(handle: Handle, send: quinn::SendStream) -> BlockingSend {
        BlockingSend { handle, send }
    }

    /// Signal end of stream; the peer's reader sees EOF.
    pub fn finish(&mut self) -> io::Result<()> {
        self.send.finish().map_err(other)
    }
}

impl Write for BlockingSend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let send = &mut self.send;
        block_on_handle(&self.handle, async move {
            send.write(buf).await.map_err(io::Error::from)
        })?
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The keeper's self-signed identity: certificate chain, key and the SHA-256 fingerprint
/// the client pins.
pub(super) struct ServerIdentity {
    pub certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    pub fingerprint: Fingerprint,
}

impl ServerIdentity {
    pub fn generate() -> io::Result<ServerIdentity> {
        let certified =
            rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()]).map_err(other)?;
        let cert = certified.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        Ok(ServerIdentity {
            fingerprint: fingerprint_of(&cert),
            certs: vec![cert],
            key: key.into(),
        })
    }
}

fn fingerprint_of(cert: &CertificateDer<'_>) -> Fingerprint {
    Fingerprint(Sha256::digest(cert.as_ref()).into())
}

/// Accepts exactly one certificate, identified by its SHA-256 fingerprint. Hostname, expiry
/// and chain are deliberately not checked: the pin is the trust anchor.
#[derive(Debug)]
struct PinnedVerifier {
    pinned: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint_of(end_entity) == self.pinned {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// max_idle_timeout = `idle`, keep_alive_interval None, max_concurrent_bidi_streams 1,
/// max_concurrent_uni_streams = `incoming_uni` (client: 64, the 3b render-lane cap; keeper: 0).
pub(super) fn transport_config(
    idle: Duration,
    incoming_uni: u32,
) -> io::Result<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(quinn::IdleTimeout::try_from(idle).map_err(other)?));
    transport.keep_alive_interval(None);
    transport.max_concurrent_bidi_streams(1u32.into());
    transport.max_concurrent_uni_streams(incoming_uni.into());
    Ok(transport)
}

pub(super) fn server_config(
    identity: &ServerIdentity,
    idle: Duration,
) -> io::Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(other)?
        .with_no_client_auth()
        .with_single_cert(identity.certs.clone(), identity.key.clone_key())
        .map_err(other)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicServerConfig::try_from(tls).map_err(other)?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config(idle, 0)?));
    Ok(config)
}

pub(super) fn client_config(
    pinned: Fingerprint,
    idle: Duration,
) -> io::Result<quinn::ClientConfig> {
    let provider = provider();
    let verifier = PinnedVerifier {
        pinned,
        algorithms: provider.signature_verification_algorithms,
    };
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = QuicClientConfig::try_from(tls).map_err(other)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(Arc::new(transport_config(idle, CLIENT_INCOMING_UNI)?));
    Ok(config)
}

/// Wildcard bind (`[::]:0` when `v6`, else `0.0.0.0:0`): roaming by QUIC migration, no rebind.
pub(super) fn client_endpoint(rt: &QuicRuntime, v6: bool) -> io::Result<quinn::Endpoint> {
    let _guard = rt.handle().enter();
    let addr: SocketAddr = if v6 {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
    };
    quinn::Endpoint::client(addr)
}

pub(super) fn server_endpoint(
    rt: &QuicRuntime,
    config: quinn::ServerConfig,
    socket: UdpSocket,
) -> io::Result<quinn::Endpoint> {
    let _guard = rt.handle().enter();
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(config),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
}

/// Render-stream header (3b): u64 LE render sequence + class byte. `class` is a raw byte here;
/// `FrameClass::wire_byte`/`from_wire_byte` (mosh-19g.2.1) map it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RenderHeader {
    pub seq: u64,
    pub class: u8,
}

impl RenderHeader {
    pub const LEN: usize = 9;

    pub fn encode(&self) -> [u8; 9] {
        let mut out = [0u8; 9];
        out[..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8] = self.class;
        out
    }

    /// Reads the first `LEN` bytes; a shorter slice is `None`.
    pub fn decode(bytes: &[u8]) -> Option<RenderHeader> {
        let seq = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
        Some(RenderHeader {
            seq,
            class: *bytes.get(8)?,
        })
    }
}

/// The bridge-side render lane of one QUIC leg.
pub(super) type RenderLaneRx = mpsc::Receiver<RenderArrival>;

/// Accepts the keeper's uni streams on the client runtime and reports each as `RenderArrival`s:
/// `Header` after the 9-byte header, `Complete` at stream end, `Reset` on RESET_STREAM. A
/// stream reset (or unreadable) before its header was read is ignored: its seq is unknown and
/// the reorder's overdue rule covers it.
///
/// Teardown never blocks: once the leg drops the receiver every send fails and the stream tasks
/// exit; once the connection closes `accept_uni` errors and the accept task exits.
pub(super) fn spawn_render_reader(handle: &Handle, conn: quinn::Connection) -> RenderLaneRx {
    let (tx, rx) = mpsc::channel();
    handle.spawn(async move {
        while let Ok(recv) = conn.accept_uni().await {
            tokio::spawn(read_render_stream(recv, tx.clone()));
        }
    });
    rx
}

async fn read_render_stream(mut recv: quinn::RecvStream, tx: mpsc::Sender<RenderArrival>) {
    let mut raw = [0u8; RenderHeader::LEN];
    if recv.read_exact(&mut raw).await.is_err() {
        return;
    }
    let Some(header) = RenderHeader::decode(&raw) else {
        return;
    };
    let Some(class) = FrameClass::from_wire_byte(header.class) else {
        let _ = recv.stop(0u32.into());
        return;
    };
    let seq = header.seq;
    if tx.send(RenderArrival::Header { seq, class }).is_err() {
        return;
    }
    let arrival = match recv.read_to_end(MAX_GRAPHICS_FRAME_SIZE + 4).await {
        Ok(bytes) => RenderArrival::Complete { seq, class, bytes },
        Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(_))) => {
            RenderArrival::Reset { seq }
        }
        Err(quinn::ReadToEndError::TooLong) => {
            let _ = recv.stop(0u32.into());
            RenderArrival::Reset { seq }
        }
        // Connection loss and the like: the leg is ending, the lane has nothing to add.
        Err(_) => return,
    };
    let _ = tx.send(arrival);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    const PAYLOAD: usize = 1 << 20;

    fn pattern(n: usize, salt: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
            .collect()
    }

    /// quinn only runs a handshake once the server accepts the incoming connection.
    fn spawn_accept_loop(rt: &QuicRuntime, server: quinn::Endpoint) {
        rt.handle().spawn(async move {
            while let Some(incoming) = server.accept().await {
                tokio::spawn(async move {
                    let _ = incoming.await;
                });
            }
        });
    }

    /// A connected loopback pair: the client side (as the bridge sees it) and the keeper side.
    struct Pair {
        rt: Arc<QuicRuntime>,
        client: quinn::Connection,
        server: quinn::Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
    }

    fn pair() -> Pair {
        let rt = QuicRuntime::new("quic-render-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server_ep = server_endpoint(
            &rt,
            server_config(&identity, KEEPER_IDLE_TIMEOUT).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        let client_ep = client_endpoint(&rt, false).expect("client endpoint");
        let config = client_config(identity.fingerprint, Duration::from_secs(10)).expect("config");
        let accept = server_ep.clone();
        let server_task = rt.handle().spawn(async move {
            let incoming = accept.accept().await.expect("incoming");
            incoming.await.expect("server conn")
        });
        let connecting = {
            let _guard = rt.handle().enter();
            client_ep
                .connect_with(config, addr, SERVER_NAME)
                .expect("connect")
        };
        let (client, server) = rt
            .block_on(async move { (connecting.await.expect("client conn"), server_task.await) })
            .expect("block_on");
        Pair {
            rt,
            client,
            server: server.expect("join"),
            _endpoints: (server_ep, client_ep),
        }
    }

    fn header(seq: u64, class: FrameClass) -> Vec<u8> {
        RenderHeader {
            seq,
            class: class.wire_byte(),
        }
        .encode()
        .to_vec()
    }

    fn next(rx: &RenderLaneRx) -> RenderArrival {
        rx.recv_timeout(Duration::from_secs(5)).expect("arrival")
    }

    #[test]
    fn header_then_complete_with_seq_class_bytes() {
        let p = pair();
        let rx = spawn_render_reader(&p.rt.handle(), p.client.clone());
        let server = p.server.clone();
        let sent = pattern(100_000, 9);
        let body = sent.clone();
        p.rt.block_on(async move {
            let mut a = server.open_uni().await.expect("uni");
            a.write_all(&header(7, FrameClass::SelfContained))
                .await
                .expect("header");
            a.write_all(&body).await.expect("body");
            a.finish().expect("finish");
            let mut b = server.open_uni().await.expect("uni");
            let mut whole = header(8, FrameClass::Chained);
            whole.extend_from_slice(b"delta");
            b.write_all(&whole).await.expect("write");
            b.finish().expect("finish");
            // Keep the streams alive until the reader has them.
            let _ = (a.stopped().await, b.stopped().await);
        })
        .expect("block_on");
        // Streams may interleave: collect per seq and check each stream's own order.
        let mut seen: Vec<RenderArrival> = (0..4).map(|_| next(&rx)).collect();
        let pos = |want: &RenderArrival, seen: &[RenderArrival]| {
            seen.iter().position(|a| a == want).expect("arrival present")
        };
        let h7 = RenderArrival::Header {
            seq: 7,
            class: FrameClass::SelfContained,
        };
        let c7 = RenderArrival::Complete {
            seq: 7,
            class: FrameClass::SelfContained,
            bytes: sent,
        };
        let h8 = RenderArrival::Header {
            seq: 8,
            class: FrameClass::Chained,
        };
        let c8 = RenderArrival::Complete {
            seq: 8,
            class: FrameClass::Chained,
            bytes: b"delta".to_vec(),
        };
        assert!(pos(&h7, &seen) < pos(&c7, &seen));
        assert!(pos(&h8, &seen) < pos(&c8, &seen));
        seen.clear();
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn reset_stream_yields_reset_seq() {
        let p = pair();
        let rx = spawn_render_reader(&p.rt.handle(), p.client.clone());
        let server = p.server.clone();
        p.rt.block_on(async move {
            let mut s = server.open_uni().await.expect("uni");
            s.write_all(&header(21, FrameClass::Chained))
                .await
                .expect("header");
            s.write_all(b"partial").await.expect("partial");
            // The header must reach the reader before the reset discards unread data.
            tokio::time::sleep(Duration::from_millis(200)).await;
            s.reset(5u32.into()).expect("reset");
        })
        .expect("block_on");
        assert_eq!(
            next(&rx),
            RenderArrival::Header {
                seq: 21,
                class: FrameClass::Chained
            }
        );
        assert_eq!(next(&rx), RenderArrival::Reset { seq: 21 });
    }

    #[test]
    fn reset_before_header_is_ignored_and_bad_class_dropped() {
        let p = pair();
        let rx = spawn_render_reader(&p.rt.handle(), p.client.clone());
        let server = p.server.clone();
        p.rt.block_on(async move {
            let mut early = server.open_uni().await.expect("uni");
            early.write_all(b"abc").await.expect("short");
            early.reset(1u32.into()).expect("reset");
            let mut bad = server.open_uni().await.expect("uni");
            bad.write_all(&RenderHeader { seq: 3, class: 9 }.encode())
                .await
                .expect("bad header");
            bad.finish().expect("finish");
            let mut good = server.open_uni().await.expect("uni");
            good.write_all(&header(4, FrameClass::Reliable))
                .await
                .expect("good");
            good.finish().expect("finish");
            let _ = good.stopped().await;
        })
        .expect("block_on");
        assert_eq!(
            next(&rx),
            RenderArrival::Header {
                seq: 4,
                class: FrameClass::Reliable
            }
        );
        assert_eq!(
            next(&rx),
            RenderArrival::Complete {
                seq: 4,
                class: FrameClass::Reliable,
                bytes: Vec::new()
            }
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn teardown_stops_reader_without_blocking() {
        let p = pair();
        let rx = spawn_render_reader(&p.rt.handle(), p.client.clone());
        // Leg teardown: drop the receiver, close the connection, from a plain thread.
        let (done_tx, done_rx) = mpsc::channel();
        let client = p.client.clone();
        let closer = thread::spawn(move || {
            drop(rx);
            client.close(0u32.into(), b"leg end");
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("teardown must not block");
        closer.join().expect("closer");
        // The runtime still serves a block_on, and the server sees the close.
        let reason = p.rt.block_on(p.server.closed()).expect("block_on");
        assert!(matches!(reason, quinn::ConnectionError::ApplicationClosed(_)));
    }

    #[test]
    fn render_header_round_trips() {
        for seq in [0, 1, u64::MAX] {
            for class in 0..=2u8 {
                let header = RenderHeader { seq, class };
                let bytes = header.encode();
                assert_eq!(bytes.len(), RenderHeader::LEN);
                assert_eq!(RenderHeader::decode(&bytes), Some(header));
            }
        }
        let bytes = RenderHeader { seq: 7, class: 1 }.encode();
        assert_eq!(&bytes[..8], &7u64.to_le_bytes());
        assert_eq!(bytes[8], 1);
        assert_eq!(RenderHeader::decode(&bytes[..8]), None);
        assert_eq!(RenderHeader::decode(&[]), None);
    }

    #[test]
    fn loopback_transfers_both_ways_through_blocking_adapter() {
        let rt = QuicRuntime::new("quic-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server = server_endpoint(
            &rt,
            server_config(&identity, KEEPER_IDLE_TIMEOUT).expect("server config"),
            socket,
        )
        .expect("server endpoint");

        let server_rt = rt.clone();
        let server_thread = thread::spawn(move || -> io::Result<()> {
            let conn = server_rt
                .block_on(async {
                    let incoming = server.accept().await.ok_or_else(|| other("no incoming"))?;
                    incoming.await.map_err(other)
                })
                .and_then(|r: io::Result<quinn::Connection>| r)?;
            let (send, recv) = server_rt.block_on(conn.accept_bi())?.map_err(other)?;
            let mut recv = BlockingRecv::new(server_rt.handle(), recv);
            let mut send = BlockingSend::new(server_rt.handle(), send);
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let n = recv.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                send.write_all(&buf[..n])?;
            }
            send.finish()?;
            server_rt.block_on(conn.closed())?;
            Ok(())
        });

        let client_rt = rt.clone();
        let fingerprint = identity.fingerprint;
        let client_thread = thread::spawn(move || -> io::Result<Vec<u8>> {
            let endpoint = client_endpoint(&client_rt, false)?;
            let config = client_config(fingerprint, Duration::from_secs(10))?;
            let conn = client_rt
                .block_on(async {
                    endpoint
                        .connect_with(config, addr, SERVER_NAME)
                        .map_err(other)?
                        .await
                        .map_err(other)
                })
                .and_then(|r: io::Result<quinn::Connection>| r)?;
            let (send, recv) = client_rt.block_on(conn.open_bi())?.map_err(other)?;
            let mut send = BlockingSend::new(client_rt.handle(), send);
            let mut recv = BlockingRecv::new(client_rt.handle(), recv);
            // Writing from a second thread keeps the echo from deadlocking on flow control.
            let writer = thread::spawn(move || -> io::Result<()> {
                send.write_all(&pattern(PAYLOAD, 3))?;
                send.finish()
            });
            let mut echoed = vec![0u8; PAYLOAD];
            recv.read_exact(&mut echoed)?;
            writer.join().map_err(|_| other("writer panicked"))??;
            conn.close(0u32.into(), b"done");
            Ok(echoed)
        });

        let echoed = client_thread
            .join()
            .expect("client thread")
            .expect("client");
        assert_eq!(echoed, pattern(PAYLOAD, 3));
        server_thread
            .join()
            .expect("server thread")
            .expect("server");
    }

    #[test]
    fn wrong_fingerprint_fails_handshake() {
        let rt = QuicRuntime::new("quic-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server = server_endpoint(
            &rt,
            server_config(&identity, Duration::from_secs(10)).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        spawn_accept_loop(&rt, server);
        assert_ne!(identity.fingerprint, Fingerprint([0; 32]));

        let endpoint = client_endpoint(&rt, false).expect("client endpoint");
        let config = client_config(Fingerprint([0; 32]), Duration::from_secs(10)).expect("config");
        let started = std::time::Instant::now();
        let result = rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    endpoint
                        .connect_with(config, addr, SERVER_NAME)
                        .expect("connect"),
                )
                .await
            })
            .expect("block_on");
        match result {
            Ok(Ok(_)) => panic!("handshake succeeded against a mismatched pin"),
            Ok(Err(_)) => {}
            Err(_) => panic!("handshake neither succeeded nor failed within 5s"),
        }
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn matching_fingerprint_handshake_succeeds() {
        // Control for the mismatch test: the same setup with the right pin connects.
        let rt = QuicRuntime::new("quic-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server = server_endpoint(
            &rt,
            server_config(&identity, Duration::from_secs(10)).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        spawn_accept_loop(&rt, server);
        let endpoint = client_endpoint(&rt, false).expect("client endpoint");
        let config = client_config(identity.fingerprint, Duration::from_secs(10)).expect("config");
        let conn = rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    endpoint
                        .connect_with(config, addr, SERVER_NAME)
                        .expect("connect"),
                )
                .await
            })
            .expect("block_on")
            .expect("timeout")
            .expect("handshake");
        drop(conn);
    }

    #[test]
    fn shared_constants_are_pinned() {
        assert_eq!(QUIC_CAPABILITY, "x-mosh-quic-v1");
        assert_eq!(QUIC_SILENCE_WINDOW, Duration::from_secs(30));
    }

    #[test]
    fn read_timeout_reports_timed_out() {
        let rt = QuicRuntime::new("quic-test").expect("runtime");
        let identity = ServerIdentity::generate().expect("identity");
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = socket.local_addr().expect("addr");
        let server = server_endpoint(
            &rt,
            server_config(&identity, KEEPER_IDLE_TIMEOUT).expect("server config"),
            socket,
        )
        .expect("server endpoint");
        let endpoint = client_endpoint(&rt, false).expect("client endpoint");
        let config = client_config(identity.fingerprint, Duration::from_secs(10)).expect("config");
        let (client_conn, server_conn) = rt
            .block_on(async {
                let connecting = endpoint
                    .connect_with(config, addr, SERVER_NAME)
                    .expect("connect");
                let accept = async {
                    let incoming = server.accept().await.expect("incoming");
                    incoming.await
                };
                tokio::join!(connecting, accept)
            })
            .expect("block_on");
        let client_conn = client_conn.expect("client handshake");
        let server_conn = server_conn.expect("server handshake");
        let (send, _recv) = rt
            .block_on(client_conn.open_bi())
            .expect("rt")
            .expect("open");
        let mut send = BlockingSend::new(rt.handle(), send);
        send.write_all(b"x").expect("write");
        let (_s, recv) = rt
            .block_on(server_conn.accept_bi())
            .expect("rt")
            .expect("accept");
        let mut recv = BlockingRecv::new(rt.handle(), recv);
        recv.set_read_timeout(Some(Duration::from_millis(200)));
        let mut byte = [0u8; 1];
        recv.read_exact(&mut byte).expect("first byte");
        assert_eq!(&byte, b"x");
        let err = recv.read(&mut byte).expect_err("no data pending");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn block_on_from_runtime_thread_is_an_error() {
        let rt = QuicRuntime::new("quic-test").expect("runtime");
        let inner = rt.clone();
        let result = rt
            .block_on(async move { inner.block_on(async { 1 }) })
            .expect("outer");
        assert!(result.is_err(), "nested block_on must be an error");
    }
}
