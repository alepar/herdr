# Phase 3a: QUIC leg with ssh bootstrap, roaming and fallback

Parent spec: `2026-10-28-mosh-phases-3-4-quic-prediction-design.md` (§1, §4, §6 3a). Bead: mosh-19g.1.
Research brief: `inputs/seam-brief.md`.

## Goal

A remote session's leg runs over QUIC when the remote supports it and UDP gets through. It survives
an address change and an idle gap within the session grace, resuming within about one RTT of the
link returning, and it falls back to the ssh leg within 3 s when QUIC is unavailable.

## Problem description

The Phase 1 bridge dials every leg with ssh over TCP, so every resume pays a TCP + ssh handshake,
and a network switch always breaks the leg. QUIC carries a connection across address changes and
reconnects in one RTT. The session layer (HELLO/WELCOME/DATA/ACK, offsets, replay, the adaptive
window) is transport-neutral and moves to a QUIC stream unchanged.

## Main challenges

- Bootstrapping trust and an address over the authenticated ssh channel, given that the keeper has
  no stdio.
- Bridging quinn's async API into the fork's std-thread code.
- Keeping `admit`/`run_leg` transport-blind.
- Choosing the address to dial when the ssh target is an alias or sits behind a ProxyJump.
- Falling back without a second ssh handshake.
- Testing UDP impairment without root.

## Key decisions made

- Leg 0 is always an ssh leg run in bootstrap mode. The attacher prints a QUIC bootstrap line after
  the ready marker. The client then attempts QUIC for up to 3 s and either switches (tearing the ssh
  child down) or keeps the already-working ssh leg.
- Reconnect legs try QUIC first with the cached bootstrap (3 s bound), then ssh.
- Render frames are split per leg (3b): a session may move between QUIC and ssh legs, and ssh legs
  carry everything on the reliable stream as in Phase 1. This deliberately overrides the root key
  abstractions' "render-split fixed per session" and "fallback is leg-0 only and sticky": a network
  that blocks UDP mid-session must not kill the session.

## Decision points, by section

### 1. Bootstrap (chosen: keeper answers a query, attacher prints)

- **Recommended:**
  - The attacher, after starting its keeper connection, sends a one-byte bootstrap query (`Q`, which
    cannot be a `SessionMsg` type byte) on the rendezvous connection.
  - The keeper starts its `QuicAcceptor` lazily on first query. It replies one JSON line:
    `{port, fingerprint, token}`.
    - `fingerprint` is the SHA-256 of its self-signed certificate, hex.
    - `token` is 32 random bytes, hex, minted per query.
  - The attacher prints `herdr-mosh-quic:1 <port> <fingerprint> <token>` right after the ready
    marker, then pumps as today. The attacher then sends HELLO on the same connection; the first
    byte disambiguates.
  - If UDP bind fails, the reply is `{}` and no line is printed.
- **Considered:** the keeper printing the line itself. Impossible, since the keeper's stdio is null
  (seam brief §1).

### 2. Token and binding (chosen: per-query token, bound at first QUIC HELLO)

- **Recommended:** HELLO gains an optional JSON field `quic_token`. Session protocol v1 readers
  ignore unknown fields, and `SESSION_PROTOCOL_VERSION` is unchanged.
- `QuicAcceptor` rules:
  - It accepts a QUIC HELLO only if `quic_token` is a token it minted.
  - At the first QUIC HELLO, the token is bound to that HELLO's session token. A token minted but
    unbound for 60 s expires.
  - Later QUIC HELLOs for that session must carry the same token.
  - A leg with a bad or absent token gets `END {reason:"quic-token"}` and is closed.

### 3. Dial address (chosen: `ssh -G` hostname, skip QUIC behind a jump)

- **Recommended:** the dialer resolves the address with `ssh -G <target>`, which already runs for
  the control path (seam brief §1): it takes `hostname` and dials `<hostname>:<port>`.
- If `ssh -G` reports a `proxyjump` or `proxycommand`, QUIC is skipped for the session: a direct UDP
  path is unlikely, and failing fast beats a 3 s penalty on every reconnect.
- **Considered:** the server address from `SSH_CONNECTION`. Rejected as the primary source because,
  behind NAT or a jump, it names an address the client cannot reach.

### 4. QUIC plumbing (chosen: one tokio runtime thread per process side)

- **Recommended:** the `quic` module owns:
  - a multi-thread tokio runtime handle (the client's `QuicLegDialer` and the keeper's `QuicAcceptor`
    each own one runtime, started lazily);
  - a blocking `Read`/`Write` adapter over a quinn `SendStream`/`RecvStream`, which calls
    `Handle::block_on` from the leg's own std threads;
  - TLS configs:
    - keeper: an `rcgen` self-signed certificate, made once per keeper process;
    - client: a rustls verifier that accepts exactly the pinned SHA-256 fingerprint and nothing else;
    - ring crypto provider only;
  - the render-stream header type used by 3b (u64 render sequence plus a class byte).
- **Transport parameters:**
  - max idle timeout = the session grace from HELLO (keeper side; client side set from its own
    grace);
  - no QUIC keepalive (session heartbeats every 2 s already keep NAT mappings);
  - one bidirectional stream per connection carries the session.
- **Considered:** async std-free rewrite of the bridge. Rejected: large and outside this phase.

### 5. `KeeperLeg` (chosen: refactor admit/run_leg to a transport-neutral leg)

- **Recommended:** `admit`, `run_leg`, `leg_writer` and `LegSlot` take a `KeeperLeg`: a boxed reader,
  a boxed writer, a shutdown handle callable from another thread, and an optional render lane
  (`None` until 3b).
- The existing Unix accept loop produces `KeeperLeg`s; `QuicAcceptor` produces them for QUIC.
- Behaviour-preserving for Unix legs; all Phase 1 and 2 tests pass unmodified.

### 6. `QuicLegDialer` (chosen: wraps `SshLegDialer`)

- **Recommended:**
  - Registration: `start_ui_bridge` wraps `SshLegDialer` in `QuicLegDialer` when the status probe
    reports `x-mosh-quic-v1` and `HERDR_MOSH_QUIC` is not `0`.
  - Leg 0: dial ssh in bootstrap mode (`SshLegDialer` reads one extra line after the ready marker,
    500 ms bound, and exposes the parsed `QuicBootstrap`). With a bootstrap and no jump, attempt the
    QUIC handshake for up to 3 s.
    - On success: tear the ssh child down and return the QUIC leg.
    - On failure: return the ssh leg as is and log the reason.
  - Reconnect legs: try QUIC with the cached bootstrap (3 s bound), then the ssh reconnect leg.
    QUIC failures are `Retryable` and never `Fatal`; a changed fingerprint means a new keeper, and
    the ssh leg's WELCOME `Unknown` then ends the session through Phase 1's rules.
  - Roaming:
    - The client socket binds the wildcard address, so a route change picks the new source address
      per packet and the keeper sees a peer address change, which quinn migrates (path validation).
    - The dialer additionally calls `Endpoint::rebind` with a fresh socket when a send fails with a
      network error (the old interface went away).
  - RTT probe: the QUIC `Leg` fills the optional RTT probe from mosh-19g.6 with the connection's own
    RTT estimate.
- **Capability:** the keeper side's support is advertised as `x-mosh-quic-v1` in
  `status client --json`, through one tagged hook line in `src/cli/status.rs` next to the existing
  capability (`#[cfg(unix)]` is already on its own line, so this is one more line).

### 7. Acceptance and tests (chosen)

Opt-in timing tests in `tests/mosh_quic.rs`. Each is `#[ignore]`, run with `--run-ignored all`,
takes `timing_lock()`, and uses `UdpRelay` (mosh-19g.4) with the fake-ssh bootstrap-port rewrite this
subepic owns.

- QUIC is used: after leg 0, the session runs with no live ssh child, and keystrokes and output
  work.
- Fallback: with the relay at 100 % loss, the session comes up on ssh within the ssh time plus 3.5 s.
- Address change: `relay.rebind()` mid-session; the next keystroke's echo appears within 2 RTT plus
  one render after the rebind (150 ms one-way delay profile).
- Idle gap: the relay blackholes for 20 s within a 60 s grace, then restores. The echo appears within
  2 RTT plus one render after restore, with no new ssh process spawned. The 10-minute case from the
  vision is the same code path, with grace ≥ 10 min (the idle timeout equals the grace).
- Mid-session UDP block: the relay goes to 100 % loss for longer than the silence timeout. The
  reconnect leg falls back to ssh and the session continues.
- Old remote: with `strip-capability` the session is ssh-only.

Non-timing unit tests: bootstrap line round trip; token mint, bind and expiry; pinning verifier
accepts only the fingerprint; `KeeperLeg` refactor covered by existing suites.

### 8. Out of scope

- QUIC 0-RTT.
- Port range configuration.
- IPv6-specific handling beyond what quinn and the wildcard bind give.
- Windows.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
