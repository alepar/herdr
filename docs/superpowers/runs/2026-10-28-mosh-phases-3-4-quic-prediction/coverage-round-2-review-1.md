requirements:
R1 → mosh-19g.1.1, mosh-19g.1.3, mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5
R2 → mosh-19g.1.3, mosh-19g.1.6, mosh-19g.1.5, mosh-19g.4
R3 → mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.3, mosh-19g.1.5, mosh-19g.4
R4 → mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.4, mosh-19g.1.5
R5 → mosh-19g.2.3, mosh-19g.2.4, mosh-19g.2.5, mosh-19g.2.6, mosh-19g.2.8, mosh-19g.2.9, mosh-19g.2.7, mosh-19g.4
R6 → mosh-19g.2.1, mosh-19g.2.2, mosh-19g.2.3, mosh-19g.2.5, mosh-19g.2.9, mosh-19g.2.7
R7 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.5
R8 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.3, mosh-19g.6, mosh-19g.3.5
R9 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.6, mosh-19g.3.5
R10 → mosh-19g.1.3, mosh-19g.2.2, mosh-19g.2.8, mosh-19g.3.3, mosh-19g.3.4, mosh-19g.2.7
R11 → mosh-19g.5
R12 → mosh-19g.1.1, mosh-19g.1.7
R13 → mosh-19g.4, mosh-19g.1.5, mosh-19g.2.7, mosh-19g.3.5
R14 → mosh-19g.3.1, mosh-19g.2.1, mosh-19g.3.5
R15 → (unmapped)
R-new: No keystroke is lost or duplicated across any leg change → mosh-19g.1.2, mosh-19g.1.6, mosh-19g.1.5

findings:
- GAP · R15 · sweep bead not in tree (C9)
- GAP · R11 · merged-tree budget check rests on the sweep (C4)
- GAP · 2.3 has no dependents (thin end-to-end path)
- UNOWNED-SEAM · keeper KeeperLeg render-lane slot; 2.8 lacks dep on 1.2
- UNOWNED-SEAM · render-stream header format
- UNOWNED-SEAM · QUIC idle timeout / keeper-side silence window (C6)
- UNOWNED-SEAM · RTT probe for QUIC legs
- GAP · R10 for 3a and Phase 4 old-client cases
- GAP · R3 token validity across a long-gap QUIC redial
- GAP · R-new exactly-once input across leg changes
- (low confidence) C14 generation increment owner
