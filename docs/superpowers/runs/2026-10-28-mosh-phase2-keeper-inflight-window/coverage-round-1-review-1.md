requirements:
R1 → mosh-hex.5, mosh-hex.2, mosh-hex.4
R2 → mosh-hex.2, mosh-hex.4
R3 → mosh-hex.2, mosh-hex.5
R4 → (unmapped)
R5 → mosh-hex.1
R6 → mosh-hex.3, mosh-hex.5
R7 → (unmapped)
R-new: The window behaves correctly across leg drop and resume (reset/re-seed base_rtt and window on a new leg; no stall and no 2 MiB burst after reconnect) → (unmapped)
R-new: Baseline and after-change measurements use the same flood workload, Ctrl-C latency probe and backlog metric → mosh-hex.3, mosh-hex.5

findings:
- GAP · R4 · no task verifies Phase 1 peer interop / unchanged wire; premortem: RTT field added → fix: interop clause in mosh-hex.4/.5, RTT from existing acks
- GAP · R7 · no task checks fork-only diff, hook budget, Cargo files → fix: closing-check clause in mosh-hex.5
- GAP · leg drop/resume window behaviour · no task line states reset on leg change; no resume-under-flood acceptance → fix: explicit in mosh-hex.4, resume case in mosh-hex.5
- UNOWNED-SEAM · server-backlog metric shared by mosh-hex.3 (baseline) and mosh-hex.5 (trace from mosh-hex.4) · different instruments → fix: one owner defines the metric
- UNOWNED-SEAM · flood workload + Ctrl-C latency probe shared by mosh-hex.3 and mosh-hex.5 · nothing owns the procedure → fix: mosh-hex.3 owns a reusable helper consumed by mosh-hex.5
- GAP · pre-window queue bound · R1 needs keeper source reads gated so backlog stays in the server socket → fix: state in mosh-hex.4; record keeper queued bytes in mosh-hex.5
NEEDS-SPEC: mosh-hex.4 — whether size source reads bounds the keeper's unsent buffer and whether leg-resume handling is in scope
