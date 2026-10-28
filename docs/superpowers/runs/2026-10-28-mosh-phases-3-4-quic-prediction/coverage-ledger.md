# coverage ledger — mosh-19g
C1 · r1 · GAP · R8 · applied — 3.5 opt-in wall-clock keypress→clear test
C2 · r1 · GAP · R9 · applied — 3.5 adaptive-fast-link case and prediction-unset suites
C3 · r1 · GAP · R10 · applied — 2.7 mixed-version 3b cases
C4 · r1 · GAP · R11 · applied — Phase 4 §4 table recounted (≈25/30); merged-tree budget check goes into the integration sweep
C5 · r1 · GAP · R12 · applied — 1.1 owns cfg(unix) Cargo deps (ring only) and gating; 1.7 registration cfg(unix)
C6 · r1 · GAP · R3 · applied — QUIC silence window 30 s (Leg override in 1.4, set by 1.6); 3a §7, root goal and §6 restated; 1.5 short/long idle cases incl. rebind across the gap
C7 · r1 · GAP · R4 · rejected — 3 s is the QUIC attempt bound implemented in 1.6; the 3.5 s test bound is 0.5 s measurement slack
C8 · r1 · GAP · R14 · applied — 3.5 opt-in prediction over QUIC with supersede and loss; deps 1.5, 19g.4, 2.7
C9 · r1 · GAP · R15 · applied — end-to-end three-phase scenario assigned to the root integration sweep
C10 · r1 · GAP · transient leg-0 UDP failure · applied — 1.6 ssh reconnect legs refresh the QuicBootstrap
C11 · r1 · UNOWNED-SEAM · prediction mode · applied — owned by 3.1, consumed by 3.4
C12 · r1 · UNOWNED-SEAM · bridge client-frame injection API · applied — owned by 2.9, consumed by 3.4
C13 · r1 · UNOWNED-SEAM · x-mosh-echo-v1 constant · applied — defined in EchoAck (3.1)
C14 · r1 · UNOWNED-SEAM · connection generation numbering · applied — 3.1 owns the keying identity and verification
C15 · r1 · UNOWNED-SEAM · echo-ack frame class · applied — 2.1 classifies non-render tag-20 kinds Reliable, with a test
C16 · r1 · UNOWNED-SEAM · harness bootstrap-port rewrite · applied — 1.5 exposes the helper; deps 2.7←1.5, 3.5←1.5
C17 · r1 · UNOWNED-SEAM · surface_revision across resync · applied — 2.2 owns monotonicity, with a test
C18 · r1 · UNOWNED-SEAM · Leg admission token into Hello.quic_token · applied — named in 1.4 owns
C19 · r1 · GAP · root goal latency wording · applied — root goal restated (2 RTT on the same connection, 3 RTT + backoff on redial, 150 ms one-way)
C20 · r2 · GAP · R15 · applied — Integration sweep mosh-19g.7 created, owns the three-phase scenario
C21 · r2 · GAP · R11 · applied — mosh-19g.7 runs mosh-diff-budget.sh on the merged tree
C22 · r2 · GAP · R3 token validity · applied — 1.3 states bound tokens stay valid for the session; 1.5 long-gap asserts reuse
C23 · r2 · UNOWNED-SEAM · keeper-side QUIC silence window · applied — QUIC_SILENCE_WINDOW in 1.1; KeeperLeg override in 1.2; set by 1.3; 1.5 asserts keeper did not enter grace
C24 · r2 · GAP · R4 mid-session UDP block test · rejected — already in 1.5 acceptance ("Mid-session UDP block longer than the silence timeout → reconnect falls back to ssh")
C25 · r2 · GAP · R6 QUIC→ssh→QUIC · applied — 2.7 leg-change case extended to redial back to QUIC
C26 · r2 · GAP · R10 3a/Phase 4 old peers · rejected — covered: 1.3 attacher without --x-mosh-quic-v1 prints no line (old client), 1.5 strip-capability (old remote), 3.5 old remote no subscribe; an old keeper never receives quic_token because the bridge only dials QUIC on x-mosh-quic-v1
C27 · r2 · UNOWNED-SEAM · x-mosh-supersede-v1 constant · applied — defined in 2.1
C28 · r2 · UNOWNED-SEAM · x-mosh-quic-v1 constant · applied — defined in 1.1
C29 · r2 · UNOWNED-SEAM · render-stream header · applied — named in 1.1 owns
C30 · r2 · UNOWNED-SEAM · QUIC Leg RTT probe · applied — named in 1.6 owns, direct dep on mosh-19g.6
C31 · r2 · GAP · 2.3 no dependents · applied — 2.7 ← 2.3
C32 · r2 · UNOWNED-SEAM · keeper render-lane slot · applied — 1.2 owns the slot; 2.8 ← 1.2
C33 · r2 · GAP · R16 exactly-once input · applied — 1.5 exactly-once case across fallback and redial
C34 · r2 · GAP · C14 generation increment owner · rejected — the generation is the upstream client registry's existing counter, not a fork component; 3.1 verifies the numbering
G1 · graph · GRAPH-EDGE · mosh-19g.3 <- mosh-19g.5 · applied — narrow to existing leaf edges (3.2, 3.3 <- 5)
G2 · graph · GRAPH-EDGE · mosh-19g.3 <- mosh-19g.6 · applied — drop; leaf edges 3.1/3.2/3.4 <- 6 exist
G3 · graph · GRAPH-EDGE · mosh-19g.2 <- mosh-19g.4 · applied — drop; 2.7 <- 4 exists
G4 · graph · GRAPH-EDGE · mosh-19g.1 <- mosh-19g.4 · applied — drop; 1.5 <- 4 exists
G5 · graph · GRAPH-EDGE · mosh-19g.2 <- mosh-19g.1 · parked — narrow to existing leaf edges (depth 14→10); safe no: shared keeper.rs/session.rs/mod.rs/status.rs edits
G6 · graph · GRAPH-EDGE · mosh-19g.1 <- mosh-19g.5 · parked — narrow to 1.7 <- 5 (with G5, depth →9); safe no: shared mod.rs lines
G7 · graph · GRAPH-EDGE · mosh-19g.2 <- mosh-19g.6 · parked — narrow to 2.5/2.9 <- 6; safe no: shared session.rs
G8 · graph · GRAPH-EDGE · mosh-19g.2 <- mosh-19g.5 · parked — narrow to 2.2 <- 5; safe no: shared mod.rs
G9 · graph · PROPOSAL · mosh-19g.1.3 / mosh-19g.1.4 · parked — seam contract for Hello.quic_token (depth 9→8 after G5/G6)
G10 · graph · GRAPH-EDGE · 14 leaf edges on the critical path · kept — each carries a real artifact
