//! The keeper's QUIC acceptor: admission tokens and the lazily started UDP endpoint.
//!
//! Owns: `QuicTokens` (mint, 60 s unbound expiry, binding to a session), the dual-stack UDP
//! bind, the acceptor runtime and the accept task that hands each connection's first bidi
//! stream to the keeper as a `QuicConn`.
//! Hides: quinn and rustls. Must not: read HELLO, admit legs or touch the session table;
//! the keeper's admission thread does that with `QuicTokens::check` as its only gate.
//! Lifecycle: started by the first bootstrap query and kept until the keeper exits; a failed
//! start is not cached, so the next query retries. Bound tokens are never removed: they stay
//! valid for their session's whole life, so QUIC redials within grace need no ssh round trip.

use std::collections::HashMap;
use std::io;
use std::net::UdpSocket;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::runtime::Handle;

use crate::remote::attach::mosh::quic::{
    self, QuicRuntime, ServerIdentity, KEEPER_IDLE_TIMEOUT,
};
use crate::remote::attach::mosh::quic_bootstrap::{Fingerprint, QuicBootstrap, QuicToken};
use crate::remote::attach::mosh::session::{Token, PRE_WELCOME_BOUND};

/// An unbound token older than this is refused.
const UNBOUND_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TokenVerdict {
    Admit,
    Refuse,
}

/// Admission tokens, sans-IO: time is an input.
#[derive(Default)]
pub(super) struct QuicTokens {
    unbound: HashMap<QuicToken, Instant>,
    bound: HashMap<QuicToken, Token>,
    by_session: HashMap<Token, QuicToken>,
}

impl QuicTokens {
    pub fn mint(&mut self, now: Instant) -> io::Result<QuicToken> {
        self.expire(now);
        let token = QuicToken::generate()?;
        self.unbound.insert(token, now);
        Ok(token)
    }

    /// `presented` is `HELLO.quic_token`, `session` is `HELLO.token`. An unbound token binds
    /// to the first session that presents it; a bound token admits only that session, and
    /// that session admits only that token.
    pub fn check(&mut self, presented: Option<&str>, session: Token, now: Instant) -> TokenVerdict {
        self.expire(now);
        let Some(token) = presented.and_then(|text| QuicToken::from_str(text).ok()) else {
            return TokenVerdict::Refuse;
        };
        if let Some(owner) = self.bound.get(&token) {
            return if *owner == session {
                TokenVerdict::Admit
            } else {
                TokenVerdict::Refuse
            };
        }
        if !self.unbound.contains_key(&token) || self.by_session.contains_key(&session) {
            return TokenVerdict::Refuse;
        }
        self.unbound.remove(&token);
        self.bound.insert(token, session);
        self.by_session.insert(session, token);
        TokenVerdict::Admit
    }

    fn expire(&mut self, now: Instant) {
        self.unbound
            .retain(|_, minted| now.saturating_duration_since(*minted) < UNBOUND_TTL);
    }

    #[cfg(test)]
    fn insert_unbound(&mut self, token: QuicToken, at: Instant) {
        self.unbound.insert(token, at);
    }
}

/// One accepted QUIC connection with its first bidi stream, before HELLO.
pub(super) struct QuicConn {
    pub handle: Handle,
    pub tokens: Arc<Mutex<QuicTokens>>,
    pub conn: quinn::Connection,
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
}

/// Called on the acceptor's runtime for each connection; it must not block.
pub(super) type OnConn = Arc<dyn Fn(QuicConn) + Send + Sync>;

struct Running {
    /// Keeps the runtime (and so the accept task and the endpoint) alive.
    _rt: Arc<QuicRuntime>,
    port: u16,
    fingerprint: Fingerprint,
    tokens: Arc<Mutex<QuicTokens>>,
}

#[derive(Default)]
pub(super) struct QuicAcceptor {
    state: Mutex<Option<Arc<Running>>>,
}

impl QuicAcceptor {
    /// The bootstrap reply for one query: starts the endpoint on first use and mints a fresh
    /// token. `None` when the endpoint cannot start (logged, not cached).
    pub fn query(&self, on_conn: OnConn, now: Instant) -> Option<QuicBootstrap> {
        let running = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.is_none() {
                match start(on_conn) {
                    Ok(running) => *state = Some(Arc::new(running)),
                    Err(err) => {
                        tracing::warn!(%err, "keeper QUIC acceptor failed to start");
                        return None;
                    }
                }
            }
            Arc::clone(state.as_ref()?)
        };
        let token = running
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mint(now)
            .map_err(|err| tracing::warn!(%err, "keeper QUIC token mint failed"))
            .ok()?;
        Some(QuicBootstrap {
            port: running.port,
            fingerprint: running.fingerprint,
            token,
        })
    }
}

fn start(on_conn: OnConn) -> io::Result<Running> {
    let socket = bind_dual_stack().or_else(|_| UdpSocket::bind("0.0.0.0:0"))?;
    socket.set_nonblocking(true)?;
    let port = socket.local_addr()?.port();
    let identity = ServerIdentity::generate()?;
    let config = quic::server_config(&identity, KEEPER_IDLE_TIMEOUT)?;
    let rt = QuicRuntime::new("mosh-quic-acceptor")?;
    let endpoint = quic::server_endpoint(&rt, config, socket)?;
    let tokens = Arc::new(Mutex::new(QuicTokens::default()));
    let handle = rt.handle();
    let accept_tokens = Arc::clone(&tokens);
    handle.spawn({
        let handle = handle.clone();
        async move {
            while let Some(incoming) = endpoint.accept().await {
                // One task per connection: a peer stalling its handshake blocks nobody else.
                let handle = handle.clone();
                let tokens = Arc::clone(&accept_tokens);
                let on_conn = Arc::clone(&on_conn);
                tokio::spawn(async move {
                    let Ok(conn) = incoming.await else { return };
                    let opened = tokio::time::timeout(PRE_WELCOME_BOUND, conn.accept_bi()).await;
                    let Ok(Ok((send, recv))) = opened else {
                        conn.close(0u32.into(), b"no stream");
                        return;
                    };
                    on_conn(QuicConn {
                        handle,
                        tokens,
                        conn,
                        send,
                        recv,
                    });
                });
            }
        }
    });
    Ok(Running {
        _rt: rt,
        port,
        fingerprint: identity.fingerprint,
        tokens,
    })
}

/// `[::]:0` with IPV6_V6ONLY off, so one socket serves IPv4 and IPv6 clients.
fn bind_dual_stack() -> io::Result<UdpSocket> {
    // SAFETY: socket(2) takes no pointers; the result is checked before use.
    let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh descriptor nothing else owns; `OwnedFd` closes it on any error below.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let off: libc::c_int = 0;
    // SAFETY: the pointer and length describe `off`, which outlives the call.
    let rc = unsafe {
        libc::setsockopt(
            owned.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            std::ptr::from_ref(&off).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: an all-zero sockaddr_in6 is `[::]` port 0; only the family needs setting.
    let mut addr: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
    addr.sin6_family = libc::AF_INET6 as libc::sa_family_t;
    // SAFETY: the pointer and length describe `addr`, which outlives the call.
    let rc = unsafe {
        libc::bind(
            owned.as_raw_fd(),
            std::ptr::from_ref(&addr).cast(),
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UdpSocket::from(owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(n: u8) -> Token {
        format!("{n:032x}").parse().unwrap()
    }

    #[test]
    fn unknown_token_refused() {
        let mut tokens = QuicTokens::default();
        let stranger = QuicToken([7; 32]).to_string();
        assert_eq!(
            tokens.check(Some(&stranger), session(1), Instant::now()),
            TokenVerdict::Refuse
        );
    }

    #[test]
    fn unbound_young_token_binds() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        let token = tokens.mint(now).unwrap();
        let text = token.to_string();
        let later = now + Duration::from_secs(59);
        assert_eq!(tokens.check(Some(&text), session(1), later), TokenVerdict::Admit);
        // Bound now: the unbound table no longer holds it.
        assert!(tokens.unbound.is_empty());
        assert_eq!(tokens.bound.get(&token), Some(&session(1)));
    }

    #[test]
    fn unbound_token_expires_after_60s() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        let token = QuicToken([1; 32]);
        tokens.insert_unbound(token, now);
        let text = token.to_string();
        assert_eq!(
            tokens.check(Some(&text), session(1), now + Duration::from_secs(60)),
            TokenVerdict::Refuse
        );
        // Expiry is permanent: a clock that does not move backwards cannot revive it.
        assert!(tokens.unbound.is_empty());
    }

    #[test]
    fn bound_token_with_other_session_refused() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        let text = tokens.mint(now).unwrap().to_string();
        assert_eq!(tokens.check(Some(&text), session(1), now), TokenVerdict::Admit);
        assert_eq!(tokens.check(Some(&text), session(2), now), TokenVerdict::Refuse);
    }

    #[test]
    fn bound_session_with_other_token_refused() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        let first = tokens.mint(now).unwrap().to_string();
        let second = tokens.mint(now).unwrap().to_string();
        assert_eq!(tokens.check(Some(&first), session(1), now), TokenVerdict::Admit);
        assert_eq!(tokens.check(Some(&second), session(1), now), TokenVerdict::Refuse);
        // The refused token stays unbound and usable by another session.
        assert_eq!(tokens.check(Some(&second), session(2), now), TokenVerdict::Admit);
    }

    #[test]
    fn bound_token_admits_again() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        let text = tokens.mint(now).unwrap().to_string();
        assert_eq!(tokens.check(Some(&text), session(1), now), TokenVerdict::Admit);
        // Far past the unbound TTL: a redial within grace still admits.
        let hours = now + Duration::from_secs(3600);
        assert_eq!(tokens.check(Some(&text), session(1), hours), TokenVerdict::Admit);
    }

    #[test]
    fn absent_or_garbage_token_refused() {
        let mut tokens = QuicTokens::default();
        let now = Instant::now();
        tokens.mint(now).unwrap();
        assert_eq!(tokens.check(None, session(1), now), TokenVerdict::Refuse);
        assert_eq!(tokens.check(Some("not hex"), session(1), now), TokenVerdict::Refuse);
        assert_eq!(tokens.check(Some(""), session(1), now), TokenVerdict::Refuse);
    }
}
