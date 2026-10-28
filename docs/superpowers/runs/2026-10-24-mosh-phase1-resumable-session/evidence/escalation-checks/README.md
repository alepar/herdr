# Escalation checks (2026-10-27, OpenSSH 10.2p1, Linux, sshd on loopback aliases, nftables drop/reject)

Leg 0 was reproduced with herdr's managed options (`-C -F <cfg with ServerAliveInterval 15 / CountMax 4>`,
optional `-S <ctl> ControlMaster=auto ControlPersist=600`), stderr on a real pty (`ptyrun.py`), stdin fed
every 2 s (HEARTBEAT_INTERVAL), and the client SIGKILLed 8 s into the outage (SILENCE_TIMEOUT).

## 1. Leg-0 ssh stderr over the TUI: real, but only outside herdr's default setup

| Variant | Setup | Outage | Terminal output |
|---|---|---|---|
| B | no mux | drop | none (killed at 8 s) |
| E | no mux | TCP reset | none |
| F | mux client, master pre-created (herdr default) | TCP reset | none; client exits quietly |
| A/C | leg 0 is itself the master | drop (C: restored at 40 s) | none |
| D | no mux, user config ServerAlive 1 x 2 | drop | `Timeout, server … not responding.` at +3 s, before herdr's kill |
| G | no mux, ProxyJump | drop | jump helper survives the kill; `Timeout, server <jump> not responding.` at +74 s |

A backgrounded mux master points stdin/stdout/stderr at /dev/null, and so does its ProxyJump helper, so
herdr's default (managed config, multiplexing on, bootstrap ssh creates the master first) is silent.
Text reaches the TUI only without multiplexing (`manage_ssh_config=false`, non-unix platforms) when the
user's ServerAlive total is under 8 s, or when the link runs through a ProxyJump.
Fix shape: pipe leg-0 stderr through a relay that forwards to the terminal until WELCOME and captures
afterwards; children (the jump helper) inherit the pipe, so their late messages are captured too.

## 2. `ssh_leg.rs:208`: stderr EOF after SIGKILL of a ProxyJump ssh: no hang

Link up: stderr EOF and helper exit are immediate after SIGKILL. Link dropped: the `ssh -W` helper is
reparented, holds the stderr pipe, and exits on its own after 71 s (ServerAlive 15 x 4), writing
`Timeout, server <jump> not responding.` (`pj-drop.log`). spawn_leg's deadline-bounded join therefore
keeps teardown bounded; the cost is one capture thread plus one orphaned helper for about 70 s per lost
leg, and that leg's stderr is dropped (post-WELCOME legs classify as Retryable regardless of stderr).
