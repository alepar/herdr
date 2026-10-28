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
