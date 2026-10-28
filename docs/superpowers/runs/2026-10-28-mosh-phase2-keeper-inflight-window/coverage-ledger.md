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
