R1 Under an output flood at 300 ms RTT, Ctrl-C in a remote pane shows the interrupted command's result on the local screen within 1.5 s
R2 While a leg is up, the sender's in-flight limit adapts to the link (delay-based window near one bandwidth-delay product) instead of a fixed 2 MiB, which stays only as the cap
R3 Fast or LAN links lose no throughput: under a flood with no added delay the window grows to at least 1 MiB
R4 The session protocol and herdr wire are unchanged, so a Phase 1 peer still interoperates
R5 A delay-and-rate link emulation runs inside the fork's test harness with no root or tc
R6 The fixed-window baseline and the after-change latency and server-socket backlog are measured and recorded, and D3 (server pacing only on evidence) is decided from that record without building pacing
R7 All changes stay in fork files (src/mosh, tests/mosh_*), with the upstream hook budget unchanged and no new dependencies
R8 The window is reset per leg, so a drop and resume neither stalls nor bursts the old window onto a new path
R9 Baseline and after-change measurements use the same flood workload, Ctrl-C latency probe and backlog metric
R10 RTT samples come only from signals the Phase 1 protocol already carries (send times against existing acks)
R11 Source reads back-pressure on window room, so output queued before Ctrl-C stays bounded on the remote side
