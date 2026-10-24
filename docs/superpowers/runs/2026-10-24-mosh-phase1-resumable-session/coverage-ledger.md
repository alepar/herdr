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
