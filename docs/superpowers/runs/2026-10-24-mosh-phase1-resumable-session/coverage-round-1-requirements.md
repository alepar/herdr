R1 herdr --remote sessions survive an ssh leg drop (process kill, mid-frame) and resume within the 60 s grace
R2 saved-machine connections survive an ssh leg drop and resume without health timeout or supervisor reconnect
R3 a stalled leg (no traffic >8 s) is detected dead and replaced by a fresh leg
R4 no client input is lost or duplicated across an outage (exactly-once, in order)
R5 after resume the client receives no reconnect snapshot or full surface (server never sees a new connection)
R6 outage banner shows last-contact time while resuming and clears on resume
R7 grace expiry / unrecoverable session falls back to today's disconnect behaviour without hangs
R8 multiple simultaneous clients each resume independently
R9 client exit ends the remote session promptly; full reconnect evicts the ghost session
R10 live handoff / server EOF during a session delivers ServerShutdown, not ConnectionLost
R11 late execution of client-timed-out requests is exactly-once and pinned by a test (D6)
R12 capability-less remote, kill switch, or Windows remote behaves byte-identically to upstream
R13 reconnect legs bypass the ControlMaster and run BatchMode; fatal ssh failures stop retrying
R14 upstream diff limited to the touch list (~11 tagged hook lines), no Cargo/wire changes, diff-budget script passes
R15 fork builds can override update manifest URLs so auto-update does not replace a fork build
R16 the remote keeper is started on demand on first attach, reachable by later attaches, and a stale keeper socket is recovered
R17 an idle but healthy link is not declared dead (heartbeats keep quiet legs alive)
R18 grace and stall thresholds come from a single configuration source both ends read
