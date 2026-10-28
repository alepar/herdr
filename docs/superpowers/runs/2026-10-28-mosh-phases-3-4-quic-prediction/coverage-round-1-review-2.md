requirements:
R1 → mosh-19g.1.1, mosh-19g.1.2, mosh-19g.1.3, mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5
R2 → mosh-19g.1.1, mosh-19g.1.6, mosh-19g.1.5, mosh-19g.4
R3 → mosh-19g.1.1, mosh-19g.1.3, mosh-19g.1.6, mosh-19g.1.5, mosh-19g.4
R4 → mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5, mosh-19g.2.9
R5 → mosh-19g.2.1, mosh-19g.2.3, mosh-19g.2.4, mosh-19g.2.5, mosh-19g.2.6, mosh-19g.2.8, mosh-19g.2.9, mosh-19g.2.7, mosh-19g.4
R6 → mosh-19g.2.1, mosh-19g.2.2, mosh-19g.2.3, mosh-19g.2.5, mosh-19g.2.9, mosh-19g.2.7
R7 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.6, mosh-19g.3.5
R8 → mosh-19g.3.1, mosh-19g.3.3, mosh-19g.3.5
R9 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.3.5
R10 → mosh-19g.1.3, mosh-19g.2.8, mosh-19g.2.2, mosh-19g.3.3, mosh-19g.3.4, mosh-19g.1.7
R11 → mosh-19g.5
R12 → mosh-19g.1.1
R13 → mosh-19g.4, mosh-19g.1.5, mosh-19g.2.7, mosh-19g.3.5
R-new: Prediction stays correct within R7/R8 over a QUIC leg with render lanes and loss → (unmapped)
R-new: At least one end-to-end scenario runs all three phases together → (unmapped)

findings:
- GAP · R2/R3 idle gap · Phase 1 silence verdict kills the QUIC leg during a 20 s blackhole; reconnect ≥ 3 RTT
- GAP · R3 10 min · idle gap not combined with rebind; long idle untested
- GAP · R4 · test bound 3.5 s vs requirement 3 s
- GAP · R11 · Phase 4 §4 table stale after deadline-driven echo; no re-tally
- GAP · R8 · test anchored at ack, not keypress
- GAP · R-new prediction over QUIC lanes · no test
- GAP · R10 · no 3b old-peer cases
- UNOWNED-SEAM · harness bootstrap-port rewrite (1.5) used by 2.7 without dep
- UNOWNED-SEAM · x-mosh-echo-v1 constant (3.3 vs 3.4)
- UNOWNED-SEAM · surface_revision semantics across resync (2.1/2.2/2.3 vs 3.1/3.3)
- UNOWNED-SEAM · echo-ack on control lane vs FrameClass/ChainGate
- UNOWNED-SEAM · FrameSplitter client-frame injection API (2.9 → 3.4)
- UNOWNED-SEAM · LocalBridge copies Leg admission token into Hello.quic_token
- GAP · transient leg-0 UDP failure leaves the session ssh-only forever
NEEDS-SPEC (re-run, not honored again): mosh-19g.2 — resync revision, control lane, 2.7 old-peer cases
