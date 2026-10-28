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
- Unix producer: reads the first byte with `set_read_timeout(PRE_WELCOME_BOUND)` and runs a closed match: `Q` answers the bootstrap and loops; the HELLO type byte reads HELLO.
- QUIC producer: reads HELLO through the blocking adapter and checks the token.
- Both call one `admit` with the `KeeperLeg` and the decoded `Hello`.
- `admit`, `run_leg`, `leg_writer`, `answer_and_close` and `LegSlot` use the leg's boxed halves and its shutdown closure: Unix is `shutdown(Both)` on a clone; QUIC is `Connection::close` (synchronous, safe from any thread).
- `admit` never sees `quic_token`.

**QuicLegDialer** — extended (root-defined). Per §Key decisions and §6, it differs from the root: fallback is per dial, a fingerprint mismatch on reconnect is `Retryable`, and render-split is per leg.
- Composition: a concrete `SshLegDialer`. It calls its bootstrap-mode method for leg 0 and its plain `dial` for ssh reconnects.
- State:
  - the resolved dial host;
  - the cached `QuicBootstrap` (`None` means QUIC is off for the session);
  - a "last QUIC leg died before WELCOME" flag, in a `Cell` because `classify` takes `&self`;
  - the client endpoint and runtime.
- Classification (keeper verdicts arrive as leg ends):
  - Leg 0, QUIC leg ended before WELCOME: Phase 1's leg-0 table, so `Unsupported` → plain bridge. `Retryable` would stop leg 0.
  - Reconnect, `quic-token` END or a transport close before WELCOME: `Retryable`, and QUIC is skipped on the next dial. `quic-token` also clears the cached bootstrap, which prevents an endless QUIC redial.
  - Reconnect refusals an ssh leg would also get (`protocol-version`, `server-connect-failed`, `token-in-use`): Phase 1's `classify_leg`.
  - Ended after WELCOME: `Retryable`.
- Lifecycle: the runtime and endpoint sit in an `Arc` shared by the dialer and every QUIC `Leg`. No `block_on` on a runtime that has shut down.
- Must not: write HELLO, read session bytes, or decide its own registration.

**QuicBootstrap** — extended: also owns the reply format (`{}` or `{port, fingerprint, token}`) and the 500 ms-bounded single-line read.

**quic module** — extended: the client endpoint's wildcard bind and roaming rebind. quinn drops UDP send errors internally, so an error-triggered rebind can only live in a wrapper over quinn's `AsyncUdpSocket`, in this module. With a wildcard bind, migration alone already handles route changes. It must never be called with `block_on` from runtime worker threads.

**Leg** — extended:
- an optional opaque admission token, set to the bootstrap token by QUIC legs and `None` for ssh; `LocalBridge` copies it into HELLO without interpreting it;
- the RTT probe (mosh-19g.6);
- a QUIC close reason in the end report;
- the render lane slot, `None` until 3b.

**Hello / SessionEndpoint** — extended: optional `quic_token` (`skip_serializing_if`, like `previous_token`). `SESSION_PROTOCOL_VERSION` is unchanged.

**SshLegDialer** — extended: a separate bootstrap-mode method, not a flag. It adds `--x-mosh-quic-v1` to the remote command and, after the ready marker, reads one line within 500 ms, returning the leg plus the parsed bootstrap.

**Attacher** — extended: sends `Q` and prints the line only when started with `--x-mosh-quic-v1`, so an older client never sees the line as session bytes.

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
  2. ssh runs with `--x-mosh-quic-v1`. The attacher sends `Q`; the keeper starts `QuicAcceptor`, which mints a token; the line is printed.
  3. The pinned handshake succeeds in under 3 s, ssh is torn down, and a QUIC `Leg` carrying the token is returned.
  4. The bridge copies the token into HELLO. The acceptor reads HELLO, `QuicTokens` binds it, then `admit` → `run_leg` proceed.
- Leg 0, UDP blocked: the same ssh leg is returned. The token expires unbound after 60 s, so a later QUIC reconnect gets `quic-token`, the cache is cleared, and the session goes ssh-only. This is a known limitation.
- Address change: quinn migrates, driven by the session heartbeats.
- Mid-session UDP block: the silence verdict fires, the QUIC dial fails in 3 s and falls back to ssh, and the next dial tries QUIC again.
- Future: a third transport adds one `LegDialer` impl, one `KeeperLeg` producer and one arm in `start_ui_bridge`.

Per §1: when the bind fails, the reply is `{}` and no line is printed, so the dialer waits out its 500 ms read. That is accepted as written.

Digest:
- QuicTokens: owns minting, 60 s expiry and session binding of QUIC admission tokens, sans-IO with time as input;
  new token rules go here; QuicAcceptor only acts on its accept or refuse verdict.

## Post-Implementation Notes

*As this design is implemented and iterated on — bug fixes, adjustments, anything that diverged from the assumptions above — append a dated note here, whether or not a formal debugging skill was used.*
