# Final review — cluster 8 (mosh-tests), epic mosh-dqw

Source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`). The review source was bound to the ledger with `bind-review-source` (exit 0). Caller checkout observed at head 2bd5303c, `git status --short` empty before and after the review.

Scope: tests/mosh_fallback.rs, tests/mosh_harness.rs, tests/mosh_integration.rs, tests/mosh_multi_client.rs, tests/mosh_resume.rs, tests/mosh_status.rs (slices 000049–000054; all are new files). Checked against the design doc's "Testing" and "Acceptance criteria" sections (docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/2026-10-24-mosh-phase1-resumable-session-design.md at head) and the epic's global constraints.

## Evidence gathered

- Built and ran all six targets in a detached worktree I created at head, with its own CARGO_TARGET_DIR and a short private TMPDIR: `cargo test --locked --test mosh_resume --test mosh_fallback --test mosh_multi_client --test mosh_integration --test mosh_status`. Results: fallback 19/19 (33 s), integration 8/8 (9 s), multi_client 15/15 (68 s), resume 18/18 (96 s), status 1/1. No failures. Afterwards no process had the test TMPDIR in its environment, and the TMPDIR was empty.
- The task 10 report raised a concern: a graceful `--remote` quit racing the server hang-up could leave the keeper session parked until grace runs out, which would break AC7's 2 s bound. I probed it with a throwaway test in the review worktree: 10 rounds of `--remote` attach, then `\x02q`. In all 10 rounds the keeper logged `session_end{server-eof}` 40–61 ms after the quit. The race did not reproduce. The worktree and probe have been removed.
- Checked how the harness targets processes. Session legs are built as `exec herdr ...` inside `posix_remote_output_command` (src/mosh/ssh_leg.rs:314-315). The fake ssh `exec`s `/bin/sh -c`, and the `bin/herdr` wrapper also `exec`s. So the pid recorded in `legs.log` really is the attacher, and SIGKILL/SIGSTOP land on the live leg. The one exception is the throttle path (`throttle | sh -c | throttle`), where the recorded pid is the outer shell. No test signals a leg under throttle.
- `encode_end` uses type byte 6, which matches `TYPE_END` in src/mosh/session.rs:47. The banner parser matches the outage.rs format "… last contact {}s ago …". The keeper end reasons `grace`, `evicted`, `server-eof`, `client-gone` and `ring-overflow` that the tests assert all exist in keeper.rs.
- `EndpointHealth` (src/client/endpoint/health.rs) pings 5 s after the last received message and expires 10 s after an unanswered ping. That supports the model behind the stall-after-ping test.

## Acceptance-criterion mapping (cluster's share)

- AC1 kill mid-stream, exactly-once, in-order keys, no reconnect snapshot: `keys_around_a_leg_kill_arrive_once_in_order`. Exactly-once and order are asserted. The spec's structural proxy is also asserted: `welcome resumed` plus exactly one `server_connect`. The kill landing mid-frame is likely during the 3 MB flood but is not verified (see Untested scope).
- AC2 SIGSTOP > 8 s: `stopped_leg_is_detected_and_resumed` asserts detection at no less than 7 s, then resume.
- AC3 saved outage past 10 s and the stall right after a ping: `saved_outage_past_the_health_timeout_keeps_the_endpoint` and `saved_leg_stalling_right_after_a_health_ping_does_not_expire_the_endpoint`. Both end with `server_connect == 1`, which also rules out a supervisor reconnect.
- AC4 banner: `outage_banner_counts_up_and_clears_on_resume` checks that the count advances and that the banner clears within 2 s.
- AC5 grace expiry: `grace_expiry_exits_with_the_reattach_hint` (`--remote`) and `saved_grace_expiry_takes_the_supervisor_reconnect_path`.
- AC6 two clients: `two_clients_have_independent_sessions` and `saved_machine_and_remote_session_share_one_keeper_with_two_tokens`.
- AC7 client exit and eviction: `client_exit_ends_the_keeper_session_within_two_seconds` (EOF path), `saved_machine_quit_ends_the_keeper_session_within_two_seconds` (should_stop, 4 rounds) and `saved_reconnect_through_the_same_path_evicts_the_parked_session`.
- AC8 live handoff: three tests (`--remote` with the leg up, `--remote` with the leg down, then resume, and saved).
- AC9 D6: `saved_request_buffered_past_the_command_timeout_executes_once_on_resume` asserts the client-side timeout fired, then exactly one execution after resume.
- AC10 plain paths: `capability_less_remote_behaves_like_upstream` and `kill_switch_behaves_like_upstream` (both modes).
- AC11 ControlMaster bypass, auth and host-key failures fatal: `reconnect_legs_never_use_the_control_master_and_auth_failure_stops_retrying` and `host_key_failure_stops_retrying_like_an_auth_failure`.
- AC12 leg-0 fallbacks: unwritable dir, spawn failure, downgrade and injected END refusals (both modes), plus the 5 s wait (`--remote` only).
- AC13 reconnect stall or EOF before WELCOME: two retry tests.
- AC14 lost token: `resume_with_a_lost_token_is_unknown_and_opens_no_server_connection`.
- AC15 slow leg with 16 MiB: `slow_live_leg_carries_16_mib_of_pastes_without_ending_the_session`. It is weaker than the spec's wording (see below).
- R18 grace from HELLO: `keeper_grace_comes_from_hello_not_the_remote_environment`. Capability advertised: `mosh_status.rs`.

## Observations (non-blocking)

- tests/mosh_resume.rs, slow_live_leg test: the delivery check is `pasted() >= 16 MiB - 256 KiB`. It accepts duplicated bytes, and it accepts up to four lost 64 KiB DATA chunks. It therefore says nothing about upstream integrity under load. Numbered lines checked for completeness would make it a real exactly-once check under flow control.
- tests/mosh_fallback.rs, `reconnect_legs_bypass_the_master_then_fail`: the test writes `~/.ssh/config` with `ControlMaster auto`, but the fake ssh never reads ssh config. The meaningful assertions are on argv (`ControlPath=none`, `ControlMaster=no`, `BatchMode=yes`, no `-S`). That is a sound proxy, because command-line `-o` overrides config, but the config write is decoration.
- tests/mosh_integration.rs, `keeper_grace_comes_from_hello_not_the_remote_environment`: the comment says "keep every reconnect failing for longer than the grace period". But `fail255` makes the first reconnect `Fatal`, so the client gives up at once. The test still validly measures the keeper-side grace (≈5 s and not 3600 s), because nothing reaches the keeper after `leg_down`.
- tests/mosh_harness.rs: the doc comment for `knob::HANG` says "Leg dies before its ready marker", but the code runs `exec sleep 600` (it hangs). Neither `knob::HANG` nor `Remote::stop_server` is used. The doc for `has_welcome_after_resume` claims an ordering ("follows the first leg_down") that the code does not check.
- tests/mosh_status.rs:30: `caps.len() == 4` will break on any upstream capability addition (already a deferred minor).

## Verdict
ready

## Must fix before landing
- none

## Untested scope
- AC7 graceful `--remote` quit (prefix+q) has no 2 s session-end assertion. Only UI-child SIGKILL (EOF path) and saved-machine quit are timed. A 10-round probe at head ended in about 60 ms with `server-eof` every time, so the race reported by task 10 is unconfirmed but also unguarded.
- AC15 is weaker than the spec. The 2 MiB unacked-in-flight bound is not measured end to end (unit-only). 16 MiB is sent as 16 × 1 MiB pastes, not one clipboard image. There is no comparison against the plain bridge under the same throttle. Bulk delivery is checked only by size with a 256 KiB tolerance, so loss or duplication of whole DATA chunks would pass.
- AC1's "in the middle of a downstream frame" is probabilistic, from a kill during a 3 MB flood. Nothing confirms that a partial frame was held back at the kill. The hold-back itself is unit-tested in link_frames.
- Leg stealing on resume (a new HELLO while the keeper still holds the old leg) is never exercised end to end. The fake ssh kills the attacher directly, so the keeper always sees EOF first. It is unit-tested in keeper.rs `resume_live_steals_leg`.
- Eviction through "a second client connection on one `--remote` bridge" is not tested end to end. Only the saved-machine supervisor reconnect is. The ghost token is unit-tested in local_bridge.rs `ghost_token_survives_new_bridge_instance`.
- On a reconnect leg, an END before WELCOME ending the session is not tested end to end (`fake-end` is only used on leg 0). Leg classification is unit-tested.
- Foreign-owned rendezvous dir (only the 0755 mode case), the 5 s keeper wait for saved machines, and live-keeper `server-connect-failed`/`protocol-version` (injected END instead) are not covered end to end.
- AC4 timing is checked loosely: one advance of the last-contact count and a clear within 2 s, not a per-second cadence or "within one frame".

## Deferred OK
- `ring_overflow_during_an_outage_ends_the_session_without_a_hang` is slow (about 90–100 s) and CPU heavy and is not `#[ignore]`d. Acceptable while the CI budget allows it.
- The harness needs perl, /proc and a non-root user (`spawn-fail` uses `ulimit -u 1`). Linux CI targets satisfy this.
- The `caps.len() == 4` brittleness in tests/mosh_status.rs: switch to a containment-only check when upstream next adds a capability.
- The paste-test integrity check (numbered lines with a completeness check) can be strengthened in a follow-up.
- Decorative `~/.ssh/config` write in the ControlMaster test, the stale `knob::HANG` doc, unused `knob::HANG`/`stop_server`, the `has_welcome_after_resume` doc drift and the misleading R18 test comment: cosmetic.
