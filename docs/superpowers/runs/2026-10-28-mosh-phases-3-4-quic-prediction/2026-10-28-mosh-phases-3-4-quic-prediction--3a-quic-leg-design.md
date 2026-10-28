# Phase 3a: QUIC leg with ssh bootstrap, roaming and fallback

Parent spec: `2026-10-28-mosh-phases-3-4-quic-prediction-design.md` (§1, §4, §6 3a).
Bead: mosh-19g.1
Research brief: `inputs/seam-brief.md`.

## Goal

A remote session's leg runs over QUIC when the remote supports it and UDP gets through. It survives
an address change and an idle gap under 30 s on the same connection (echo within 2 RTT of the link
returning), redials QUIC after a longer gap within grace, and it falls back to the ssh leg within 3 s when QUIC is unavailable.

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

- Leg 0 is always an ssh leg run in bootstrap mode. The attacher writes exactly one bootstrap line
  (or a `none` line) after the ready marker and before any session bytes. The client then attempts
  QUIC for up to 3 s and either switches (tearing the ssh child down once WELCOME arrives over QUIC)
  or keeps the already-working ssh leg. Any QUIC failure before WELCOME continues on that live ssh leg.
- Reconnect legs try QUIC first with the cached bootstrap (3 s bound), then ssh in the same dial.
- Render frames are split per leg (3b): a session may move between QUIC and ssh legs, and ssh legs
  carry everything on the reliable stream as in Phase 1. This deliberately overrides the root key
  abstractions' "render-split fixed per session" and "fallback is leg-0 only and sticky": a network
  that blocks UDP mid-session must not kill the session.

## Decision points, by section

### 1. Bootstrap (chosen: keeper answers a query, attacher prints)

- **Recommended:**
  - The attacher opens its own short-lived rendezvous connection for the query and sends a one-byte
    bootstrap query (`Q`, which cannot be a `SessionMsg` type byte) on it. The query never shares a
    connection with session bytes.
  - The keeper starts its `QuicAcceptor` lazily on first query. It replies one JSON line:
    `{port, fingerprint, token}`.
    - `fingerprint` is the SHA-256 of its self-signed certificate, hex.
    - `token` is 32 random bytes, hex, minted per query.
  - The attacher reads the reply with a 2 s timeout, then closes the query connection. EOF, a read
    error or the timeout all count as `none`; an older keeper that parses `Q` as a broken HELLO and
    closes the connection therefore yields `none` and a plain ssh mosh leg. A reply that arrives
    after the timeout lands on the closed query connection and never reaches the session stream.
  - Only when started with `--x-mosh-quic-v1`, the attacher writes exactly one line right after the
    ready marker and before any session bytes:
    - `herdr-mosh-quic:1 <port> <fingerprint> <token>` on a reply with fields;
    - `herdr-mosh-quic:1 none` when the reply is `{}` (UDP bind failed) or the query gave no reply.
  - With the flag, the attacher opens its session rendezvous connection lazily, on the first byte
    from stdin (the HELLO), then pumps as today. A leg-0 ssh leg held as the QUIC standby (§6)
    therefore holds no keeper connection, and no `PRE_WELCOME_BOUND` runs on it while QUIC is tried.
  - The dialer waits for that one line, bounded by a 5 s safety timeout that it treats as `none`.
    This replaces a best-effort bounded read, so a late line can never land in the session stream.
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
- Client side: once a QUIC leg has reached WELCOME with the cached token, that token is bound and
  the client keeps it for the session. A bootstrap refresh on an ssh leg (§7) then updates only port
  and fingerprint. Before the first binding, a refresh replaces the token as well. This lets a session
  that fell back to ssh mid-session return to QUIC at a later reconnect dial.

### 3. Dial address (chosen: `ssh -G` hostname, skip QUIC behind a jump)

- **Recommended:** the dialer resolves the address with `ssh -G <target>`, which already runs for
  the control path (seam brief §1): it takes `hostname` and dials `<hostname>:<port>`, in the address
  family that `hostname` resolves to.
- The keeper binds a dual-stack wildcard: `[::]` with `IPV6_V6ONLY` off, falling back to `0.0.0.0`.
- A fingerprint mismatch on a reconnect dial (say, a round-robin name reaching another host) falls
  through to an ssh leg in the same dial and is `Retryable`, never `Fatal` (§6).
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
- **Dependencies:** under a `cfg(unix)` target table, `quinn` and `rustls` with
  `default-features = false` (`quinn`: `runtime-tokio`, `rustls-ring`; `rustls`: `ring`, `std`,
  `tls12`), and `rcgen` with `ring`. `cargo tree -i aws-lc-rs` (and `aws-lc-sys`) must come back
  empty.
- **Transport parameters:**
  - `max_idle_timeout` is fixed in `TransportConfig` on both peers before the dial: the keeper
    advertises 3600 s and the client its own grace, so the negotiated value is the client's grace
    (see Key abstractions, QuicAcceptor). Grace stays a keeper application timer; the 30 s QUIC
    silence window (§7) ends a leg first;
  - no QUIC keepalive (session heartbeats every 2 s already keep NAT mappings);
  - one bidirectional stream per connection carries the session.
- **Considered:** async std-free rewrite of the bridge. Rejected: large and outside this phase.

### 5. `KeeperLeg` (chosen: refactor admit/run_leg to a transport-neutral leg)

- **Recommended:** `admit`, `run_leg`, `leg_writer` and `LegSlot` take a `KeeperLeg`: a boxed reader,
  a boxed writer, a shutdown handle callable from another thread, and an optional render lane
  (`None` until 3b).
- The existing Unix accept loop produces `KeeperLeg`s; `QuicAcceptor` produces them for QUIC. The
  Unix producer's first-byte match answers `Q` on any rendezvous connection; the attacher sends it
  only on its dedicated query connection (§1).
- Behaviour-preserving for Unix legs; all Phase 1 and 2 tests pass unmodified.

### 6. `QuicLegDialer` (chosen: wraps `SshLegDialer`)

- **Recommended:**
  - Registration: `start_ui_bridge` wraps `SshLegDialer` in `QuicLegDialer` when the status probe
    reports `x-mosh-quic-v1` and `HERDR_MOSH_QUIC` is not `0`.
  - Leg 0: dial ssh in bootstrap mode (`SshLegDialer` waits for the one bootstrap line after the
    ready marker, 5 s safety timeout treated as `none`, and exposes the parsed `QuicBootstrap`).
    With a bootstrap and no jump, attempt the QUIC handshake for up to 3 s.
    - On success: return the QUIC leg, carrying the live ssh leg as its standby. The ssh child stays
      alive until WELCOME arrives over QUIC; `LocalBridge` then drops the standby, which tears the
      ssh child down. If the QUIC leg ends before WELCOME (token refusal, pre-WELCOME bound, early
      loss), `LocalBridge` switches to the standby ssh leg, never the plain bridge, and sends a
      leg-0-style HELLO on it (resume false) with a fresh session token and `previous_token` set to
      the QUIC attempt's session token. The bridge cannot tell whether the keeper admitted the QUIC
      HELLO; Phase 1's ghost eviction (`previous_token` handling in keeper.rs) removes that session if
      it exists and is a no-op otherwise. The standby attacher opens its session connection only now
      (§1), so no keeper-side `PRE_WELCOME_BOUND` ran on it during the QUIC attempt.
    - On failure: return the ssh leg as is and log the reason.
  - Reconnect legs: try QUIC with the cached bootstrap (3 s bound; its token kept across ssh refreshes
    once bound, §2); any QUIC failure, a fingerprint
    mismatch included, falls through to the ssh reconnect leg in the same dial. QUIC failures are
    `Retryable` and never `Fatal`; a changed fingerprint means a new keeper, and the ssh leg's
    WELCOME `Unknown` then ends the session through Phase 1's rules.
  - Roaming:
    - The client socket binds the wildcard address, so a route change picks the new source address
      per packet and the keeper sees a peer address change, which quinn migrates (path validation).
    - No error-triggered `Endpoint::rebind`: quinn swallows UDP send errors, and with a wildcard bind
      migration alone handles route changes (see Key abstractions, quic module). Revisit only if the
      address-change test fails.
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
- Short idle gap: the relay blackholes for 20 s, then restores. The same QUIC leg survives (no new
  leg, no ssh process) and the echo appears within 2 RTT plus one render after restore. This needs
  the QUIC silence window below.
- Long idle gap: the relay blackholes for 60 s within a 120 s grace and calls `rebind()` during the gap,
  then restores. The bridge redials QUIC first (ssh only if that QUIC attempt itself fails); a new
  QUIC leg is admitted and the echo appears within 3 RTT plus one
  reconnect backoff step plus one render after restore, plus up to the 3 s handshake bound of an
  attempt already in flight when the link returns.
- **QUIC silence window (coverage round 1):** Phase 1's bridge silence verdict would end a QUIC leg
  during a 20 s gap. A `Leg` may carry a silence-timeout override that `LocalBridge` honours; QUIC
  legs set 30 s (ssh legs keep Phase 1's value). Gaps under it ride QUIC migration on the same
  connection; longer ones end the leg and reconnect QUIC-first.
- **Bootstrap refresh (coverage round 1):** ssh reconnect legs run in bootstrap mode too, and a fresh
  `QuicBootstrap` updates the cache, so a transient UDP failure at leg 0 does not leave the session
  ssh-only for life. Before the token is bound the refresh replaces it; once bound, the refresh
  updates only port and fingerprint and the bound token stays (§2).
- Mid-session UDP block: the relay goes to 100 % loss for longer than the silence timeout. The
  reconnect leg falls back to ssh and the session continues. The relay then restores UDP and the ssh
  leg is dropped; the next reconnect dial returns the session to QUIC (no `quic-token` END).
- Old remote: with `strip-capability` the session is ssh-only.
- Old keeper: a pre-Phase-3 keeper process still running under a new binary closes the query
  connection on `Q`; the attacher writes `herdr-mosh-quic:1 none` and the session comes up on a plain
  ssh mosh leg.
- Late bootstrap reply: a keeper whose reply is delayed past the attacher's 2 s timeout; the line is
  `none`, and the late reply never reaches the session stream (the leg reads WELCOME first).

Non-timing unit tests: bootstrap line round trip; token mint, bind and expiry; pinning verifier
accepts only the fingerprint; `KeeperLeg` refactor covered by existing suites.

### 8. Out of scope

- QUIC 0-RTT.
- Port range configuration.
- IPv6-specific handling beyond what quinn and the wildcard bind give.
- Windows.

## Key abstractions

**QuicTokens** — new sans-IO value type in the keeper's QUIC acceptor code; the token table behind §2.
- Owns:
  - minting 32-byte tokens (reusing `QuicBootstrap`'s token type);
  - the unbound-token map with mint times, and the bound QUIC-token ↔ session-token bimap;
  - the check: unknown → refuse; unbound and younger than 60 s → bind to `HELLO.token`; bound → must match its session; an already-bound session must present its own token.
- Hides: expiry and binding rules. Expiry is lazy, applied on each mint and check with time as an input, so there is no timer thread.
- Must not: do I/O, write END, or touch the keeper's session `Table`.
- Lifecycle: owned by `QuicAcceptor` behind its own mutex, for the keeper process's life. Bound entries are never removed, a small leak bounded by the keeper's 30 s idle exit.
- Errors: a refused check is a value; `QuicAcceptor` turns it into `END {reason:"quic-token"}`.

**QuicAcceptor** — extended (root-defined):
- Composes `QuicTokens` and answers the bootstrap query (the keeper's `Q` handler asks it for `{port, fingerprint, token}`). The first query starts it. A failed UDP bind replies `{}` and is not cached, so the next query retries.
- Per accepted connection: take the first bidirectional stream; on a std thread, read HELLO under the pre-WELCOME bound; check the token; hand a `KeeperLeg` and the decoded HELLO to the shared `admit`.
- That thread counts in `Shared.pending`, like `spawn_handler`, so idle exit waits for it.
- Idle timeout: quinn negotiates `max_idle_timeout` as the minimum of both peers' values during the handshake, before HELLO arrives. So the keeper advertises the grace ceiling (3600 s) and the client advertises its own grace; the negotiated value is the client's grace.

**KeeperLeg** — extended (root-defined); the HELLO read moves out of `admit` into each producer.
- Unix producer: reads the first byte with `set_read_timeout(PRE_WELCOME_BOUND)` and runs a closed match: `Q` answers the bootstrap and loops; the HELLO type byte reads HELLO. The match accepts `Q` on any rendezvous connection; the attacher sends it only on its dedicated query connection, which it closes after the reply (§1), so the loop then sees EOF and ends without admitting anything.
- QUIC producer: reads HELLO through the blocking adapter and checks the token.
- Both call one `admit` with the `KeeperLeg` and the decoded `Hello`.
- `admit`, `run_leg`, `leg_writer`, `answer_and_close` and `LegSlot` use the leg's boxed halves and its shutdown closure: Unix is `shutdown(Both)` on a clone; QUIC is `Connection::close` (synchronous, safe from any thread).
- `admit` never sees `quic_token`.

**QuicLegDialer** — extended (root-defined). Per §Key decisions and §6, it differs from the root: fallback is per dial, a fingerprint mismatch on reconnect is `Retryable`, and render-split is per leg.
- Composition: a concrete `SshLegDialer`. It calls its bootstrap-mode method for leg 0 and its plain `dial` for ssh reconnects.
- State:
  - the resolved dial host;
  - the cached `QuicBootstrap` (`None` means QUIC is off for the session) and whether its token is bound (a QUIC leg reached WELCOME with it); a refresh keeps a bound token and updates only port and fingerprint (§2);
  - a "last QUIC leg died before WELCOME" flag, in a `Cell` because `classify` takes `&self`;
  - the client endpoint and runtime.
- Classification (keeper verdicts arrive as leg ends):
  - Leg 0, QUIC leg ended before WELCOME (token refusal, pre-WELCOME bound, early loss): not a leg-0 failure. `LocalBridge` switches to the leg's live ssh standby (§6), never the plain bridge, with a leg-0-style HELLO carrying a fresh session token and `previous_token` = the QUIC attempt's token, so ghost eviction removes a QUIC session the keeper may have admitted; Phase 1's leg-0 table applies only if that ssh leg also ends before WELCOME.
  - Reconnect, `quic-token` END or a transport close before WELCOME: `Retryable`, and QUIC is skipped on the next dial. `quic-token` also clears the cached bootstrap, which prevents an endless QUIC redial.
  - Reconnect refusals an ssh leg would also get (`protocol-version`, `server-connect-failed`, `token-in-use`): Phase 1's `classify_leg`.
  - Ended after WELCOME: `Retryable`.
- Lifecycle: the runtime and endpoint sit in an `Arc` shared by the dialer and every QUIC `Leg`. No `block_on` on a runtime that has shut down.
- Must not: write HELLO, read session bytes, or decide its own registration.

**QuicBootstrap** — extended: also owns the reply format (`{}` or `{port, fingerprint, token}`), the `herdr-mosh-quic:1 none` line, and the single-line read, which waits for exactly one line (5 s safety timeout, treated as `none`).

**quic module** — extended: the client endpoint's wildcard bind and roaming rebind. quinn drops UDP send errors internally, so an error-triggered rebind can only live in a wrapper over quinn's `AsyncUdpSocket`, in this module. With a wildcard bind, migration alone already handles route changes. It must never be called with `block_on` from runtime worker threads.

**Leg** — extended:
- an optional opaque admission token, set to the bootstrap token by QUIC legs and `None` for ssh; `LocalBridge` copies it into HELLO without interpreting it;
- the RTT probe (mosh-19g.6);
- a QUIC close reason in the end report;
- the render lane slot, `None` until 3b;
- on a leg-0 QUIC leg, the live ssh leg as an optional standby: `LocalBridge` drops it on WELCOME (tearing the ssh child down) and switches to it if the leg ends before WELCOME, without learning the transport. On the switch it sends a leg-0-style HELLO with a fresh session token and `previous_token` = the QUIC attempt's session token (§6). The standby holds no keeper connection until that HELLO (§1).

**Hello / SessionEndpoint** — extended: optional `quic_token` (`skip_serializing_if`, like `previous_token`). `SESSION_PROTOCOL_VERSION` is unchanged.

**SshLegDialer** — extended: a separate bootstrap-mode method, not a flag. It adds `--x-mosh-quic-v1` to the remote command and, after the ready marker, waits for the one bootstrap line (5 s safety timeout, treated as `none`), returning the leg plus the parsed bootstrap.

**Attacher** — extended: sends `Q` and writes a line only when started with `--x-mosh-quic-v1`, so an older client never sees the line as session bytes. With the flag:
- it sends `Q` on its own short-lived rendezvous connection, reads the reply with a 2 s timeout and closes that connection; EOF, an error or the timeout mean `none`, so an older keeper that closes on `Q` is just `none`;
- it writes exactly one line before any session bytes: the bootstrap, or `herdr-mosh-quic:1 none` on a bind failure or no reply;
- it opens the session rendezvous connection lazily, on the first byte from stdin (the HELLO), so a late reply never reaches the session stream and a standby ssh leg holds no keeper connection.

**start_ui_bridge** — extended, the one place the kind is chosen. It wraps in `QuicLegDialer` only when:
- the probe reports `x-mosh-quic-v1`;
- `HERDR_MOSH_QUIC` is not `0`;
- `ssh -G <target>` shows no `proxyjump` or `proxycommand`.
The `-G` parse is a small function next to the existing `-G` use (attach.rs:886), which only runs on the managed control path.

**status.rs** — one more tagged capability line.

**Variation:**
- Leg transport → the `LegDialer` trait, chosen only in `start_ui_bridge`.
- Keeper leg sources → `KeeperLeg` producers feeding one `admit`.
- The rendezvous first byte → one closed match in the Unix producer.
- Token rules → only `QuicTokens`.

**Walk-through:**
- Leg 0, success:
  1. `start_ui_bridge` registers `QuicLegDialer(SshLegDialer)`.
  2. ssh runs with `--x-mosh-quic-v1`. The attacher sends `Q` on a dedicated query connection; the keeper starts `QuicAcceptor`, which mints a token; the attacher closes the query connection and prints the line. It opens no session connection yet.
  3. The pinned handshake succeeds in under 3 s, and a QUIC `Leg` carrying the token and the live ssh leg as its standby is returned.
  4. The bridge copies the token into HELLO. The acceptor reads HELLO, `QuicTokens` binds it, then `admit` → `run_leg` proceed. WELCOME arrives over QUIC and the bridge drops the standby, tearing ssh down.
- Leg 0, QUIC ends before WELCOME (token refused, pre-WELCOME bound, early loss): the bridge switches to the standby ssh leg and sends a HELLO with a fresh session token and `previous_token` = the QUIC attempt's token; the attacher opens its session connection on that HELLO, and the keeper evicts any admitted QUIC session.
- Leg 0, UDP blocked: the same ssh leg is returned. The token expires unbound after 60 s; the next ssh reconnect leg runs in bootstrap mode and refreshes the cached bootstrap, token included because it was never bound (Coverage round 1), so QUIC is retried then. A session whose ssh leg never drops stays on ssh.
- Leg 0, old keeper: it closes the query connection on `Q`; the line is `none` and the session runs on a plain ssh mosh leg.
- Address change: quinn migrates, driven by the session heartbeats.
- Mid-session UDP block: the silence verdict fires, the QUIC dial fails in 3 s and falls back to ssh, whose refresh keeps the bound token; the next dial tries QUIC again with it.
- Future: a third transport adds one `LegDialer` impl, one `KeeperLeg` producer and one arm in `start_ui_bridge`.

Per §1 (design roast r1): when the bind fails, the reply is `{}` and the attacher writes `herdr-mosh-quic:1 none`, so the dialer never waits out a timeout on a healthy remote; the 5 s safety timeout only covers a stuck attacher.

Digest:
- QuicTokens: owns minting, 60 s expiry and session binding of QUIC admission tokens, sans-IO with time as input;
  new token rules go here; QuicAcceptor only acts on its accept or refuse verdict.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*

### 2026-10-28: acceptance tests over the UDP relay (mosh-19g.1.5)

`tests/mosh_quic.rs` runs the QUIC leg end to end through `UdpRelay` at 150 ms one-way (RTT 300 ms)
on a loopback host, with the bootstrap port rewritten by the fake ssh. Measured values:

- Short idle gap (20 s blackhole, same QUIC leg): restore to first echo 0.91 to 0.94 s over four
  runs, against the 2 RTT bound of 0.6 s. The excess over 2 RTT is 0.31 to 0.34 s. This is the
  parked escalation (QUIC PTO backoff after 20 s of lost heartbeats): it is not a miss against
  "2 RTT + one render" while a render is allowed 0.5 s, but it is about three RTTs rather than
  two. The test asserts 2 RTT + 0.5 s and logs the measured time.
- Address change (`rebind`): first echo 0.51 to 0.53 s, same connection, no new leg.
- Long idle gap (60 s blackhole, `rebind` during the gap, ssh unreachable throughout): restore to
  first echo 6.2 s with a new QUIC leg admitted; the bound is 3 RTT + 8 s backoff + 3 s + render.
- Fallback at 100 % loss: session up in 3.26 s against 0.30 s over plain ssh (+2.96 s, bound +3.5 s).

Two defects found by these tests and fixed with them:

- `LocalBridge` injected the QUIC leg's SRTT as an `x-mosh.link.v1` frame before the server's first
  frame, so the client handshake failed with "expected endpoint welcome". The SRTT report now waits
  for the first server frame.
- `QuicLegDialer.token_bound` was only set by `classify`, which the bridge calls for legs that end
  before WELCOME. After a QUIC leg that welcomed and then died, the ssh fallback refresh replaced
  the bound token, and the next QUIC redial was refused with `quic-token` (the "returns to QUIC with
  the bound token" case). The dialer now treats a QUIC leg the keeper answered on as bound.
