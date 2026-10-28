//! QUIC rendezvous value types: the `herdr-mosh-quic:1` bootstrap line the attacher prints
//! over ssh, the keeper's JSON reply, and the fingerprint and admission-token types.
//!
//! Owns: `QuicBootstrap`, `Fingerprint`, `QuicToken` and their wire formats. Does not own:
//! any I/O beyond reading `/dev/urandom` for a token.
//!
//! Self-contained on purpose (only `std` and `serde`): `tests/mosh_harness.rs` includes this
//! file by path to reuse the parser.
// Unused in non-test builds until mosh-19g.1.3, 1.4 and 1.6 consume it; they remove this.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

pub(super) const LINE_PREFIX: &str = "herdr-mosh-quic:1";

fn write_hex(bytes: &[u8; 32], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(f, "{byte:02x}")?;
    }
    Ok(())
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        *slot = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

/// Error for a string that is not exactly 64 hex digits.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct HexError;

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected exactly 64 hex digits")
    }
}

impl std::error::Error for HexError {}

/// SHA-256 of the keeper's self-signed certificate; lowercase hex (64 chars) on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Fingerprint(pub [u8; 32]);

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for Fingerprint {
    type Err = HexError;

    fn from_str(s: &str) -> Result<Self, HexError> {
        parse_hex32(s).map(Fingerprint).ok_or(HexError)
    }
}

/// 32-byte admission token, minted per bootstrap query; lowercase hex (64 chars).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct QuicToken(pub [u8; 32]);

impl QuicToken {
    pub fn generate() -> io::Result<QuicToken> {
        let mut buf = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
        Ok(QuicToken(buf))
    }
}

impl fmt::Display for QuicToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, f)
    }
}

impl FromStr for QuicToken {
    type Err = HexError;

    fn from_str(s: &str) -> Result<Self, HexError> {
        parse_hex32(s).map(QuicToken).ok_or(HexError)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct QuicBootstrap {
    pub port: u16,
    pub fingerprint: Fingerprint,
    pub token: QuicToken,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplyWire {
    port: u16,
    fingerprint: String,
    token: String,
}

impl QuicBootstrap {
    /// `herdr-mosh-quic:1 <port> <fingerprint> <token>` or `herdr-mosh-quic:1 none` (no newline).
    pub fn encode_line(bootstrap: Option<&QuicBootstrap>) -> String {
        match bootstrap {
            Some(b) => format!("{LINE_PREFIX} {} {} {}", b.port, b.fingerprint, b.token),
            None => format!("{LINE_PREFIX} none"),
        }
    }

    /// Trailing `\r`/`\n` trimmed; exactly four space-separated fields; port 1..=65535.
    /// The `none` line and every malformed line are `None`.
    pub fn parse_line(line: &str) -> Option<QuicBootstrap> {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut fields = line.split(' ');
        if fields.next()? != LINE_PREFIX {
            return None;
        }
        let port = parse_port(fields.next()?)?;
        let fingerprint = fields.next()?.parse().ok()?;
        let token = fields.next()?.parse().ok()?;
        if fields.next().is_some() {
            return None;
        }
        Some(QuicBootstrap {
            port,
            fingerprint,
            token,
        })
    }

    /// The keeper's rendezvous reply: `{}` or `{"port":..,"fingerprint":"..","token":".."}`.
    pub fn encode_reply(bootstrap: Option<&QuicBootstrap>) -> String {
        let Some(b) = bootstrap else {
            return "{}".to_string();
        };
        let wire = ReplyWire {
            port: b.port,
            fingerprint: b.fingerprint.to_string(),
            token: b.token.to_string(),
        };
        serde_json::to_string(&wire).unwrap_or_else(|_| "{}".to_string())
    }

    /// `{}` and anything malformed are `None`.
    pub fn parse_reply(json: &str) -> Option<QuicBootstrap> {
        let wire: ReplyWire = serde_json::from_str(json).ok()?;
        if wire.port == 0 {
            return None;
        }
        Some(QuicBootstrap {
            port: wire.port,
            fingerprint: wire.fingerprint.parse().ok()?,
            token: wire.token.parse().ok()?,
        })
    }
}

/// Decimal digits only (no sign, no whitespace), 1..=65535.
fn parse_port(s: &str) -> Option<u16> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u16>().ok().filter(|&p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> QuicBootstrap {
        let mut fp = [0u8; 32];
        let mut tok = [0u8; 32];
        for i in 0..32 {
            fp[i] = i as u8;
            tok[i] = 255 - i as u8;
        }
        QuicBootstrap {
            port: 40123,
            fingerprint: Fingerprint(fp),
            token: QuicToken(tok),
        }
    }

    #[test]
    fn bootstrap_line_round_trips() {
        let b = sample();
        let line = QuicBootstrap::encode_line(Some(&b));
        assert_eq!(
            line,
            "herdr-mosh-quic:1 40123 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f \
             fffefdfcfbfaf9f8f7f6f5f4f3f2f1f0efeeedecebeae9e8e7e6e5e4e3e2e1e0"
        );
        assert_eq!(QuicBootstrap::parse_line(&line), Some(b));
        assert_eq!(QuicBootstrap::parse_line(&format!("{line}\r\n")), Some(b));
    }

    #[test]
    fn none_line_parses_as_none() {
        let line = QuicBootstrap::encode_line(None);
        assert_eq!(line, "herdr-mosh-quic:1 none");
        assert_eq!(QuicBootstrap::parse_line(&line), None);
    }

    #[test]
    fn reply_round_trips() {
        let b = sample();
        let json = QuicBootstrap::encode_reply(Some(&b));
        assert_eq!(QuicBootstrap::parse_reply(&json), Some(b));
    }

    #[test]
    fn empty_reply_is_none() {
        assert_eq!(QuicBootstrap::encode_reply(None), "{}");
        assert_eq!(QuicBootstrap::parse_reply("{}"), None);
    }

    #[test]
    fn malformed_lines_are_none() {
        let fp = "00".repeat(32);
        let tok = "11".repeat(32);
        let cases = [
            format!("herdr-mosh-quic:2 5 {fp} {tok}"),
            format!("herdr-mosh-quic:1 0 {fp} {tok}"),
            format!("herdr-mosh-quic:1 65536 {fp} {tok}"),
            format!("herdr-mosh-quic:1 x {fp} {tok}"),
            format!("herdr-mosh-quic:1 +5 {fp} {tok}"),
            format!("herdr-mosh-quic:1 5 {} {tok}", &fp[..63]),
            format!("herdr-mosh-quic:1 5 {fp} {}", &tok[..63]),
            format!("herdr-mosh-quic:1 5 {} {tok}", "zz".repeat(32)),
            format!("herdr-mosh-quic:1 5 {fp} {} ", "gg".repeat(32)),
            format!("herdr-mosh-quic:1 5 {fp} {tok} extra"),
            format!("herdr-mosh-quic:1 5 {fp}"),
            format!("herdr-mosh-quic:1 5 {fp}  {tok}"),
            String::new(),
        ];
        for case in &cases {
            assert_eq!(QuicBootstrap::parse_line(case), None, "{case:?}");
        }
        // Control: the unmangled line parses, so each case above fails for its own defect.
        assert!(QuicBootstrap::parse_line(&format!("herdr-mosh-quic:1 5 {fp} {tok}")).is_some());
    }

    #[test]
    fn malformed_reply_is_none() {
        let fp = "00".repeat(32);
        let tok = "11".repeat(32);
        let cases = [
            r#"{"port":1}"#.to_string(),
            format!(r#"{{"port":1,"fingerprint":"{}","token":"{tok}"}}"#, "zz".repeat(32)),
            format!(r#"{{"port":0,"fingerprint":"{fp}","token":"{tok}"}}"#),
            format!(r#"{{"port":1,"fingerprint":"{fp}","token":"{tok}","extra":1}}"#),
            "not json".to_string(),
            String::new(),
        ];
        for case in &cases {
            assert_eq!(QuicBootstrap::parse_reply(case), None, "{case:?}");
        }
        assert!(QuicBootstrap::parse_reply(&format!(
            r#"{{"port":1,"fingerprint":"{fp}","token":"{tok}"}}"#
        ))
        .is_some());
    }

    #[test]
    fn token_generate_is_random_and_round_trips() {
        let a = QuicToken::generate().expect("token");
        let b = QuicToken::generate().expect("token");
        assert_ne!(a, b);
        assert_eq!(a.to_string().parse::<QuicToken>(), Ok(a));
        assert_eq!(a.to_string().len(), 64);
    }
}
