# findings ledger — design — through r1
triggers: none
blocking: r0=- r1=0 carriedB=- regressedB=- degraded=none not-computable=no-progress,returned,regressed
ignored-overrides: 0
- [Should-fix] §5 Acceptance test 'Ctrl-C under flood' (`yes; echo <marker>`) · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Should-fix] §5 'No throughput regression' (LAN window grows to >= 1 MiB) with §1 grow rule and Key abstractions 'Gap settled here' · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Should-fix] §1 RTT sampling gate ('takes an RTT sample on each ACK that advances the acked offset by at least ACK_EVERY_BYTES') · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Should-fix] §1 'a bounded deque, at most WINDOW / MAX_DATA_CHUNK entries' with §3 reads 'rounded up to 4 KiB' · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Should-fix] §4 'Each chunk read at time t is released at max(t + delay, previous release + len / rate)' · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Nit] §1 'base_rtt, the minimum sample on the current leg'; reset only on leg up/down · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
- [Should-fix] §1 Constants 'FLOOR = 2 * ACK_EVERY_BYTES (128 KiB)' vs §1 Considered 'too large for a 128 KiB/s link'; §5 R rule · raised r1@- · open · source roast · text docs/superpowers/runs/2026-10-28-mosh-phase2-keeper-inflight-window/2026-10-28-mosh-phase2-keeper-inflight-window-roast-design-1.md
