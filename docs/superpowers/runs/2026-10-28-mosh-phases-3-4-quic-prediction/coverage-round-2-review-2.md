requirements:
R1 → mosh-19g.1.1, mosh-19g.1.3, mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5
R2 → mosh-19g.1.6, mosh-19g.1.3, mosh-19g.1.5, mosh-19g.4
R3 → mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.3, mosh-19g.1.5
R4 → mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.4, mosh-19g.1.5
R5 → mosh-19g.2.3, mosh-19g.2.4, mosh-19g.2.5, mosh-19g.2.6, mosh-19g.2.8, mosh-19g.2.9, mosh-19g.2.7, mosh-19g.4
R6 → mosh-19g.2.1, mosh-19g.2.2, mosh-19g.2.3, mosh-19g.2.9, mosh-19g.2.7
R7 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.3.5, mosh-19g.6
R8 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.3, mosh-19g.3.5
R9 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.3.5, mosh-19g.6
R10 → mosh-19g.1.3, mosh-19g.1.7, mosh-19g.2.2, mosh-19g.2.8, mosh-19g.3.3, mosh-19g.2.7
R11 → mosh-19g.5
R12 → mosh-19g.1.1, mosh-19g.1.7
R13 → mosh-19g.4, mosh-19g.1.5, mosh-19g.2.7, mosh-19g.3.5
R14 → mosh-19g.3.1, mosh-19g.3.3, mosh-19g.3.5
R15 → (unmapped)
R-new: QUIC redial within grace reuses still-valid fingerprint and admission token, no ssh round trip → mosh-19g.1.3, mosh-19g.1.6
R-new: keeper-side QUIC idle/leg-silence matches the client's 30 s window → (unmapped)

findings:
- GAP · R15 · integration sweep bead does not exist yet (ledger C9)
- GAP · R11 · merged-tree budget check rests on the non-existent sweep (ledger C4)
- GAP · R3 · admission token validity/reuse across QUIC redials within grace unstated
- UNOWNED-SEAM · keeper-side QUIC idle and leg-silence vs client 30 s window
- GAP · R4 · mid-session UDP block → ssh fallback not tested
- GAP · R6 · QUIC→ssh→QUIC leg change chain integrity not tested
- GAP · R9/R10 · Phase 4 remote-without-echo and 3a mixed-version cases unnamed
- UNOWNED-SEAM · x-mosh-supersede-v1 constant (2.2 vs 2.9)
- UNOWNED-SEAM · x-mosh-quic-v1 constant / attacher flag (1.3 vs 1.7)
- UNOWNED-SEAM · render-stream header (2.4 writer, 2.6 reader; 1.1 owns line silent)
- UNOWNED-SEAM · QUIC Leg RTT probe source (19g.6 vs 1.6)
