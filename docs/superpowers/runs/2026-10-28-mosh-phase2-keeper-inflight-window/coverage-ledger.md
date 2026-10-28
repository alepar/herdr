cov-1 · r1 · GAP · R4 · applied — mosh-hex.4 now states RTT comes only from existing DATA/ACK traffic and no session message, field or constant changes; existing acceptance keeps the constants pinned
cov-2 · r1 · GAP · R7 · applied — mosh-hex.5 closing check: diff confined to src/mosh/**, tests/mosh_*, run dir; mosh-diff-budget 12; Cargo.toml/Cargo.lock unchanged
cov-3 · r1 · GAP · R5 · applied — mosh-hex.1 retitled "delay-and-rate link knob" (description already specified rate)
cov-4 · r1 · GAP · R8 · applied — mosh-hex.5 adds resume-under-flood acceptance (window restarts at INITIAL, Ctrl-C bound holds); mosh-hex.4 already resets on set_up/leg_down
cov-5 · r1 · GAP · R11 · applied — mosh-hex.4 acceptance: ring occupancy never exceeds the window while up; keeper stops reading the server at source_room() 0
cov-6 · r1 · GAP · R1 miss path · applied — mosh-hex.5 acceptance: on a miss with converged window, attribute residual latency, record D3 pacing-needed, report failed, park follow-up
cov-7 · r1 · GAP · R10 · applied — mosh-hex.4 states RTT from existing DATA/ACK only; mosh-hex.2 acceptance already rejects sub-quantum ack samples (ack coalescing)
cov-8 · r1 · UNOWNED-SEAM · backlog metric + measurement procedure · applied — mosh-hex.3 owns the backlog metric definition (Recv-Q == FIONREAD) and the reusable measurement helper; mosh-hex.4 and mosh-hex.5 consume them
cov-9 · r1 · UNOWNED-SEAM · after-change evidence + D3 decision · applied — mosh-hex.5 owns evidence/phase2-acceptance.md and the D3 entry
cov-10 · r1 · NEEDS-SPEC · mosh-hex.4 · declined — mosh-hex.4 is a leaf, not a subepic; both questions answered by the amendments above
cov-11 · r2 · GAP · baseline ordering · applied — edge mosh-hex.4 <- mosh-hex.3 (baseline before the window lands), blocked-by line added
cov-12 · r2 · GAP · server slow-client behaviour · applied — mosh-hex.3 records server behaviour under a non-reading client with file:line, and any userland queue in the metric/attribution
cov-13 · r2 · GAP · R4 interop verification · applied — mosh-hex.5 protocol check: no SessionMsg/codec/constant diff; Phase 1 codec/wire tests pass unmodified (receiver code unchanged, so a Phase 1 receiver is the same code)
cov-14 · r2 · UNOWNED-SEAM · leg-kill facility · applied — mosh-hex.5 consumes existing Remote::signal_leg / wait_leg_count
cov-15 · r2 · UNOWNED-SEAM · INITIAL + trace reset marker · applied — trace schema gains `leg`, reset emits an event; mosh-hex.5 consumes INITIAL from mosh-hex.2
cov-16 · r2 · GAP · R1 conditional on D3 · applied — mosh-hex.3 predicts the latency budget and picks a profile that fits, else pre-fills the miss path; miss path kept as acknowledged contingency
cov-17 · r2 · GAP · backlog metric cap · applied — mosh-hex.3 records the FIONREAD saturation cap; mosh-hex.5 D3 entry states it
cov-18 · r2 · GAP · harness location · rejected — mosh-hex.1 already names tests/mosh_harness.rs, inside tests/mosh_*
cov-19 · r2 · UNOWNED-SEAM · trace capture path · applied — mosh-hex.4 owns emission through the existing HERDR_MOSH_TRACE keeper file, read via Remote::keeper_events
