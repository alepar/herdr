requirements:
R1 → mosh-dqw.4, mosh-dqw.5, mosh-dqw.6, mosh-dqw.7, mosh-dqw.8, mosh-dqw.10, mosh-dqw.9
R2 → (unmapped)
R3 → mosh-dqw.4, mosh-dqw.8, mosh-dqw.7, mosh-dqw.6
R4 → mosh-dqw.4, mosh-dqw.7, mosh-dqw.8
R5 → mosh-dqw.7, mosh-dqw.8
R6 → mosh-dqw.2, mosh-dqw.8
R7 → mosh-dqw.7, mosh-dqw.8, mosh-dqw.6
R8 → mosh-dqw.7, mosh-dqw.9
R9 → mosh-dqw.7, mosh-dqw.10
R10 → (unmapped)
R11 → (unmapped)
R12 → mosh-dqw.1, mosh-dqw.6, mosh-dqw.10
R13 → mosh-dqw.6
R14 → mosh-dqw.3, mosh-dqw.1
R15 → mosh-dqw.3
R-new: the remote keeper is started on demand on the remote host and a stale/locked keeper socket is recovered → mosh-dqw.10, mosh-dqw.7
R-new: grace and stall thresholds come from a single source both ends read → mosh-dqw.1

GAP · R2 · saved-machine health hold-off not named
GAP · R10 · server EOF/handoff → ServerShutdown not named
GAP · R11 · D6 not named
GAP · R12 · Windows-remote short-circuit not named
GAP · R9 · ghost eviction not named
GAP · R3 · heartbeat/stall timer not owned
UNOWNED-SEAM · attacher↔keeper protocol over keeper socket
UNOWNED-SEAM · leg command line SshLegDialer→remote::dispatch
UNOWNED-SEAM · capability advertisement→probe
UNOWNED-SEAM · injected link-frame format
UNOWNED-SEAM · OutageState and last-contact timestamp
UNOWNED-SEAM · MoshConfig constants
UNOWNED-SEAM · Leg consumed by keeper side
GAP (ledger: C1) · mosh-dqw.8 breadth
NEEDS-SPEC: mosh-dqw.8; NEEDS-SPEC: mosh-dqw.10
