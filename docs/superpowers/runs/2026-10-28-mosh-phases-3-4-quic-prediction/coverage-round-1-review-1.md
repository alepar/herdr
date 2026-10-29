requirements:
R1 → mosh-19g.1.1, mosh-19g.1.3, mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5
R2 → mosh-19g.1.6, mosh-19g.1.3, mosh-19g.1.2, mosh-19g.4, mosh-19g.1.5
R3 → mosh-19g.1.6, mosh-19g.1.3, mosh-19g.1.1, mosh-19g.1.5
R4 → mosh-19g.1.4, mosh-19g.1.6, mosh-19g.1.7, mosh-19g.1.5
R5 → mosh-19g.2.1, mosh-19g.2.3, mosh-19g.2.4, mosh-19g.2.5, mosh-19g.2.6, mosh-19g.2.8, mosh-19g.2.9, mosh-19g.2.7, mosh-19g.4
R6 → mosh-19g.2.2, mosh-19g.2.3, mosh-19g.2.5, mosh-19g.2.9, mosh-19g.2.7
R7 → mosh-19g.3.1, mosh-19g.3.2, mosh-19g.3.4, mosh-19g.6, mosh-19g.3.5
R8 → mosh-19g.3.1, mosh-19g.3.3, mosh-19g.3.5
R9 → mosh-19g.3.1, mosh-19g.3.4, mosh-19g.3.5
R10 → mosh-19g.1.3, mosh-19g.1.7, mosh-19g.2.2, mosh-19g.3.3
R11 → mosh-19g.5
R12 → mosh-19g.1.1
R13 → mosh-19g.4, mosh-19g.1.5, mosh-19g.2.7, mosh-19g.3.5
R-new: root goal "about one RTT" vs R2/R3 2 RTT — state 2 RTT or test the tighter bound → mosh-19g.1.5

findings:
- GAP · R8 · integration check measures from ack arrival, not keypress; lost-ack path unbounded; add opt-in wall-clock keypress→cleared test to 3.5
- GAP · R9 · adaptive-on-fast-link and prediction-unset not verified end to end; add to 3.5
- GAP · R10 · no mixed-version 3b tests (supersede stripped, old Hello without render_split); extend 2.7
- GAP · R11 · Phase 4 §4 table not updated for deadline-driven echo (echo_due, echo_deadline); no merged-tree budget check
- GAP · R12 · Cargo deps/ring feature and cfg(unix) gate not named in 1.1 owns
- UNOWNED-SEAM · prediction mode read by Predictor (3.1) and bridge (3.4)
- UNOWNED-SEAM · bridge upstream FrameSplitter post-Hello injection path (2.9 → 3.4)
- UNOWNED-SEAM · echo probe flag x-mosh-echo-v1 in the bridge (3.3 ↔ 3.4)
- UNOWNED-SEAM · connection-generation numbering (3.1 keying ↔ 3.2 hooks)
- UNOWNED-SEAM · echo-ack classified as Reliable by FrameClass (2.1) vs 3.3
