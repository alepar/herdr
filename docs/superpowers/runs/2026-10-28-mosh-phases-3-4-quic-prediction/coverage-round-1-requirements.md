R1 A remote session's leg runs over QUIC when the remote advertises x-mosh-quic-v1 and UDP reaches the keeper, authenticated by a fingerprint delivered over ssh.
R2 After a client address change (relay rebind) the QUIC session resumes and shows the next keystroke's echo within 2 RTT of the link returning, with no new ssh process.
R3 An idle gap under the 30 s QUIC silence window resumes on the same connection and shows echo within 2 RTT of the link returning; a longer gap within grace (up to 10 min) redials QUIC, not ssh, within 3 RTT plus one backoff step.
R4 With UDP blocked or the remote lacking QUIC support, the session comes up over ssh within 3 s plus the ssh time, and a mid-session UDP block falls back to ssh without ending the session.
R5 Under 10 % loss at 150 ms one-way delay, typing produces visible output within 2 s and flood output keeps advancing, measured against the ssh leg.
R6 Dropping or losing render frames never breaks the client's strict render chain (no client disconnect), across QUIC legs, ssh legs and leg changes, and the client's surface recovers via a full-surface resync.
R7 With prediction on at 150 ms one-way delay, the median keypress-to-screen latency of printable keys in a shell is under 5 ms.
R8 A wrong prediction is cleared within 1 RTT + 50 ms + one render.
R9 With prediction off (default), on a fast link in adaptive mode, or against a remote without echo support, client behaviour and output are unchanged.
R10 Older peers keep working: no frozen codec, ServerMessage/ClientMessage variant or PROTOCOL_VERSION change; new behaviour only through EndpointControl kinds, optional JSON fields and advertised capabilities.
R11 Upstream edits are one-statement tagged hooks totalling at most 30 lines, enforced by scripts/mosh-diff-budget.sh.
R12 New Cargo dependencies are only quinn/rustls/rcgen with the ring provider (no aws-lc-rs); QUIC is unix-only.
R13 Tests live only in fork files; wall-clock acceptance tests are opt-in (#[ignore], --run-ignored all, timing_lock(), dense flood) and use a userspace UDP relay with delay, seeded loss and rebind.
R14 Prediction stays correct within R7/R8 over a QUIC leg with render lanes and 10 % loss (echo-ack may arrive before its surface).
R15 At least one end-to-end scenario runs all three phases together (QUIC leg, supersede under loss, prediction on).
