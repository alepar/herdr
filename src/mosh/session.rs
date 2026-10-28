//! Session wire protocol, per-direction offset and ack state machine, flow-control rule,
//! link timers and hello/welcome negotiation. Pure: operates on buffers, no sockets.
//! Does not own leg lifecycle or ring storage policy.
//!
//! Time is always an `Instant` parameter; nothing here reads a clock.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::ring::{ReplayRing, RingOverflow};
use super::window::{InflightWindow, WindowSnapshot};

/// HELLO/WELCOME `v`; keeper rendezvous name input.
pub(super) const SESSION_PROTOCOL_VERSION: u32 = 1;

// Protocol constants. Changing any of them changes wire behaviour, so it bumps
// `SESSION_PROTOCOL_VERSION`, `STATUS_CAPABILITY` and `SESSION_FLAG` together.
/// Timer-driven heartbeat period while a leg is up.
pub(super) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Inbound silence after WELCOME that kills a leg.
pub(super) const SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
/// Bound from the local end writing HELLO to reading WELCOME, on any leg.
pub(super) const PRE_WELCOME_BOUND: Duration = Duration::from_secs(20);
/// Reconnect-leg setup deadline from spawn to HELLO.
pub(super) const LEG_SETUP_MAX: Duration = Duration::from_secs(15);
/// Largest DATA payload (excluding the 8-byte offset); larger is a leg protocol error.
pub(super) const MAX_DATA_CHUNK: usize = 64 * 1024;
/// Hard cap on the in-flight window (parity with OpenSSH CHAN_SES_WINDOW_DEFAULT); the
/// adaptive window below it is `window::InflightWindow`.
pub(super) const WINDOW: usize = 2 * 1024 * 1024;
/// Ack after this many newly delivered bytes ...
pub(super) const ACK_EVERY_BYTES: u64 = 64 * 1024;
/// ... or this long after the first unacked delivered byte.
pub(super) const ACK_EVERY: Duration = Duration::from_millis(200);
/// Largest HELLO/WELCOME/END JSON body.
pub(super) const MAX_CONTROL_PAYLOAD: usize = 4096;

const _: () = assert!(WINDOW == 2 * 1024 * 1024 && MAX_DATA_CHUNK == 64 * 1024);

/// Source reads while a leg is up are rounded up to this many bytes.
const SOURCE_READ_QUANTUM: usize = 4 * 1024;

const TYPE_HELLO: u8 = 1;
const TYPE_WELCOME: u8 = 2;
const TYPE_DATA: u8 = 3;
const TYPE_ACK: u8 = 4;
const TYPE_HEARTBEAT: u8 = 5;
const TYPE_END: u8 = 6;
const HEADER_LEN: usize = 5;

/// The HELLO type byte, which the keeper's producers read first on a new connection.
pub(super) const HELLO_TYPE: u8 = TYPE_HELLO;
/// First byte of a bootstrap query to the keeper. Not a session message type.
pub(super) const BOOTSTRAP_QUERY: u8 = b'Q';

const _: () = assert!(
    BOOTSTRAP_QUERY != TYPE_HELLO
        && BOOTSTRAP_QUERY != TYPE_WELCOME
        && BOOTSTRAP_QUERY != TYPE_DATA
        && BOOTSTRAP_QUERY != TYPE_ACK
        && BOOTSTRAP_QUERY != TYPE_HEARTBEAT
        && BOOTSTRAP_QUERY != TYPE_END
);

/// END reasons and the local reaction each requires.
pub(super) mod end_reason {
    /// Leg 0: plain fallback. Reconnect leg: Fatal.
    pub const KEEPER_UNAVAILABLE: &str = "keeper-unavailable";
    /// Leg 0: plain fallback.
    pub const SERVER_CONNECT_FAILED: &str = "server-connect-failed";
    /// Leg 0: plain fallback. Reconnect leg: Fatal.
    pub const PROTOCOL_VERSION: &str = "protocol-version";
    /// Leg 0: plain fallback.
    pub const TOKEN_IN_USE: &str = "token-in-use";
    /// Deliver the complete frames, then close the client.
    pub const SERVER_EOF: &str = "server-eof";
    /// Close the client.
    pub const RING_OVERFLOW: &str = "ring-overflow";
    /// The keeper ends the session.
    pub const CLIENT_GONE: &str = "client-gone";
    /// The keeper refused the QUIC admission token on a QUIC leg: the dialer falls back.
    pub const QUIC_TOKEN: &str = "quic-token";
}

/// A problem that ends the current leg, never the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegProtocolError(pub String);

impl fmt::Display for LegProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "session leg protocol error: {}", self.0)
    }
}

impl std::error::Error for LegProtocolError {}

/// A leg-byte error plus the events decoded from the same buffer before it. `recv_offset`
/// already counts every `Deliver` in `delivered`, so the caller must hand them on before
/// dropping the leg or those bytes are lost for good (the peer will not resend them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LegBytesError {
    pub error: LegProtocolError,
    pub delivered: Vec<SessionEvent>,
}

impl fmt::Display for LegBytesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for LegBytesError {}

fn leg_err<T>(msg: impl Into<String>) -> Result<T, LegProtocolError> {
    Err(LegProtocolError(msg.into()))
}

/// 128-bit session token.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct Token([u8; 16]);

impl Token {
    pub fn generate() -> io::Result<Token> {
        let mut buf = [0u8; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
        Ok(Token(buf))
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for Token {
    type Err = LegProtocolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 32 {
            return leg_err("token must be 32 hex digits");
        }
        let mut out = [0u8; 16];
        for (i, pair) in bytes.chunks(2).enumerate() {
            let hi = hex_val(pair[0])?;
            let lo = hex_val(pair[1])?;
            out[i] = hi << 4 | lo;
        }
        Ok(Token(out))
    }
}

fn hex_val(b: u8) -> Result<u8, LegProtocolError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => leg_err("token has a non-hex digit"),
    }
}

impl Serialize for Token {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Token {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct Hello {
    pub v: u32,
    pub token: Token,
    pub resume: bool,
    pub recv_offset: u64,
    pub grace_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_token: Option<Token>,
    /// QUIC admission token (hex text, opaque here); only the keeper's QUIC acceptor parses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_token: Option<String>,
    /// The client wants render frames on the render lane (3b); fixed by the HELLO that
    /// creates the session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub render_split: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum WelcomeStatus {
    New,
    Resumed,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Welcome {
    pub v: u32,
    pub status: WelcomeStatus,
    pub recv_offset: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct End {
    pub reason: String,
}

/// The session message codec. New kinds are a variant plus a match arm; callers never hand-pack
/// type/len bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SessionMsg {
    Hello(Hello),
    Welcome(Welcome),
    Data { offset: u64, bytes: Vec<u8> },
    Ack(u64),
    Heartbeat,
    End(End),
}

impl SessionMsg {
    /// `[u8 type][u32 LE len][payload]`.
    pub fn encode(&self) -> Vec<u8> {
        let (ty, payload) = match self {
            SessionMsg::Hello(h) => (TYPE_HELLO, serde_json::to_vec(h).expect("hello json")),
            SessionMsg::Welcome(w) => (TYPE_WELCOME, serde_json::to_vec(w).expect("welcome json")),
            SessionMsg::Data { offset, bytes } => {
                let mut p = Vec::with_capacity(8 + bytes.len());
                p.extend_from_slice(&offset.to_le_bytes());
                p.extend_from_slice(bytes);
                (TYPE_DATA, p)
            }
            SessionMsg::Ack(n) => (TYPE_ACK, n.to_le_bytes().to_vec()),
            SessionMsg::Heartbeat => (TYPE_HEARTBEAT, Vec::new()),
            SessionMsg::End(e) => (TYPE_END, serde_json::to_vec(e).expect("end json")),
        };
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.push(ty);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        out
    }
}

/// Incremental decoder. Header problems are reported as soon as the 5-byte header is in, before
/// any payload is buffered.
#[derive(Default)]
pub(super) struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next(&mut self) -> Result<Option<SessionMsg>, LegProtocolError> {
        if self.buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let ty = self.buf[0];
        let len = u32::from_le_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        match ty {
            TYPE_HELLO | TYPE_WELCOME | TYPE_END if len > MAX_CONTROL_PAYLOAD => {
                return leg_err(format!("control message too long ({len})"));
            }
            TYPE_DATA if !(8..=8 + MAX_DATA_CHUNK).contains(&len) => {
                return leg_err(format!("DATA payload length {len} out of range"));
            }
            TYPE_ACK if len != 8 => return leg_err("ACK length must be 8"),
            TYPE_HEARTBEAT if len != 0 => return leg_err("HEARTBEAT must be empty"),
            TYPE_HELLO | TYPE_WELCOME | TYPE_DATA | TYPE_ACK | TYPE_HEARTBEAT | TYPE_END => {}
            other => return leg_err(format!("unknown message type {other}")),
        }
        if self.buf.len() < HEADER_LEN + len {
            return Ok(None);
        }
        let payload: Vec<u8> = self.buf.drain(..HEADER_LEN + len).skip(HEADER_LEN).collect();
        let json_err = |e: serde_json::Error| LegProtocolError(format!("malformed JSON: {e}"));
        let msg = match ty {
            TYPE_HELLO => SessionMsg::Hello(serde_json::from_slice(&payload).map_err(json_err)?),
            TYPE_WELCOME => {
                SessionMsg::Welcome(serde_json::from_slice(&payload).map_err(json_err)?)
            }
            TYPE_END => SessionMsg::End(serde_json::from_slice(&payload).map_err(json_err)?),
            TYPE_DATA => SessionMsg::Data {
                offset: u64::from_le_bytes(payload[..8].try_into().expect("8 bytes")),
                bytes: payload[8..].to_vec(),
            },
            TYPE_ACK => SessionMsg::Ack(u64::from_le_bytes(payload[..8].try_into().expect("8"))),
            _ => SessionMsg::Heartbeat,
        };
        Ok(Some(msg))
    }
}

/// The keeper's answer to a HELLO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HelloAnswer {
    RefuseVersion,
    RefuseTokenInUse,
    Welcome(WelcomeStatus),
}

/// `exists`: the keeper has a session for `hello.token` (live, parked or eof). The one place
/// that enforces "resume:true never yields new".
pub(super) fn answer_hello(hello: &Hello, exists: bool) -> HelloAnswer {
    if hello.v != SESSION_PROTOCOL_VERSION {
        return HelloAnswer::RefuseVersion;
    }
    match (hello.resume, exists) {
        (false, false) => HelloAnswer::Welcome(WelcomeStatus::New),
        (false, true) => HelloAnswer::RefuseTokenInUse,
        (true, false) => HelloAnswer::Welcome(WelcomeStatus::Unknown),
        (true, true) => HelloAnswer::Welcome(WelcomeStatus::Resumed),
    }
}

/// Reconnect-leg setup deadline: `min(now + LEG_SETUP_MAX, give_up_at)`.
pub(super) fn setup_deadline(now: Instant, give_up_at: Instant) -> Instant {
    (now + LEG_SETUP_MAX).min(give_up_at)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Local,
    Keeper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkPhase {
    Down,
    PreWelcome {
        hello_at: Instant,
    },
    Up {
        last_inbound: Instant,
        last_heartbeat: Instant,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SessionEvent {
    /// New in-order bytes for the consumer (overlap already dropped).
    Deliver(Vec<u8>),
    /// Keeper only: a HELLO arrived; answer via `welcome()` / `refuse()`, then call
    /// `on_leg_bytes(now, &[])` to drain whatever followed it.
    Hello(Hello),
    /// Local only: `New` on leg 0, `Resumed` on a reconnect.
    Welcomed(WelcomeStatus),
    /// Local only: `Unknown`, or `New` answered to `resume:true` (session gone).
    ResumeRefused,
    /// END received: the session is over (or, before WELCOME, a refusal). Processing stops here.
    PeerEnd(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkVerdict {
    Silence,
    PreWelcomeBound,
}

/// One end of a session: owns the heartbeat round-trip probe (a HEARTBEAT is answered by one ACK),
/// offsets, dedup, acks, flow control (the `InflightWindow`'s window,
/// capped at `WINDOW`, and the window reading rule, also applied while down), link timers and resume negotiation. Bridge and keeper never do offset arithmetic.
pub(super) struct SessionEndpoint {
    role: Role,
    token: Token,
    ring: ReplayRing,
    /// The in-flight window controller; fed only from here.
    window: InflightWindow,
    /// Highest offset ever put on the wire; bytes below it are replays (never sampled).
    fresh_end: u64,
    /// Next outbound offset to put on the wire.
    send_next: u64,
    /// Next inbound offset expected (everything below was received).
    recv_offset: u64,
    /// Everything below this reached the consumer.
    delivered: u64,
    last_ack_sent: u64,
    /// When the first not-yet-acked delivered byte was reported.
    ack_pending_since: Option<Instant>,
    /// A HEARTBEAT arrived and its ACK answer has not been sent.
    answer_heartbeat: bool,
    /// When the last HEARTBEAT went out with nothing unacked: the next ACK is its answer.
    probe_sent_at: Option<Instant>,
    phase: LinkPhase,
    decoder: Decoder,
    /// Retained session state: re-sent on every leg until the session is removed.
    final_end: Option<String>,
    /// The final END was emitted on the current leg.
    final_end_sent: bool,
    established: bool,
    last_contact: Option<Instant>,
    grace_ms: u32,
    /// Per-leg override of `SILENCE_TIMEOUT`; `None` is the Phase 1 value.
    silence_timeout: Option<Duration>,
    /// Sent in every HELLO while set (QUIC legs only).
    quic_token: Option<String>,
    /// Sent in every HELLO (3b render split).
    render_split: bool,
}

impl SessionEndpoint {
    pub fn new(role: Role, token: Token, ring_cap: usize) -> Self {
        SessionEndpoint {
            role,
            token,
            ring: ReplayRing::new(ring_cap),
            window: InflightWindow::new(0),
            fresh_end: 0,
            send_next: 0,
            recv_offset: 0,
            delivered: 0,
            last_ack_sent: 0,
            ack_pending_since: None,
            answer_heartbeat: false,
            probe_sent_at: None,
            phase: LinkPhase::Down,
            decoder: Decoder::default(),
            final_end: None,
            final_end_sent: false,
            established: false,
            last_contact: None,
            grace_ms: 0,
            silence_timeout: None,
            quic_token: None,
            render_split: false,
        }
    }

    /// Whether the next HELLOs ask for render-split delivery.
    // mosh-19g.2.9 (the attacher) is its only non-test caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_render_split(&mut self, split: bool) {
        self.render_split = split;
    }

    /// The QUIC admission token the next HELLOs carry; `None` for ssh legs.
    // The attacher's QUIC dialer (the attacher half of mosh-19g.1.3) is its only non-test caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_quic_token(&mut self, token: Option<String>) {
        self.quic_token = token;
    }

    /// Sets the inbound silence that kills the current leg; `None` restores `SILENCE_TIMEOUT`.
    pub fn set_silence_timeout(&mut self, timeout: Option<Duration>) {
        self.silence_timeout = timeout;
    }

    fn silence_limit(&self) -> Duration {
        self.silence_timeout.unwrap_or(SILENCE_TIMEOUT)
    }

    pub fn token(&self) -> Token {
        self.token
    }

    pub fn recv_offset(&self) -> u64 {
        self.recv_offset
    }

    /// Time of the last inbound byte on any leg; `None` before the first.
    pub fn last_inbound(&self) -> Option<Instant> {
        self.last_contact
    }

    pub fn outbound_end(&self) -> u64 {
        self.ring.end_offset()
    }

    /// Every outbound byte has been acked (the keeper drops an eof session on this).
    pub fn fully_acked(&self) -> bool {
        self.ring.occupancy() == 0
    }

    /// Smoothed RTT of the current leg from ACK and heartbeat samples; `None` while unknown.
    pub fn srtt(&self) -> Option<Duration> {
        self.window.srtt()
    }

    pub fn is_up(&self) -> bool {
        matches!(self.phase, LinkPhase::Up { .. })
    }

    #[cfg(test)]
    pub fn established(&self) -> bool {
        self.established
    }

    /// Local end: HELLO for the next leg. `resume` is false until the first `Welcomed(New)`,
    /// true for the rest of the session.
    pub fn hello(&mut self, now: Instant, grace_ms: u32, previous_token: Option<Token>) -> Vec<u8> {
        self.grace_ms = grace_ms;
        self.decoder = Decoder::default();
        self.send_next = self.ring.start_offset();
        self.phase = LinkPhase::PreWelcome { hello_at: now };
        SessionMsg::Hello(Hello {
            v: SESSION_PROTOCOL_VERSION,
            token: self.token,
            resume: self.established,
            recv_offset: self.recv_offset,
            grace_ms,
            previous_token,
            quic_token: self.quic_token.clone(),
            render_split: self.render_split,
        })
        .encode()
    }

    /// Keeper end: answer an accepted HELLO. The silence clock starts now.
    pub fn welcome(
        &mut self,
        now: Instant,
        status: WelcomeStatus,
        peer_recv_offset: u64,
    ) -> Result<Vec<u8>, LegProtocolError> {
        match status {
            WelcomeStatus::New => self.send_next = self.ring.start_offset(),
            WelcomeStatus::Resumed => self.resume_from(peer_recv_offset)?,
            WelcomeStatus::Unknown => {
                // Refusal: the leg is not brought up.
                return Ok(self.welcome_bytes(status));
            }
        }
        self.established = true;
        self.set_up(now);
        Ok(self.welcome_bytes(status))
    }

    fn welcome_bytes(&self, status: WelcomeStatus) -> Vec<u8> {
        SessionMsg::Welcome(Welcome {
            v: SESSION_PROTOCOL_VERSION,
            status,
            recv_offset: self.recv_offset,
        })
        .encode()
    }

    /// Encode a pre-WELCOME END (a refusal); state is unchanged.
    #[cfg(test)]
    pub fn refuse(&self, reason: &str) -> Vec<u8> {
        self.end_now(reason)
    }

    /// Encode an END for immediate use (the stop path and refusals).
    pub fn end_now(&self, reason: &str) -> Vec<u8> {
        SessionMsg::End(End {
            reason: reason.to_string(),
        })
        .encode()
    }

    fn resume_from(&mut self, peer_recv_offset: u64) -> Result<(), LegProtocolError> {
        if peer_recv_offset < self.ring.start_offset() || peer_recv_offset > self.ring.end_offset()
        {
            return leg_err(format!(
                "peer recv_offset {peer_recv_offset} outside replay range {}..={}",
                self.ring.start_offset(),
                self.ring.end_offset()
            ));
        }
        self.send_next = peer_recv_offset;
        Ok(())
    }

    fn set_up(&mut self, now: Instant) {
        self.final_end_sent = false;
        // A new leg may be a new path: start the window over.
        self.window.reset(self.ring.start_offset());
        self.phase = LinkPhase::Up {
            last_inbound: now,
            last_heartbeat: now,
        };
        // Acks sent on a dead leg may have been lost; re-announce what was delivered.
        self.ack_pending_since = (self.delivered > self.last_ack_sent).then_some(now);
        self.answer_heartbeat = false;
        self.probe_sent_at = None;
    }

    /// Feed inbound leg bytes. Any byte, including part of a message, resets the silence clock.
    pub fn on_leg_bytes(
        &mut self,
        now: Instant,
        bytes: &[u8],
    ) -> Result<Vec<SessionEvent>, LegBytesError> {
        let mut events = Vec::new();
        match self.decode_leg_bytes(now, bytes, &mut events) {
            Ok(()) => Ok(events),
            Err(error) => Err(LegBytesError {
                error,
                delivered: events,
            }),
        }
    }

    fn decode_leg_bytes(
        &mut self,
        now: Instant,
        bytes: &[u8],
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), LegProtocolError> {
        if !bytes.is_empty() {
            self.last_contact = Some(now);
            if let LinkPhase::Up { last_inbound, .. } = &mut self.phase {
                *last_inbound = now;
            }
        }
        self.decoder.push(bytes);
        while let Some(msg) = self.decoder.next()? {
            match msg {
                SessionMsg::Data { offset, bytes } => {
                    self.require_up("DATA")?;
                    if offset > self.recv_offset {
                        return leg_err(format!(
                            "DATA gap: offset {offset}, expected {}",
                            self.recv_offset
                        ));
                    }
                    let skip = (self.recv_offset - offset) as usize;
                    if skip < bytes.len() {
                        let fresh = bytes[skip..].to_vec();
                        self.recv_offset += fresh.len() as u64;
                        events.push(SessionEvent::Deliver(fresh));
                    }
                }
                SessionMsg::Ack(n) => {
                    self.require_up("ACK")?;
                    if self.ring.ack(n).is_err() {
                        return leg_err(format!("ACK {n} beyond sent bytes"));
                    }
                    self.send_next = self.send_next.max(n);
                    if let Some(sent) = self.probe_sent_at.take() {
                        if let Some(rtt) = now.checked_duration_since(sent) {
                            self.window.on_rtt_probe_sample(rtt);
                        }
                    }
                    self.window.on_ack(now, n);
                }
                SessionMsg::Heartbeat => {
                    self.require_up("HEARTBEAT")?;
                    self.answer_heartbeat = true;
                }
                SessionMsg::Hello(h) => {
                    if self.role != Role::Keeper || self.is_up() {
                        return leg_err("unexpected HELLO");
                    }
                    events.push(SessionEvent::Hello(h));
                    return Ok(());
                }
                SessionMsg::Welcome(w) => {
                    let LinkPhase::PreWelcome { .. } = self.phase else {
                        return leg_err("unexpected WELCOME");
                    };
                    if self.role != Role::Local || w.v != SESSION_PROTOCOL_VERSION {
                        return leg_err("unacceptable WELCOME");
                    }
                    match (w.status, self.established) {
                        (WelcomeStatus::New, false) => {
                            self.established = true;
                            self.send_next = self.ring.start_offset();
                            self.set_up(now);
                            events.push(SessionEvent::Welcomed(WelcomeStatus::New));
                        }
                        (WelcomeStatus::New, true) | (WelcomeStatus::Unknown, _) => {
                            events.push(SessionEvent::ResumeRefused);
                            return Ok(());
                        }
                        (WelcomeStatus::Resumed, true) => {
                            self.resume_from(w.recv_offset)?;
                            self.set_up(now);
                            events.push(SessionEvent::Welcomed(WelcomeStatus::Resumed));
                        }
                        (WelcomeStatus::Resumed, false) => {
                            return leg_err("WELCOME resumed for a session never established");
                        }
                    }
                }
                SessionMsg::End(e) => {
                    events.push(SessionEvent::PeerEnd(e.reason));
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn require_up(&self, what: &str) -> Result<(), LegProtocolError> {
        if self.is_up() {
            Ok(())
        } else {
            leg_err(format!("{what} before WELCOME"))
        }
    }

    /// The leg is gone: rewind to the ring start, drop partial input. A session-final END, queued or already emitted,
    /// is re-sent on the next leg.
    pub fn leg_down(&mut self) {
        self.final_end_sent = false;
        self.phase = LinkPhase::Down;
        self.send_next = self.ring.start_offset();
        self.window.reset(self.ring.start_offset());
        self.decoder = Decoder::default();
        self.ack_pending_since = None;
        self.last_ack_sent = 0;
        self.answer_heartbeat = false;
        self.probe_sent_at = None;
    }

    /// Bytes the owner may queue from its source now: up to the controller's window (capped by
    /// the ring), whether or not a leg is up. While down the window is the reset value, so an
    /// outage backlog stays with the source instead of filling the ring with stale bytes; bytes
    /// already queued past it are kept and replayed. 0 means do not read.
    pub fn source_room(&self) -> usize {
        self.window
            .current()
            .min(self.ring.capacity())
            .saturating_sub(self.ring.occupancy())
    }

    /// Whether the owner may read the source (`source_room() > 0`).
    pub fn may_read_source(&self) -> bool {
        self.source_room() > 0
    }

    /// How many bytes to read from the source now; 0 means wait. While a leg is up:
    /// `source_room()` rounded up to 4 KiB, at most one chunk, so a read cannot overshoot a
    /// small window by a chunk (ring occupancy may exceed the window by under 4 KiB; bytes in
    /// flight never do). While down: exactly `source_room()`, at most one chunk, so the ring
    /// never passes the reset window from source reads.
    pub fn source_read_size(&self) -> usize {
        let room = self.source_room();
        if room == 0 {
            0
        } else if self.is_up() {
            room.next_multiple_of(SOURCE_READ_QUANTUM)
                .min(MAX_DATA_CHUNK)
        } else {
            room.min(MAX_DATA_CHUNK)
        }
    }

    /// The controller's current in-flight window.
    pub fn window(&self) -> usize {
        self.window.current()
    }

    /// The controller's last closed epoch, once (the keeper traces it).
    pub fn take_window_snapshot(&mut self) -> Option<WindowSnapshot> {
        self.window.take_snapshot()
    }

    /// Queue source bytes (at most `MAX_DATA_CHUNK` per read) for sending. Overflow is reported;
    /// a source that reads within `source_room()` never overflows (a ring at least `WINDOW` plus
    /// one chunk never overflows while up), so it needs a source that pushes past the room, which
    /// only the local bridge does while down.
    pub fn push_source(&mut self, data: &[u8]) -> Result<(), RingOverflow> {
        let result = self.ring.push(data);
        debug_assert!(
            result.is_ok() || !self.is_up() || self.ring.capacity() < WINDOW + MAX_DATA_CHUNK,
            "ring overflow while a leg is up"
        );
        // The source filled the window: from here it is refused room this epoch.
        if self.is_up() && self.ring.occupancy() >= self.window.current() {
            self.window.on_window_limited();
        }
        result
    }

    /// The owner reports that everything below `offset` reached its consumer. Only delivered
    /// offsets are ever acked.
    pub fn delivered(&mut self, now: Instant, offset: u64) {
        let offset = offset.min(self.recv_offset);
        if offset <= self.delivered {
            return;
        }
        self.delivered = offset;
        if self.delivered > self.last_ack_sent && self.ack_pending_since.is_none() {
            self.ack_pending_since = Some(now);
        }
    }

    fn ack_due(&self, now: Instant) -> bool {
        self.answer_heartbeat
            || (self.delivered > self.last_ack_sent
                && (self.delivered - self.last_ack_sent >= ACK_EVERY_BYTES
                    || self
                        .ack_pending_since
                        .is_some_and(|since| now.saturating_duration_since(since) >= ACK_EVERY)))
    }

    fn take_ack(&mut self) -> Vec<u8> {
        self.answer_heartbeat = false;
        self.last_ack_sent = self.delivered;
        self.ack_pending_since = None;
        SessionMsg::Ack(self.delivered).encode()
    }

    /// The next message to write, or `None`. Call until `None`. One DATA chunk per call, so a
    /// control message never waits behind more than one chunk. Order: due ACK (including a heartbeat
    /// answer), due HEARTBEAT (preceded by a fresh ACK), one DATA chunk, the session-final END
    /// once the ring is sent.
    pub fn poll_send(&mut self, now: Instant) -> Option<Vec<u8>> {
        let LinkPhase::Up { last_heartbeat, .. } = self.phase else {
            return None;
        };
        if self.ack_due(now) {
            return Some(self.take_ack());
        }
        if now.saturating_duration_since(last_heartbeat) >= HEARTBEAT_INTERVAL {
            if self.delivered > self.last_ack_sent {
                return Some(self.take_ack());
            }
            if let LinkPhase::Up { last_heartbeat, .. } = &mut self.phase {
                *last_heartbeat = now;
            }
            // With nothing unacked the peer has no other reason to ACK: its next ACK answers this.
            self.probe_sent_at = self.fully_acked().then_some(now);
            return Some(SessionMsg::Heartbeat.encode());
        }
        let end = self.ring.end_offset();
        if self.send_next < end {
            let in_flight = (self.send_next - self.ring.start_offset()) as usize;
            let room = self.window.current().saturating_sub(in_flight);
            let n = MAX_DATA_CHUNK.min((end - self.send_next) as usize).min(room);
            if n > 0 {
                let bytes = self.ring.read_at(self.send_next, n)?;
                let offset = self.send_next;
                self.send_next += bytes.len() as u64;
                // The next ACK may be for this data, not the heartbeat: no clean sample.
                self.probe_sent_at = None;
                if self.send_next > self.fresh_end {
                    self.window
                        .on_send(now, offset.max(self.fresh_end), self.send_next);
                    self.fresh_end = self.send_next;
                }
                return Some(SessionMsg::Data { offset, bytes }.encode());
            }
            // Bytes to send and no room.
            self.window.on_window_limited();
            return None;
        }
        if !self.final_end_sent {
            if let Some(reason) = &self.final_end {
                self.final_end_sent = true;
                return Some(self.end_now(reason));
            }
        }
        None
    }

    /// An ACK for everything delivered, sent now regardless of the ack timer. `None` while no
    /// leg is up.
    pub fn ack_now(&mut self) -> Option<Vec<u8>> {
        self.is_up().then(|| self.take_ack())
    }

    /// A session-final END, sent after every ring byte has been sent. The first reason wins.
    pub fn queue_final_end(&mut self, reason: &str) {
        if self.final_end.is_none() {
            self.final_end = Some(reason.to_string());
        }
    }

    pub fn poll_timers(&self, now: Instant) -> Option<LinkVerdict> {
        match self.phase {
            LinkPhase::Up { last_inbound, .. }
                if now.saturating_duration_since(last_inbound) >= self.silence_limit() =>
            {
                Some(LinkVerdict::Silence)
            }
            LinkPhase::PreWelcome { hello_at }
                if now.saturating_duration_since(hello_at) >= PRE_WELCOME_BOUND =>
            {
                Some(LinkVerdict::PreWelcomeBound)
            }
            _ => None,
        }
    }

    /// The earliest heartbeat, ack or verdict instant, so owners can sleep precisely.
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        match self.phase {
            LinkPhase::Down => None,
            LinkPhase::PreWelcome { hello_at } => Some(hello_at + PRE_WELCOME_BOUND),
            LinkPhase::Up {
                last_inbound,
                last_heartbeat,
            } => {
                let mut next =
                    (last_inbound + self.silence_limit()).min(last_heartbeat + HEARTBEAT_INTERVAL);
                if self.ack_due(now) {
                    next = now;
                } else if let Some(since) = self.ack_pending_since {
                    next = next.min(since + ACK_EVERY);
                }
                Some(next)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::window::{CAP, INITIAL};
    use super::*;

    const MIB: usize = 1024 * 1024;

    fn tok() -> Token {
        "000102030405060708090a0b0c0d0e0f".parse().unwrap()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn hello_msg(resume: bool) -> Hello {
        Hello {
            v: SESSION_PROTOCOL_VERSION,
            token: tok(),
            resume,
            recv_offset: 7,
            grace_ms: 60_000,
            previous_token: None,
            quic_token: None,
            render_split: false,
        }
    }

    fn decode_all(bytes: &[u8]) -> Vec<SessionMsg> {
        let mut d = Decoder::default();
        d.push(bytes);
        let mut out = Vec::new();
        while let Some(m) = d.next().expect("decode") {
            out.push(m);
        }
        out
    }

    fn one(bytes: &[u8]) -> SessionMsg {
        let mut msgs = decode_all(bytes);
        assert_eq!(msgs.len(), 1);
        msgs.remove(0)
    }

    fn all_variants() -> Vec<SessionMsg> {
        vec![
            SessionMsg::Hello(Hello {
                previous_token: Some(tok()),
                ..hello_msg(true)
            }),
            SessionMsg::Hello(hello_msg(false)),
            SessionMsg::Welcome(Welcome {
                v: 1,
                status: WelcomeStatus::Resumed,
                recv_offset: 99,
            }),
            SessionMsg::Data {
                offset: 1 << 40,
                bytes: vec![1, 2, 3],
            },
            SessionMsg::Data {
                offset: 0,
                bytes: vec![9; MAX_DATA_CHUNK],
            },
            SessionMsg::Ack(u64::MAX),
            SessionMsg::Heartbeat,
            SessionMsg::End(End {
                reason: end_reason::SERVER_EOF.into(),
            }),
        ]
    }

    // ---- codec ----

    #[test]
    fn codec_round_trips_byte_by_byte_and_in_random_chunks() {
        let msgs = all_variants();
        let wire: Vec<u8> = msgs.iter().flat_map(SessionMsg::encode).collect();

        let mut d = Decoder::default();
        let mut got = Vec::new();
        for b in &wire {
            d.push(&[*b]);
            while let Some(m) = d.next().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);

        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut d = Decoder::default();
        let mut got = Vec::new();
        let mut pos = 0;
        while pos < wire.len() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let n = 1 + (seed >> 33) as usize % 5000;
            let end = (pos + n).min(wire.len());
            d.push(&wire[pos..end]);
            pos = end;
            while let Some(m) = d.next().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, msgs);
    }

    #[test]
    fn wire_layout_and_json_field_names() {
        let wire = SessionMsg::Hello(hello_msg(false)).encode();
        assert_eq!(wire[0], 1);
        let len = u32::from_le_bytes(wire[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, wire.len() - 5);
        assert_eq!(
            std::str::from_utf8(&wire[5..]).unwrap(),
            r#"{"v":1,"token":"000102030405060708090a0b0c0d0e0f","resume":false,"recv_offset":7,"grace_ms":60000}"#
        );
        let with_prev = SessionMsg::Hello(Hello {
            previous_token: Some(tok()),
            ..hello_msg(true)
        })
        .encode();
        assert!(std::str::from_utf8(&with_prev[5..])
            .unwrap()
            .ends_with(r#""previous_token":"000102030405060708090a0b0c0d0e0f"}"#));

        let welcome = SessionMsg::Welcome(Welcome {
            v: 1,
            status: WelcomeStatus::Unknown,
            recv_offset: 0,
        })
        .encode();
        assert_eq!(welcome[0], 2);
        assert_eq!(
            std::str::from_utf8(&welcome[5..]).unwrap(),
            r#"{"v":1,"status":"unknown","recv_offset":0}"#
        );

        let data = SessionMsg::Data {
            offset: 0x0102,
            bytes: vec![0xaa],
        }
        .encode();
        assert_eq!(data, [3, 9, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 0xaa]);
        assert_eq!(SessionMsg::Ack(1).encode(), [4, 8, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(SessionMsg::Heartbeat.encode(), [5, 0, 0, 0, 0]);
        assert_eq!(
            SessionMsg::End(End { reason: "x".into() }).encode()[0],
            6
        );
    }

    #[test]
    fn oversized_data_is_rejected_at_the_header() {
        let mut d = Decoder::default();
        let mut header = vec![TYPE_DATA];
        header.extend_from_slice(&((8 + MAX_DATA_CHUNK + 1) as u32).to_le_bytes());
        d.push(&header); // only the header: no payload buffered yet
        assert!(d.next().is_err());

        let ok = SessionMsg::Data {
            offset: 0,
            bytes: vec![0; MAX_DATA_CHUNK],
        }
        .encode();
        assert!(matches!(one(&ok), SessionMsg::Data { .. }));
    }

    #[test]
    fn decoder_rejects_bad_headers_and_json() {
        let bad = |bytes: &[u8]| {
            let mut d = Decoder::default();
            d.push(bytes);
            d.next().is_err()
        };
        assert!(bad(&[9, 0, 0, 0, 0]), "unknown type");
        assert!(bad(&[0, 0, 0, 0, 0]), "type 0");
        assert!(bad(&[4, 7, 0, 0, 0]), "short ack");
        assert!(bad(&[5, 1, 0, 0, 0]), "heartbeat with payload");
        assert!(bad(&[3, 7, 0, 0, 0]), "data without offset");
        let mut big = vec![TYPE_END];
        big.extend_from_slice(&((MAX_CONTROL_PAYLOAD + 1) as u32).to_le_bytes());
        assert!(bad(&big), "control over 4096");
        let mut junk = vec![TYPE_HELLO, 3, 0, 0, 0];
        junk.extend_from_slice(b"{x}");
        assert!(bad(&junk), "malformed json");
        let mut missing = vec![TYPE_END, 2, 0, 0, 0];
        missing.extend_from_slice(b"{}");
        assert!(bad(&missing), "missing field");
    }

    #[test]
    fn token_hex_round_trip_and_validation() {
        let t = tok();
        assert_eq!(t.to_string(), "000102030405060708090a0b0c0d0e0f");
        assert_eq!(format!("{t:?}"), t.to_string());
        assert_eq!(t.to_string().parse::<Token>().unwrap(), t);
        assert!("0".repeat(31).parse::<Token>().is_err());
        assert!("0".repeat(33).parse::<Token>().is_err());
        assert!(format!("{}g", "0".repeat(31)).parse::<Token>().is_err());
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Token>(&json).unwrap(), t);
        assert!(serde_json::from_str::<Token>("\"abc\"").is_err());
        assert_ne!(Token::generate().unwrap(), Token::generate().unwrap());
    }

    #[test]
    fn hello_without_quic_token_omits_the_field() {
        let wire = SessionMsg::Hello(hello_msg(false)).encode();
        assert!(!std::str::from_utf8(&wire[5..]).unwrap().contains("quic_token"));
    }

    #[test]
    fn old_hello_json_parses() {
        let json = br#"{"v":1,"token":"000102030405060708090a0b0c0d0e0f","resume":true,"recv_offset":7,"grace_ms":60000}"#;
        let mut wire = vec![TYPE_HELLO];
        wire.extend_from_slice(&(json.len() as u32).to_le_bytes());
        wire.extend_from_slice(json);
        let SessionMsg::Hello(hello) = one(&wire) else {
            panic!("not a HELLO");
        };
        assert_eq!(hello.quic_token, None);
        assert_eq!(hello, hello_msg(true));
    }

    #[test]
    fn hello_render_split_round_trips() {
        let mut endpoint = SessionEndpoint::new(Role::Local, tok(), 1024);
        let off = endpoint.hello(Instant::now(), 60_000, None);
        assert!(!std::str::from_utf8(&off[5..]).unwrap().contains("render_split"));
        let SessionMsg::Hello(hello) = one(&off) else {
            panic!("not a HELLO");
        };
        assert!(!hello.render_split);

        endpoint.set_render_split(true);
        let on = endpoint.hello(Instant::now(), 60_000, None);
        assert!(std::str::from_utf8(&on[5..]).unwrap().contains(r#""render_split":true"#));
        let SessionMsg::Hello(hello) = one(&on) else {
            panic!("not a HELLO");
        };
        assert!(hello.render_split);
    }

    #[test]
    fn old_hello_json_parses_as_not_split() {
        let json = br#"{"v":1,"token":"000102030405060708090a0b0c0d0e0f","resume":false,"recv_offset":0,"grace_ms":1}"#;
        let mut wire = vec![TYPE_HELLO];
        wire.extend_from_slice(&(json.len() as u32).to_le_bytes());
        wire.extend_from_slice(json);
        let SessionMsg::Hello(hello) = one(&wire) else {
            panic!("not a HELLO");
        };
        assert!(!hello.render_split);
    }

    #[test]
    fn hello_with_quic_token_round_trips() {
        let mut endpoint = SessionEndpoint::new(Role::Local, tok(), 1024);
        endpoint.set_quic_token(Some("ab".repeat(32)));
        let bytes = endpoint.hello(Instant::now(), 60_000, None);
        let SessionMsg::Hello(hello) = one(&bytes) else {
            panic!("not a HELLO");
        };
        assert_eq!(hello.quic_token, Some("ab".repeat(32)));
        assert!(std::str::from_utf8(&bytes[5..]).unwrap().contains(r#""quic_token":"abab"#));
    }

    // ---- negotiation ----

    #[test]
    fn answer_hello_covers_every_cell() {
        let h = |v, resume| Hello {
            v,
            resume,
            ..hello_msg(false)
        };
        let v = SESSION_PROTOCOL_VERSION;
        assert_eq!(answer_hello(&h(v, false), false), HelloAnswer::Welcome(WelcomeStatus::New));
        assert_eq!(answer_hello(&h(v, false), true), HelloAnswer::RefuseTokenInUse);
        assert_eq!(answer_hello(&h(v, true), false), HelloAnswer::Welcome(WelcomeStatus::Unknown));
        assert_eq!(answer_hello(&h(v, true), true), HelloAnswer::Welcome(WelcomeStatus::Resumed));
        for resume in [false, true] {
            for exists in [false, true] {
                assert_eq!(answer_hello(&h(v + 1, resume), exists), HelloAnswer::RefuseVersion);
            }
        }
    }

    #[test]
    fn setup_deadline_is_min_of_remaining_grace_and_15s() {
        let t0 = Instant::now();
        assert_eq!(setup_deadline(t0, t0 + secs(5)), t0 + secs(5));
        assert_eq!(setup_deadline(t0, t0 + secs(60)), t0 + secs(15));
        assert_eq!(setup_deadline(t0, t0 + secs(15)), t0 + secs(15));
    }

    // ---- endpoint harness ----

    struct Pair {
        local: SessionEndpoint,
        keeper: SessionEndpoint,
        /// Bytes delivered to the keeper's consumer (local -> keeper stream).
        up: Vec<u8>,
        /// Bytes delivered to the local consumer.
        down: Vec<u8>,
        ends_at_keeper: Vec<String>,
        ends_at_local: Vec<String>,
        /// Every whole message that crossed the wire: (to_keeper, message).
        log: Vec<(bool, SessionMsg)>,
        wire_bytes: usize,
        now: Instant,
        keeper_has_session: bool,
    }

    impl Pair {
        fn new(local_cap: usize, keeper_cap: usize) -> Pair {
            Pair {
                local: SessionEndpoint::new(Role::Local, tok(), local_cap),
                keeper: SessionEndpoint::new(Role::Keeper, tok(), keeper_cap),
                up: Vec::new(),
                down: Vec::new(),
                ends_at_keeper: Vec::new(),
                ends_at_local: Vec::new(),
                log: Vec::new(),
                wire_bytes: 0,
                now: Instant::now(),
                keeper_has_session: false,
            }
        }

        /// HELLO / WELCOME handshake through the real negotiation helper.
        fn connect(&mut self) -> Vec<SessionEvent> {
            let hello = self.local.hello(self.now, 60_000, None);
            let evs = self.keeper.on_leg_bytes(self.now, &hello).unwrap();
            let [SessionEvent::Hello(h)] = &evs[..] else {
                panic!("expected one Hello event, got {evs:?}");
            };
            let HelloAnswer::Welcome(status) = answer_hello(h, self.keeper_has_session) else {
                panic!("hello refused");
            };
            let welcome = self.keeper.welcome(self.now, status, h.recv_offset).unwrap();
            self.keeper_has_session = true;
            assert!(self.keeper.on_leg_bytes(self.now, &[]).unwrap().is_empty());
            self.local.on_leg_bytes(self.now, &welcome).unwrap()
        }

        fn leg_down(&mut self) {
            self.local.leg_down();
            self.keeper.leg_down();
        }

        fn feed(&mut self, to_keeper: bool, bytes: &[u8]) {
            let now = self.now;
            let (rx, sink, ends) = if to_keeper {
                (&mut self.keeper, &mut self.up, &mut self.ends_at_keeper)
            } else {
                (&mut self.local, &mut self.down, &mut self.ends_at_local)
            };
            for ev in rx.on_leg_bytes(now, bytes).expect("leg error") {
                match ev {
                    SessionEvent::Deliver(d) => {
                        sink.extend_from_slice(&d);
                        let off = rx.recv_offset();
                        rx.delivered(now, off);
                    }
                    SessionEvent::PeerEnd(r) => ends.push(r),
                    other => panic!("unexpected event {other:?}"),
                }
            }
        }

        /// One pass moving everything both ends want to send. `budget` limits the total wire
        /// bytes still allowed; returns (moved anything, cut hit).
        fn round(&mut self, budget: &mut Option<usize>) -> (bool, bool) {
            let mut moved = false;
            for to_keeper in [true, false] {
                loop {
                    let msg = if to_keeper {
                        self.local.poll_send(self.now)
                    } else {
                        self.keeper.poll_send(self.now)
                    };
                    let Some(msg) = msg else { break };
                    if msg[0] == TYPE_DATA {
                        let ep = if to_keeper { &self.local } else { &self.keeper };
                        let in_flight = (ep.send_next - ep.ring.start_offset()) as usize;
                        assert!(
                            in_flight <= ep.window(),
                            "in flight {in_flight} > window {}",
                            ep.window()
                        );
                    }
                    moved = true;
                    let mut deliver = msg.len();
                    let mut cut = false;
                    if let Some(b) = budget {
                        if msg.len() > *b {
                            deliver = *b;
                            cut = true;
                        }
                        *b -= deliver;
                    }
                    self.wire_bytes += deliver;
                    if cut {
                        self.feed(to_keeper, &msg[..deliver]);
                        return (true, true);
                    }
                    self.log.push((to_keeper, one(&msg)));
                    self.feed(to_keeper, &msg);
                }
            }
            (moved, false)
        }

        /// Run (advancing 50 ms per round) until quiet; true if the byte budget cut the leg.
        fn pump(&mut self, mut budget: Option<usize>) -> bool {
            loop {
                self.now += ms(50);
                let (moved, cut) = self.round(&mut budget);
                if cut {
                    return true;
                }
                if !moved {
                    return false;
                }
            }
        }

        fn count(&self, to_keeper: bool, f: impl Fn(&SessionMsg) -> bool) -> usize {
            self.log.iter().filter(|(d, m)| *d == to_keeper && f(m)).count()
        }
    }

    fn is_data(m: &SessionMsg) -> bool {
        matches!(m, SessionMsg::Data { .. })
    }

    fn drain(ep: &mut SessionEndpoint, now: Instant) -> Vec<SessionMsg> {
        let mut out = Vec::new();
        while let Some(b) = ep.poll_send(now) {
            out.push(one(&b));
        }
        out
    }

    fn data_bytes(msgs: &[SessionMsg]) -> usize {
        msgs.iter()
            .map(|m| match m {
                SessionMsg::Data { bytes, .. } => bytes.len(),
                _ => 0,
            })
            .sum()
    }

    fn push_chunks(ep: &mut SessionEndpoint, total: usize) {
        let mut left = total;
        while left > 0 {
            let n = left.min(MAX_DATA_CHUNK);
            ep.push_source(&vec![0x5a; n]).unwrap();
            left -= n;
        }
    }

    // ---- flow control ----

    #[test]
    fn data_over_chunk_is_leg_error() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let now = p.now;
        let mut header = vec![TYPE_DATA];
        header.extend_from_slice(&((8 + MAX_DATA_CHUNK + 1) as u32).to_le_bytes());
        assert!(p.keeper.on_leg_bytes(now, &header).is_err());

        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let at_limit = SessionMsg::Data {
            offset: 0,
            bytes: vec![1; MAX_DATA_CHUNK],
        }
        .encode();
        let evs = p.keeper.on_leg_bytes(p.now, &at_limit).unwrap();
        assert_eq!(evs, vec![SessionEvent::Deliver(vec![1; MAX_DATA_CHUNK])]);
    }

    #[test]
    fn acks_follow_delivery_only() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 100 * 1024);
        let now = p.now;
        let mut received = 0;
        for m in drain(&mut p.local, now) {
            let evs = p.keeper.on_leg_bytes(now, &SessionMsg::encode(&m)).unwrap();
            for ev in evs {
                if let SessionEvent::Deliver(d) = ev {
                    received += d.len();
                }
            }
        }
        assert_eq!(received, 100 * 1024);
        // The consumer has taken only 10 KiB; the other 90 KiB is received-but-queued.
        p.keeper.delivered(now, 10 * 1024);
        let mut acks = Vec::new();
        for step in 0..40u64 {
            let t = now + ms(100 * step);
            for m in drain(&mut p.keeper, t) {
                if let SessionMsg::Ack(n) = m {
                    acks.push(n);
                }
            }
        }
        assert_eq!(acks, vec![10 * 1024]);
    }

    #[test]
    fn window_limits_reading_and_in_flight() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        assert_eq!(p.local.window(), INITIAL);
        let mut pushed = 0;
        while p.local.may_read_source() {
            let n = p.local.source_read_size();
            p.local.push_source(&vec![7; n]).unwrap();
            pushed += n;
        }
        // INITIAL is a multiple of the read quantum, so the fill is exact.
        assert_eq!((pushed, p.local.source_room()), (INITIAL, 0));
        let now = p.now;
        assert_eq!(data_bytes(&drain(&mut p.local, now)), INITIAL);
        assert!(p.local.poll_send(now).is_none());
        // An ACK of 128 KiB reopens the window by that much, plus any growth the epoch close
        // granted (the source filled the window, so the epoch was window-limited).
        p.local
            .on_leg_bytes(now, &SessionMsg::Ack(128 * 1024).encode())
            .unwrap();
        let reopened = p.local.window() - (INITIAL - 128 * 1024);
        assert_eq!(p.local.source_room(), reopened);
        while p.local.may_read_source() {
            let n = p.local.source_read_size();
            p.local.push_source(&vec![7; n]).unwrap();
        }
        assert_eq!(data_bytes(&drain(&mut p.local, now)), reopened);
    }

    #[test]
    fn source_room_is_zero_at_the_window_and_sized_reads_stay_near_it() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        // An odd occupancy: interactive bytes first.
        p.local.push_source(&[1; 1000]).unwrap();
        // Pushing at most source_room() never exceeds the window.
        while p.local.source_room() > 0 {
            let n = p.local.source_room().min(MAX_DATA_CHUNK);
            p.local.push_source(&vec![2; n]).unwrap();
            assert!(p.local.ring.occupancy() <= p.local.window());
        }
        assert_eq!(p.local.ring.occupancy(), p.local.window());
        assert_eq!((p.local.source_room(), p.local.source_read_size()), (0, 0));
        assert!(!p.local.may_read_source());
        // source_read_size rounds up to 4 KiB: the ring may pass the window by < 4 KiB, never more.
        let mut q = Pair::new(8 * MIB, MIB);
        q.connect();
        q.local.push_source(&[1; 1000]).unwrap();
        while q.local.may_read_source() {
            let n = q.local.source_read_size();
            assert!(n.is_multiple_of(SOURCE_READ_QUANTUM) && n <= MAX_DATA_CHUNK);
            q.local.push_source(&vec![2; n]).unwrap();
        }
        assert!(q.local.ring.occupancy() < q.local.window() + SOURCE_READ_QUANTUM);
        let now = q.now;
        let sent = data_bytes(&drain(&mut q.local, now));
        assert_eq!(sent, q.local.window(), "in flight is capped at the window exactly");
    }

    #[test]
    fn down_reads_stop_at_initial_and_a_forced_push_still_reports_overflow() {
        // A ring much larger than INITIAL: while down, reads stop at INITIAL, not at the cap.
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        p.leg_down();
        assert_eq!(p.local.window(), INITIAL);
        // An odd occupancy first: down reads are sized to the room exactly (no 4 KiB rounding).
        p.local.push_source(&[1; 1000]).unwrap();
        while p.local.may_read_source() {
            let n = p.local.source_read_size();
            assert!(n > 0 && n <= MAX_DATA_CHUNK && n <= p.local.source_room());
            p.local.push_source(&vec![1; n]).unwrap();
            assert!(p.local.ring.occupancy() <= INITIAL);
        }
        assert_eq!(p.local.ring.occupancy(), INITIAL);
        assert_eq!((p.local.source_room(), p.local.source_read_size()), (0, 0));

        // The ring cap stays the hard cap: a ring smaller than INITIAL stops at its capacity,
        // and a push past it (the local bridge reads while down regardless) still overflows.
        let cap = 2 * MAX_DATA_CHUNK;
        assert!(cap < INITIAL);
        let mut small = Pair::new(cap, MIB);
        small.connect();
        small.leg_down();
        while small.local.may_read_source() {
            let n = small.local.source_read_size();
            small.local.push_source(&vec![1; n]).unwrap();
        }
        assert_eq!(small.local.ring.occupancy(), cap);
        let err = small.local.push_source(&[1]).unwrap_err();
        assert_eq!((err.capacity, err.attempted), (cap, cap + 1));
        assert_eq!(small.local.outbound_end(), cap as u64);
    }

    #[test]
    fn bytes_queued_past_initial_before_a_drop_are_kept_and_replayed_in_order() {
        let mut p = Pair::new(8 * MIB, 8 * MIB);
        p.connect();
        let pat: Vec<u8> = (0..MIB).map(|i| (i % 251) as u8).collect();
        for piece in pat.chunks(MAX_DATA_CHUNK) {
            p.local.push_source(piece).unwrap();
        }
        p.round(&mut None);
        p.leg_down();
        assert!(p.local.ring.occupancy() > INITIAL);
        assert_eq!(p.local.source_room(), 0);
        assert_eq!(p.local.source_read_size(), 0);
        assert!(!p.local.may_read_source());
        assert_eq!(
            p.connect(),
            vec![SessionEvent::Welcomed(WelcomeStatus::Resumed)]
        );
        assert!(!p.pump(None));
        assert_eq!(p.up, pat);
        assert_eq!(p.local.ring.occupancy(), 0);
        assert!(p.local.may_read_source());
    }

    #[test]
    fn after_resume_the_window_rule_governs_reads_again() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        p.leg_down();
        while p.local.may_read_source() {
            let n = p.local.source_read_size();
            p.local.push_source(&vec![3; n]).unwrap();
        }
        assert_eq!(p.local.ring.occupancy(), INITIAL);
        p.connect();
        assert_eq!(
            p.local.source_room(),
            p.local.window().saturating_sub(p.local.ring.occupancy())
        );
        assert!(!p.pump(None));
        assert_eq!(p.local.ring.occupancy(), 0);
        let n = p.local.source_read_size();
        assert_eq!(
            n,
            p.local
                .source_room()
                .next_multiple_of(SOURCE_READ_QUANTUM)
                .min(MAX_DATA_CHUNK)
        );
        assert_eq!(n % SOURCE_READ_QUANTUM, 0);
        assert!(n > 0);
    }

    #[test]
    fn replay_respects_window() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        p.leg_down();
        push_chunks(&mut p.local, 6 * MIB);
        let evs = p.connect();
        assert_eq!(evs, vec![SessionEvent::Welcomed(WelcomeStatus::Resumed)]);
        // First round: the replay stops at the window although 6 MiB is queued.
        p.round(&mut None);
        assert_eq!(
            p.log.iter().map(|(_, m)| data_bytes(std::slice::from_ref(m))).sum::<usize>(),
            INITIAL
        );
        // Acks release more, never more than the window beyond the acked point.
        assert!(!p.pump(None));
        assert_eq!(p.up.len(), 6 * MIB);
        let mut inflight_max = 0u64;
        let mut acked = 0u64;
        for (to_keeper, m) in &p.log {
            match (to_keeper, m) {
                (false, SessionMsg::Ack(n)) => acked = acked.max(*n),
                (true, SessionMsg::Data { offset, bytes }) => {
                    inflight_max = inflight_max.max(offset + bytes.len() as u64 - acked);
                }
                _ => {}
            }
        }
        // The per-send invariant in `Pair::round` checks the exact window; CAP bounds it overall.
        assert!(inflight_max <= CAP as u64, "in flight {inflight_max}");
    }

    #[test]
    fn a_flood_grows_the_window_on_either_side_and_sparse_input_keeps_initial() {
        for keeper_floods in [true, false] {
            let mut p = Pair::new(8 * MIB, 8 * MIB);
            p.connect();
            for _ in 0..60 {
                let (flooder, typist) = if keeper_floods {
                    (&mut p.keeper, &mut p.local)
                } else {
                    (&mut p.local, &mut p.keeper)
                };
                while flooder.may_read_source() {
                    let n = flooder.source_read_size();
                    flooder.push_source(&vec![1; n]).unwrap();
                }
                typist.push_source(b"ls\r").unwrap();
                p.pump(None);
            }
            let (flooder, typist) = if keeper_floods {
                (&p.keeper, &p.local)
            } else {
                (&p.local, &p.keeper)
            };
            assert!(
                flooder.window() > INITIAL,
                "keeper_floods={keeper_floods}: {}",
                flooder.window()
            );
            assert_eq!(typist.window(), INITIAL, "keeper_floods={keeper_floods}");
        }
    }

    #[test]
    fn a_grown_window_resets_to_initial_on_leg_down_and_down_reads_stop_there() {
        // Grow the keeper's window with the flood loop of
        // `a_flood_grows_the_window_on_either_side_and_sparse_input_keeps_initial`. The rings are
        // far larger than any window, so only the window can stop reads.
        let mut p = Pair::new(8 * MIB, 8 * MIB);
        p.connect();
        for _ in 0..60 {
            while p.keeper.may_read_source() {
                let n = p.keeper.source_read_size();
                p.keeper.push_source(&vec![1; n]).unwrap();
            }
            p.local.push_source(b"ls\r").unwrap();
            p.pump(None);
        }
        let grown = p.keeper.window();
        assert!(grown > INITIAL, "the flood did not grow the window: {grown}");
        // Everything sent was delivered and acked, so the down fill starts at most at INITIAL.
        let queued = p.keeper.ring.occupancy();
        assert!(queued <= INITIAL, "queued {queued} before the drop");

        p.leg_down();
        assert_eq!(
            p.keeper.window(),
            INITIAL,
            "leg_down kept the grown window {grown}"
        );
        // While down, reads stop with the ring at the reset window, not at the grown window.
        while p.keeper.may_read_source() {
            let n = p.keeper.source_read_size();
            assert!(n > 0 && n <= p.keeper.source_room());
            p.keeper.push_source(&vec![2; n]).unwrap();
        }
        assert_eq!(p.keeper.ring.occupancy(), INITIAL);
        assert_eq!(
            (p.keeper.source_room(), p.keeper.source_read_size()),
            (0, 0)
        );
    }

    #[test]
    fn replayed_bytes_are_never_reported_to_the_controller() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, INITIAL);
        let now = p.now;
        drain(&mut p.local, now); // sent, never delivered
        p.leg_down();
        p.now += secs(5);
        assert_eq!(p.connect(), vec![SessionEvent::Welcomed(WelcomeStatus::Resumed)]);
        assert!(!p.pump(None));
        assert_eq!(p.up.len(), INITIAL);
        assert_eq!(p.local.window.samples(), 0, "replayed bytes were sampled");
        push_chunks(&mut p.local, 256 * 1024);
        assert!(!p.pump(None));
        assert!(p.local.window.samples() > 0, "fresh bytes were not sampled");
    }

    #[test]
    fn control_never_behind_more_than_one_chunk() {
        let mut p = Pair::new(2 * MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, MIB);
        let now = p.now;
        // One chunk goes out; then an ACK becomes due.
        assert!(is_data(&one(&p.local.poll_send(now).unwrap())));
        let data = SessionMsg::Data {
            offset: 0,
            bytes: vec![1; MAX_DATA_CHUNK],
        }
        .encode();
        p.local.on_leg_bytes(now, &data).unwrap();
        p.local.delivered(now, MAX_DATA_CHUNK as u64);
        assert_eq!(
            one(&p.local.poll_send(now).unwrap()),
            SessionMsg::Ack(MAX_DATA_CHUNK as u64)
        );
        // A heartbeat due with a megabyte queued is next, ahead of the data.
        let later = now + secs(2);
        assert_eq!(one(&p.local.poll_send(later).unwrap()), SessionMsg::Heartbeat);
        assert!(is_data(&one(&p.local.poll_send(later).unwrap())));
    }

    #[test]
    fn pre_welcome_refusal_is_a_single_message_and_leaves_state() {
        let mut p = Pair::new(MIB, MIB);
        push_chunks(&mut p.keeper, 100 * 1024);
        let refusal = p.keeper.refuse(end_reason::PROTOCOL_VERSION);
        assert_eq!(
            one(&refusal),
            SessionMsg::End(End {
                reason: "protocol-version".into()
            })
        );
        assert!(!p.keeper.is_up());
        assert_eq!(p.keeper.outbound_end(), 100 * 1024);
        // The local end sees it as a refusal before WELCOME.
        p.local.hello(p.now, 1, None);
        assert_eq!(
            p.local.on_leg_bytes(p.now, &refusal).unwrap(),
            vec![SessionEvent::PeerEnd("protocol-version".into())]
        );
    }

    #[test]
    fn final_end_only_after_every_ring_byte_is_sent() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 200 * 1024);
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        let now = p.now;
        let msgs = drain(&mut p.local, now);
        let (last, rest) = msgs.split_last().unwrap();
        assert!(matches!(last, SessionMsg::End(e) if e.reason == "client-gone"));
        assert_eq!(rest.len(), 4);
        assert!(rest.iter().all(is_data));
        assert_eq!(data_bytes(rest), 200 * 1024);

        // Window-blocked: the END waits for the window to reopen and the ring to be sent.
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 2 * MIB + 512 * 1024);
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        let now = p.now;
        let first = drain(&mut p.local, now);
        assert_eq!(data_bytes(&first), INITIAL);
        assert!(!first.iter().any(|m| matches!(m, SessionMsg::End(_))));
        let mut acked = INITIAL as u64;
        let mut rest = Vec::new();
        for _ in 0..100 {
            p.local
                .on_leg_bytes(now, &SessionMsg::Ack(acked).encode())
                .unwrap();
            let batch = drain(&mut p.local, now);
            acked += data_bytes(&batch) as u64;
            let ended = batch.iter().any(|m| matches!(m, SessionMsg::End(_)));
            rest.extend(batch);
            if ended {
                break;
            }
        }
        assert_eq!(INITIAL + data_bytes(&rest), 2 * MIB + 512 * 1024);
        assert!(matches!(rest.last(), Some(SessionMsg::End(_))));
    }

    #[test]
    fn final_end_survives_leg_down_and_goes_after_replay() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 100 * 1024);
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        p.leg_down();
        p.connect();
        let now = p.now;
        let msgs = drain(&mut p.local, now);
        assert_eq!(data_bytes(&msgs), 100 * 1024);
        assert!(matches!(msgs.last(), Some(SessionMsg::End(_))));
    }

    #[test]
    fn final_end_taken_then_leg_down_is_resent_after_replay() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 100 * 1024);
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        let now = p.now;
        let first = drain(&mut p.local, now);
        assert!(matches!(first.last(), Some(SessionMsg::End(_))));
        p.leg_down();
        p.connect();
        let now = p.now;
        let msgs = drain(&mut p.local, now);
        assert_eq!(data_bytes(&msgs), 100 * 1024);
        assert!(matches!(msgs.last(), Some(SessionMsg::End(e)) if e.reason == "client-gone"));
    }

    #[test]
    fn final_end_emitted_once_per_leg() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        // A later reason must not overwrite the first.
        p.local.queue_final_end(end_reason::SERVER_EOF);
        let now = p.now;
        let msgs = drain(&mut p.local, now);
        assert!(matches!(msgs.last(), Some(SessionMsg::End(e)) if e.reason == "client-gone"));
        assert!(p.local.poll_send(now).is_none());
        assert!(p.local.poll_send(now + secs(1)).is_none());
    }

    #[test]
    fn ack_now_bypasses_ack_timer() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let now = p.now;
        let data = SessionMsg::Data { offset: 0, bytes: vec![7; 100] }.encode();
        p.local.on_leg_bytes(now, &data).unwrap();
        let off = p.local.recv_offset();
        p.local.delivered(now, off);
        assert!(p.local.poll_send(now).is_none());
        assert_eq!(p.local.ack_now(), Some(SessionMsg::Ack(100).encode()));
        p.local.leg_down();
        assert_eq!(p.local.ack_now(), None);
    }

    // ---- timers ----

    #[test]
    fn silence_clock_starts_at_welcome_and_resets_on_partial_message() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        assert_eq!(p.local.poll_timers(t0 + ms(7999)), None);
        assert_eq!(p.local.poll_timers(t0 + secs(8)), Some(LinkVerdict::Silence));
        // Three bytes of a five-byte heartbeat at t0+7 s defer the verdict to t0+15 s.
        let hb = SessionMsg::Heartbeat.encode();
        assert!(p.local.on_leg_bytes(t0 + secs(7), &hb[..3]).unwrap().is_empty());
        assert_eq!(p.local.poll_timers(t0 + ms(14999)), None);
        assert_eq!(p.local.poll_timers(t0 + secs(15)), Some(LinkVerdict::Silence));
        assert_eq!(p.local.last_inbound(), Some(t0 + secs(7)));
    }

    #[test]
    fn silence_override_extends_the_verdict() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        p.keeper.set_silence_timeout(Some(secs(30)));
        assert_eq!(p.keeper.poll_timers(t0 + secs(10)), None);
        assert_eq!(p.keeper.poll_timers(t0 + ms(29999)), None);
        assert_eq!(p.keeper.poll_timers(t0 + secs(30)), Some(LinkVerdict::Silence));
        p.keeper.set_silence_timeout(None);
        assert_eq!(p.keeper.poll_timers(t0 + secs(8)), Some(LinkVerdict::Silence));
    }

    #[test]
    fn keeper_silence_clock_starts_when_it_writes_welcome() {
        let mut p = Pair::new(MIB, MIB);
        p.now += secs(3);
        p.connect();
        let t = p.now;
        assert_eq!(p.keeper.poll_timers(t + ms(7999)), None);
        assert_eq!(p.keeper.poll_timers(t + secs(8)), Some(LinkVerdict::Silence));
    }

    #[test]
    fn pre_welcome_only_the_20s_bound_applies() {
        let mut p = Pair::new(MIB, MIB);
        let t0 = p.now;
        p.local.hello(t0, 60_000, None);
        assert_eq!(p.local.poll_timers(t0 + secs(8)), None);
        assert_eq!(p.local.poll_timers(t0 + secs(19)), None);
        assert_eq!(
            p.local.poll_timers(t0 + secs(20)),
            Some(LinkVerdict::PreWelcomeBound)
        );
        // Bytes before WELCOME do not extend the bound.
        let hb = SessionMsg::Heartbeat.encode();
        let _ = p.local.on_leg_bytes(t0 + secs(19), &hb[..2]);
        assert_eq!(
            p.local.poll_timers(t0 + secs(20)),
            Some(LinkVerdict::PreWelcomeBound)
        );
        assert_eq!(p.keeper.poll_timers(t0 + secs(100)), None);
    }

    #[test]
    fn silence_verdict_at_8s_and_deadlines() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        assert_eq!(p.local.next_deadline(t0), Some(t0 + secs(2)));
        assert_eq!(p.local.poll_timers(t0 + secs(2)), None);
        p.local.leg_down();
        assert_eq!(p.local.poll_timers(t0 + secs(60)), None);
        assert_eq!(p.local.next_deadline(t0), None);
        p.local.hello(t0, 1, None);
        assert_eq!(p.local.next_deadline(t0), Some(t0 + secs(20)));
    }

    #[test]
    fn idle_heartbeats_keep_link_alive() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let start = p.now;
        for _ in 0..60 {
            p.now += ms(500);
            let mut no_budget = None;
            p.round(&mut no_budget);
            assert_eq!(p.local.poll_timers(p.now), None);
            assert_eq!(p.keeper.poll_timers(p.now), None);
        }
        assert!(p.now - start >= secs(30));
        let hb = |m: &SessionMsg| matches!(m, SessionMsg::Heartbeat);
        assert_eq!(p.count(true, hb), 15);
        assert_eq!(p.count(false, hb), 15);
        let ack = |m: &SessionMsg| matches!(m, SessionMsg::Ack(0));
        // Every heartbeat is answered by one ACK; within a round the keeper writes last, so
        // its final heartbeat's answer is still pending when the loop ends.
        assert_eq!(p.count(false, ack), 15);
        assert_eq!(p.count(true, ack), 14);
        assert_eq!(p.count(true, is_data) + p.count(false, is_data), 0);
        assert_eq!(p.log.len(), 59, "only heartbeats and their answers crossed the wire");
    }

    #[test]
    fn heartbeat_is_answered_by_ack() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let now = p.now;
        let hb = SessionMsg::Heartbeat.encode();
        p.local.on_leg_bytes(now, &hb).unwrap();
        assert_eq!(p.local.next_deadline(now), Some(now));
        assert_eq!(one(&p.local.poll_send(now).unwrap()), SessionMsg::Ack(0));
        assert!(p.local.poll_send(now).is_none());
        assert_ne!(p.local.next_deadline(now), Some(now));
    }

    #[test]
    fn heartbeats_alone_produce_srtt_samples() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        assert_eq!(p.local.srtt(), None);
        // The local heartbeat goes out at t0 + 2 s, reaches the keeper 150 ms later, and the
        // keeper's ACK reaches the local end 150 ms after that.
        let sent = t0 + HEARTBEAT_INTERVAL;
        assert_eq!(one(&p.local.poll_send(sent).unwrap()), SessionMsg::Heartbeat);
        let at_keeper = sent + ms(150);
        p.keeper.on_leg_bytes(at_keeper, &SessionMsg::Heartbeat.encode()).unwrap();
        assert_eq!(p.local.srtt(), None);
        let ack = p.keeper.poll_send(at_keeper).unwrap();
        assert_eq!(one(&ack), SessionMsg::Ack(0));
        p.local.on_leg_bytes(at_keeper + ms(150), &ack).unwrap();
        assert_eq!(p.local.srtt(), Some(ms(300)));
        // The keeper has measured nothing yet: its own heartbeat is not out.
        assert_eq!(p.keeper.srtt(), None);
    }

    #[test]
    fn heartbeat_probe_not_armed_with_unacked_data() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        p.local.push_source(&[7; 100]).unwrap();
        // Data first (one chunk), then the heartbeat is due with the data still unacked.
        let sent = t0 + HEARTBEAT_INTERVAL;
        let msgs = drain(&mut p.local, sent);
        assert!(msgs.contains(&SessionMsg::Heartbeat) && msgs.iter().any(is_data));
        p.local
            .on_leg_bytes(sent + ms(300), &SessionMsg::Ack(100).encode())
            .unwrap();
        assert_eq!(p.local.srtt(), None, "a data ACK is not a heartbeat answer");
    }

    #[test]
    fn heartbeats_are_not_piggybacked_on_data() {
        let mut p = Pair::new(8 * MIB, MIB);
        p.connect();
        let start = p.now;
        // Continuous data for 10 s; the heartbeat still fires on its own 2 s timer.
        for _ in 0..200 {
            p.now += ms(50);
            p.local.push_source(&[1; 100]).unwrap();
            let mut b = None;
            p.round(&mut b);
        }
        assert_eq!(p.now - start, secs(10));
        let hb = p.count(true, |m| matches!(m, SessionMsg::Heartbeat));
        assert_eq!(hb, 5);
    }

    // ---- negotiation through endpoints ----

    #[test]
    fn hello_resume_flag_and_welcomed_events() {
        let mut p = Pair::new(MIB, MIB);
        let first = p.local.hello(p.now, 60_000, None);
        let SessionMsg::Hello(h) = one(&first) else { panic!() };
        assert!(!h.resume);
        assert_eq!((h.v, h.grace_ms, h.token), (1, 60_000, tok()));
        p.connect();
        p.leg_down();
        let second = p.local.hello(p.now, 60_000, Some(tok()));
        let SessionMsg::Hello(h) = one(&second) else { panic!() };
        assert!(h.resume);
        assert_eq!(h.previous_token, Some(tok()));
        assert!(p.local.established());
    }

    #[test]
    fn resume_true_never_new_and_new_on_reconnect_is_session_gone() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        p.leg_down();
        p.local.hello(p.now, 60_000, None);
        let forged = SessionMsg::Welcome(Welcome {
            v: 1,
            status: WelcomeStatus::New,
            recv_offset: 0,
        })
        .encode();
        assert_eq!(
            p.local.on_leg_bytes(p.now, &forged).unwrap(),
            vec![SessionEvent::ResumeRefused]
        );
        assert!(!p.local.is_up());
    }

    #[test]
    fn unknown_welcome_is_resume_refused() {
        let unknown = SessionMsg::Welcome(Welcome {
            v: 1,
            status: WelcomeStatus::Unknown,
            recv_offset: 0,
        })
        .encode();
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        p.leg_down();
        p.local.hello(p.now, 1, None);
        assert_eq!(
            p.local.on_leg_bytes(p.now, &unknown).unwrap(),
            vec![SessionEvent::ResumeRefused]
        );
        // The keeper side of the same exchange: unknown token, resume:true.
        let mut fresh = Pair::new(MIB, MIB);
        fresh.keeper_has_session = false;
        let mut hello = hello_msg(true);
        hello.recv_offset = 0;
        assert_eq!(
            answer_hello(&hello, false),
            HelloAnswer::Welcome(WelcomeStatus::Unknown)
        );
        let w = fresh
            .keeper
            .welcome(fresh.now, WelcomeStatus::Unknown, 0)
            .unwrap();
        assert!(matches!(one(&w), SessionMsg::Welcome(Welcome { status: WelcomeStatus::Unknown, .. })));
        assert!(!fresh.keeper.is_up());
    }

    #[test]
    fn welcome_with_wrong_version_or_out_of_range_offset_is_leg_error() {
        let mut p = Pair::new(MIB, MIB);
        p.local.hello(p.now, 1, None);
        let bad_v = SessionMsg::Welcome(Welcome {
            v: 2,
            status: WelcomeStatus::New,
            recv_offset: 0,
        })
        .encode();
        assert!(p.local.on_leg_bytes(p.now, &bad_v).is_err());

        let mut p = Pair::new(MIB, MIB);
        p.connect();
        push_chunks(&mut p.keeper, 1000);
        p.leg_down();
        // Peer claims to have received more than the keeper ever sent.
        assert!(p.keeper.welcome(p.now, WelcomeStatus::Resumed, 1001).is_err());
        assert!(p.keeper.welcome(p.now, WelcomeStatus::Resumed, 1000).is_ok());
    }

    #[test]
    fn misplaced_messages_are_leg_errors() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let hello = SessionMsg::Hello(hello_msg(false)).encode();
        assert!(p.local.on_leg_bytes(p.now, &hello).is_err(), "HELLO at local");
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let welcome = SessionMsg::Welcome(Welcome {
            v: 1,
            status: WelcomeStatus::New,
            recv_offset: 0,
        })
        .encode();
        assert!(p.keeper.on_leg_bytes(p.now, &welcome).is_err(), "WELCOME at keeper");
        // Data before WELCOME.
        let mut p = Pair::new(MIB, MIB);
        p.local.hello(p.now, 1, None);
        let data = SessionMsg::Data {
            offset: 0,
            bytes: vec![1],
        }
        .encode();
        assert!(p.local.on_leg_bytes(p.now, &data).is_err());
    }

    // ---- offsets, dedup, resume ----

    #[test]
    fn duplicate_and_overlapping_data_is_discarded() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let d = |offset, s: &str| {
            SessionMsg::Data {
                offset,
                bytes: s.as_bytes().to_vec(),
            }
            .encode()
        };
        let now = p.now;
        let ev = p.keeper.on_leg_bytes(now, &d(0, "abcd")).unwrap();
        assert_eq!(ev, vec![SessionEvent::Deliver(b"abcd".to_vec())]);
        assert!(p.keeper.on_leg_bytes(now, &d(0, "abcd")).unwrap().is_empty());
        assert!(p.keeper.on_leg_bytes(now, &d(1, "bc")).unwrap().is_empty());
        let ev = p.keeper.on_leg_bytes(now, &d(2, "cdef")).unwrap();
        assert_eq!(ev, vec![SessionEvent::Deliver(b"ef".to_vec())]);
        assert_eq!(p.keeper.recv_offset(), 6);
    }

    fn data_then_bad_message_keeps_delivered_bytes(keeper_role: bool) {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let mut buf = SessionMsg::Data {
            offset: 0,
            bytes: b"good".to_vec(),
        }
        .encode();
        // A gap DATA is a protocol error at either role.
        buf.extend(
            SessionMsg::Data {
                offset: 99,
                bytes: vec![1],
            }
            .encode(),
        );
        let ep = if keeper_role { &mut p.keeper } else { &mut p.local };
        let err = ep.on_leg_bytes(p.now, &buf).unwrap_err();
        assert_eq!(err.delivered, vec![SessionEvent::Deliver(b"good".to_vec())]);
        assert_eq!(ep.recv_offset(), 4, "recv_offset matches the returned Deliver");
    }

    #[test]
    fn valid_data_before_bad_message_is_returned_with_error_keeper() {
        data_then_bad_message_keeps_delivered_bytes(true);
    }

    #[test]
    fn valid_data_before_bad_message_is_returned_with_error_local() {
        data_then_bad_message_keeps_delivered_bytes(false);
    }

    #[test]
    fn gap_is_leg_error_and_state_is_untouched() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let gap = SessionMsg::Data {
            offset: 1,
            bytes: vec![1],
        }
        .encode();
        assert!(p.keeper.on_leg_bytes(p.now, &gap).is_err());
        assert_eq!(p.keeper.recv_offset(), 0);
    }

    #[test]
    fn ack_beyond_sent_bytes_is_leg_error() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        push_chunks(&mut p.local, 10);
        assert!(p
            .local
            .on_leg_bytes(p.now, &SessionMsg::Ack(11).encode())
            .is_err());
    }

    #[test]
    fn double_reconnect() {
        let mut p = Pair::new(MIB, MIB);
        let mut want_up = Vec::new();
        let mut want_down = Vec::new();
        for round in 0..3u8 {
            if round == 0 {
                assert_eq!(p.connect(), vec![SessionEvent::Welcomed(WelcomeStatus::New)]);
            } else {
                p.leg_down();
                assert_eq!(p.connect(), vec![SessionEvent::Welcomed(WelcomeStatus::Resumed)]);
            }
            let a = vec![round + 1; 70_000];
            let b = vec![round + 100; 5];
            p.local.push_source(&a[..MAX_DATA_CHUNK]).unwrap();
            p.local.push_source(&a[MAX_DATA_CHUNK..]).unwrap();
            p.keeper.push_source(&b).unwrap();
            want_up.extend_from_slice(&a);
            want_down.extend_from_slice(&b);
            assert!(!p.pump(None));
            assert_eq!(p.up, want_up);
            assert_eq!(p.down, want_down);
        }
    }

    #[test]
    fn end_both_directions() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        p.local.queue_final_end(end_reason::CLIENT_GONE);
        p.pump(None);
        assert_eq!(p.ends_at_keeper, vec!["client-gone".to_string()]);

        let mut p = Pair::new(MIB, MIB);
        p.connect();
        p.keeper.push_source(b"tail").unwrap();
        p.keeper.queue_final_end(end_reason::SERVER_EOF);
        p.pump(None);
        assert_eq!(p.down, b"tail");
        assert_eq!(p.ends_at_local, vec!["server-eof".to_string()]);
    }

    #[test]
    fn ack_cadence() {
        let d = |offset: u64, n: usize| {
            SessionMsg::Data {
                offset,
                bytes: vec![0; n],
            }
            .encode()
        };
        // 64 KiB delivered: ack at once.
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t = p.now + ms(10);
        p.local.on_leg_bytes(t, &d(0, MAX_DATA_CHUNK)).unwrap();
        p.local.delivered(t, MAX_DATA_CHUNK as u64);
        assert_eq!(one(&p.local.poll_send(t).unwrap()), SessionMsg::Ack(65536));
        assert!(p.local.poll_send(t).is_none());

        // 1 byte: ack only after 200 ms.
        p.local.on_leg_bytes(t, &d(65536, 1)).unwrap();
        p.local.delivered(t, 65537);
        assert!(p.local.poll_send(t).is_none());
        assert!(p.local.poll_send(t + ms(199)).is_none());
        assert_eq!(p.local.next_deadline(t), Some(t + ms(200)));
        assert_eq!(one(&p.local.poll_send(t + ms(200)).unwrap()), SessionMsg::Ack(65537));

        // Heartbeat carries a fresh ack even when the 200 ms timer is not up.
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let t0 = p.now;
        let t = t0 + ms(1950);
        p.local.on_leg_bytes(t, &d(0, 1)).unwrap();
        p.local.delivered(t, 1);
        assert!(p.local.poll_send(t).is_none());
        let at_hb = t0 + secs(2);
        assert_eq!(one(&p.local.poll_send(at_hb).unwrap()), SessionMsg::Ack(1));
        assert_eq!(one(&p.local.poll_send(at_hb).unwrap()), SessionMsg::Heartbeat);
        assert!(p.local.poll_send(at_hb).is_none());
    }

    #[test]
    fn acks_lost_with_a_leg_are_reissued_after_resume() {
        let mut p = Pair::new(MIB, MIB);
        p.connect();
        let d = SessionMsg::Data {
            offset: 0,
            bytes: vec![0; 10],
        }
        .encode();
        p.local.on_leg_bytes(p.now, &d).unwrap();
        p.local.delivered(p.now, 10);
        p.now += ms(300);
        assert_eq!(one(&p.local.poll_send(p.now).unwrap()), SessionMsg::Ack(10));
        p.leg_down(); // the ack never arrived
        p.keeper.leg_down();
        p.local.hello(p.now, 1, None);
        let w = SessionMsg::Welcome(Welcome {
            v: 1,
            status: WelcomeStatus::Resumed,
            recv_offset: 0,
        })
        .encode();
        p.local.on_leg_bytes(p.now, &w).unwrap();
        p.now += ms(250);
        assert_eq!(one(&p.local.poll_send(p.now).unwrap()), SessionMsg::Ack(10));
    }

    #[test]
    fn loss_at_every_offset_exactly_once() {
        let script_up: Vec<u8> = (0..300u32).map(|i| (i * 7 + 1) as u8).collect();
        let script_down: Vec<u8> = (0..300u32).map(|i| (i * 13 + 5) as u8).collect();

        let run = |cut: usize| -> (Pair, usize) {
            let mut p = Pair::new(MIB, MIB);
            let pieces = |s: &[u8]| s.chunks(37).map(<[u8]>::to_vec).collect::<Vec<_>>();
            let (up, down) = (pieces(&script_up), pieces(&script_down));
            p.connect();
            let before = p.wire_bytes;
            let mut budget = Some(cut);
            let mut cut_hit = false;
            for i in 0..up.len() {
                p.local.push_source(&up[i]).unwrap();
                p.keeper.push_source(&down[i]).unwrap();
                if !cut_hit {
                    p.now += ms(100);
                    cut_hit = p.round(&mut budget).1;
                }
            }
            let first_leg = p.wire_bytes - before;
            p.leg_down();
            p.connect();
            assert!(!p.pump(None));
            (p, first_leg)
        };

        let (_, total) = run(usize::MAX);
        assert!(total > 600, "total wire bytes {total}");
        for cut in 0..=total {
            let (p, _) = run(cut);
            assert_eq!(p.up, script_up, "up stream, cut at {cut}");
            assert_eq!(p.down, script_down, "down stream, cut at {cut}");
        }
    }

    #[test]
    fn constants_are_compile_time_and_pinned() {
        const _: () = assert!(WINDOW == 2 * 1024 * 1024 && MAX_DATA_CHUNK == 64 * 1024);
        assert_eq!(HEARTBEAT_INTERVAL, secs(2));
        assert_eq!(SILENCE_TIMEOUT, secs(8));
        assert_eq!(PRE_WELCOME_BOUND, secs(20));
        assert_eq!(LEG_SETUP_MAX, secs(15));
        assert_eq!(ACK_EVERY_BYTES, 64 * 1024);
        assert_eq!(ACK_EVERY, ms(200));
        assert_eq!(SESSION_PROTOCOL_VERSION, 1);
    }

    #[test]
    fn grace_is_carried_in_hello() {
        let mut p = Pair::new(MIB, MIB);
        let SessionMsg::Hello(h) = one(&p.local.hello(p.now, 12_345, None)) else {
            panic!()
        };
        assert_eq!(h.grace_ms, 12_345);
    }
}
