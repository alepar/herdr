requirements:
R1 → mosh-hex.4, mosh-hex.5
R2 → mosh-hex.2, mosh-hex.4
R3 → mosh-hex.2, mosh-hex.5
R4 → (unmapped)
R5 → mosh-hex.1
R6 → mosh-hex.3, mosh-hex.5
R7 → (unmapped)
R-new: The window takes RTT samples only from signals the Phase 1 protocol already carries → mosh-hex.4
R-new: Window state is handled correctly across leg drop and resume → (unmapped)
R-new: PTY/source reads back-pressure on window room so pre-Ctrl-C queued bytes stay bounded → mosh-hex.4

findings:
- GAP · R4 · no task verifies Phase 1 interop / wire unchanged → fix: interop clause or leaf
- GAP · R7 · no task checks fork-only diff, hook budget, Cargo files → fix: acceptance in mosh-hex.5
- GAP · R5 rate half · mosh-hex.1 title says only delay-line → fix: knob covers delay and rate
- GAP · leg drop/resume window behaviour → fix: explicit reset in mosh-hex.4 with test
- GAP · R1 miss path if residual latency is server/ssh backlog → fix: mosh-hex.5 defines outcome for an R1 miss, D3 pacing-needed follow-up
- GAP · RTT sample source under unchanged protocol / ack coalescing → fix: explicit deliverable in mosh-hex.4 with coalescing test
- UNOWNED-SEAM · server-backlog probe + flood/Ctrl-C procedure shared by mosh-hex.3 and mosh-hex.5 → fix: mosh-hex.3 owns the measurement harness
- UNOWNED-SEAM · after-change evidence record + D3 decision location written by mosh-hex.5 → fix: mosh-hex.5 owns it
NEEDS-SPEC: mosh-hex.4 — whether spec §2/§3 cover RTT sample source and leg-resume reset
