C1 · r1 · flag-sweep · mosh-dqw.8 · kept — PROMOTE overruled; uncertainties resolved in spec 4be22e31
C2 · r1 · flag-sweep · mosh-dqw.3 · kept — PROMOTE overruled; split would fall below decomposition floor
C3 · r1 · flag-sweep · mosh-dqw.2 · kept — PROMOTE overruled; label/removal resolved in spec 4be22e31
C4 · r1 · GAP · R2 · applied — scoped in mosh-dqw.8 body (injection keeps saved-machine health alive) + .9 test; summary lines added so it is visible
C5 · r1 · GAP · R3 · applied — heartbeat/silence verdict owned by mosh-dqw.4, acted on by .7/.8; summary lines added
C6 · r1 · GAP · R9 · applied — ghost eviction scoped in mosh-dqw.7 (previous_token) and .8 (last_token); summary lines added
C7 · r1 · GAP · R10 · applied — server EOF/END server-eof scoped in mosh-dqw.7 and .8; .9 handoff test; summary lines added
C8 · r1 · GAP · R11 · applied — D6 late-execution test scoped in mosh-dqw.9 acceptance; summary line added
C9 · r1 · GAP · R12 · applied — Windows/kill-switch/no-capability plain gate in mosh-dqw.8 (+ unit test), fallback e2e in .9; summary lines added
C10 · r1 · UNOWNED-SEAM · keeper rendezvous protocol · applied — owned by mosh-dqw.1 (socket path + live/spawn locks), consumed by .7/.10; also fixed a spec self-deadlock (single lock held by keeper vs attacher)
C11 · r1 · UNOWNED-SEAM · leg remote argv flags · applied — flag constants owned by mosh-dqw.1; consumes line added to .6/.10
C12 · r1 · UNOWNED-SEAM · capability string · applied — STATUS_CAPABILITY owned by mosh-dqw.1; consumes added to .6/.10
C13 · r1 · UNOWNED-SEAM · link-frame injection format · applied — LINK_KIND+LinkStatus owned by mosh-dqw.2 (client-visible); .5 owns encoding and consumes them; edge .5→.2
C14 · r1 · UNOWNED-SEAM · MoshConfig constants · applied — consumes added to .4/.6/.7/.8/.10
C15 · r1 · UNOWNED-SEAM · OutageState · rejected — internal to mosh-dqw.2's observe/paint; no other task exchanges it
C16 · r1 · UNOWNED-SEAM · Leg on keeper side · rejected — Leg is local-only; keeper leg handler takes a plain duplex stream (spec §Key abstractions Keeper)
C17 · r1 · GAP · mosh-dqw.8 breadth · rejected — ledger C1; R2/R3/R10 are inside .8's spec sections (verified in bead body); NEEDS-SPEC not honored (applies to subepics; .8/.10 are leaves)
C18 · r2 · GAP · R18 · applied — spec §Keeper 'Configuration across hosts': grace via HELLO.grace_ms, thresholds compile-time; .4/.7 acceptance
C19 · r2 · GAP · R11 · applied — D6 delivery mechanism named in mosh-dqw.8 acceptance (byte exactly-once + server continuity; no request-level code); .9 notes it
C20 · r2 · GAP · R17 · applied — idle-link unit test in mosh-dqw.4, e2e idle case in .9
C21 · r2 · GAP · R16 · applied — keeper spawn/reuse/stale/concurrent e2e cases in mosh-dqw.9
C22 · r2 · GAP · keeper lifetime · applied — R19 appended; idle exit already in mosh-dqw.7, e2e case in .9; version skew via build id in socket name (mosh-dqw.1)
C23 · r2 · GAP · R2 · applied — mosh-dqw.1 names connect_saved_ssh (saved-machine supervisor path) explicitly
C24 · r2 · UNOWNED-SEAM · upstream touch list · rejected — tag convention is an epic Global Constraint; the script audits tags + budget outside fork paths, not a list
C25 · r2 · UNOWNED-SEAM · keeper entry point · applied — mosh-dqw.1 owns keeper::run/attacher::run stub signatures; .7/.10 fill and consume
C26 · r2 · UNOWNED-SEAM · attacher-keeper preamble · applied — pure-pipe rule in spec and mosh-dqw.1 rendezvous ownership
C27 · r2 · UNOWNED-SEAM · capability advertisement format · applied — format = upstream endpoint_capabilities entry (mosh-dqw.1); consumes added to .6
C28 · r2 · UNOWNED-SEAM · link-frame injection encoding · applied — consumes added to mosh-dqw.8
C29 · r2 · UNOWNED-SEAM · mosh module tree for .2/.3 · rejected — .2's outage.rs is mounted from client/mod.rs, .3 has no src/mosh code; no exchange with .1's tree
C30 · r2 · UNOWNED-SEAM · MoshConfig test override · applied — HERDR_MOSH_GRACE=5 (clamp min) in spec; .9 consumes MoshConfig
C31 · r2 · ORPHAN · mosh-dqw.3 · applied — root goal gains the self-update outcome it serves (non-interactive: reviewer's add-goal-element fix)
