[Nit] src/mosh/local_bridge.rs:1096 · dissolved@r2
[Nit] src/mosh/local_bridge.rs:1065 · dissolved@r2
[Nit] src/mosh/local_bridge.rs:1084; src/mosh/local_bridge.rs:1086 · dissolved@r2
[Nit] src/mosh/remote/keeper.rs:568 · dissolved@r2
[Nit] src/mosh/local_bridge.rs:1116 · dissolved@r2
[Must-fix] src/mosh/remote/keeper.rs run_leg — PeerEnd remove_session can drop bytes still queued on to_server (client's final Detach) · dissolved@r2
[Must-fix] src/mosh/remote/keeper.rs run_timers — fully acked eof session parked for grace instead of removed · dissolved@r2
[Must-fix] src/mosh/ssh_leg.rs spawn_leg teardown / local_bridge.rs pre_welcome_failed — unbounded child wait and stderr-thread join · dissolved@r2
[Should-fix] src/mosh/mod.rs:153 · disputed@r2 (resolved by 4561acb6 (mosh-dqw.21); round-2 roast reports it resolved; verified at src/mosh/mod.rs:185-195 (`idle_opt_in` shared by the plain command and the resume leg command))
[Should-fix] src/mosh/session.rs:507 · disputed@r2 (resolved by 18112920 (mosh-dqw.20); round-2 roast reports it resolved; verified at src/mosh/session.rs:81-86 and :526 (`LegBytesError.delivered` returns the decoded Deliver events with the error))
[Should-fix] src/mosh/local_bridge.rs:986 · disputed@r2 (resolved by ca3f68ea (mosh-dqw.19); round-2 roast reports it resolved; verified at src/mosh/session.rs:506 and :713-729 (`final_end` retained in the endpoint, `final_end_sent` re-armed on leg down, re-sent on the next leg))
[Should-fix] src/mosh/remote/keeper.rs:1202; src/mosh/remote/keeper.rs:1195 · disputed@r2 (resolved by fa8e7b2c (mosh-dqw.22); round-2 roast reports it resolved; verified at src/mosh/remote/keeper.rs:1223-1238 (test keeps pumping the local end while waiting for the `session_end` trace))
[Should-fix] src/mosh/remote/attacher.rs:430 · disputed@r2 (resolved by fa8e7b2c (mosh-dqw.22); round-2 roast reports it resolved; verified at src/mosh/remote/attacher.rs:228-241 (fixed short `/tmp/att-<pid>-<n>` base, asserts `rdv.dir` stays under it))
[Should-fix] src/mosh/session.rs:1581 · disputed@r2 (resolved by 18112920 (mosh-dqw.20); round-2 roast reports it resolved; verified at src/mosh/session.rs:1700-1708 (`valid_data_before_bad_message_is_returned_with_error_{keeper,local}`))
[Should-fix] src/mosh/ssh_leg.rs:649 · disputed@r2 (resolved by 4561acb6 (mosh-dqw.21); round-2 roast reports it resolved; verified at src/mosh/ssh_leg.rs:669-690 (`resume_command_idle_flag_matches_plain_command` asserts the flag for noninteractive && advertised))
[Should-fix] src/mosh/local_bridge.rs:2036 · disputed@r2 (resolved by fa8e7b2c (mosh-dqw.22); round-2 roast reports it resolved; verified at src/mosh/local_bridge.rs:2127-2135 (fake keeper reads during the WELCOME delay; test asserts `pre_welcome_msgs() == 0` after `wait_until` on hellos))
[Nit] tests/mosh_resume.rs:118 · disputed@r2 (resolved by fa8e7b2c (mosh-dqw.22); round-2 roast reports it resolved; verified at tests/mosh_resume.rs:128-132 (lower bound loosened to 8 s minus heartbeat interval minus slack))
