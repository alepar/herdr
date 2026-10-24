# Final review: cluster 3 (run-docs-evidence), epic mosh-dqw

Source: base 5608de0608918aba2f9d501e7793b984a0ca9cfa, head 2bd5303c2b3ab118792a7b9e10672b487158c931 (both verified with `git rev-parse --verify <sha>^{commit}`). Caller checkout observed at HEAD 2bd5303c, working tree clean, before and during the review. Review source bound via `dispatch-text.mjs bind-review-source` (exit 0).

## Scope

One file: `docs/superpowers/runs/2026-10-24-mosh-phase1-resumable-session/evidence/run-start-baseline.txt` (slice 000019, 19 lines, new). It records `cargo nextest run --locked --no-fail-fast` at dec51b74: 4116 run, 4102 passed, 14 failed, 17 skipped, and the 14 failing test names.

## Checks on the artifact itself

- Provenance holds. dec51b74 ("chore: enter code phase") descends from base 5608de06; the baseline was committed in the next commit, 5aebc9d6. `git diff --stat 5608de06 dec51b74 -- src tests Cargo.toml Cargo.lock` is empty, so the baseline measures the pre-fork product tree exactly.
- Internally consistent. There are 14 FAIL lines and the summary says 14 failed. `friction.md:11` cites the file and puts the failures down to pre-existing agent-status tests. All 14 are agent/api_ping/cross_area/live_handoff names.
- Cosmetic: the header says `exit=100` and the last line says `[exited with code 0]`. The second is the capturing wrapper's exit status, not nextest's. This is confusing but harmless.

## Finding: the baseline shows an upstream test regression that was attributed to the environment

The baseline exists so later suite runs can tell pre-existing failures from regressions. Checked against the end-of-run evidence, it shows one regression:

- Task 11's full-suite run on the integrated tree (`.superpowers/sdd/mosh-dqw-plan/task-11-report.md:49`, log `/tmp/sp-b2a86bd1/mosh-dqw.11/full.log`) had 15 failures: the 14 baseline names plus `herdr::client_mode federated_client_starts_without_local_and_survives_its_restart`.
- That test is absent from the run-start FAIL list. At dec51b74 (product tree == base) it passed.
- At head it panics at `tests/client_mode.rs:1083`: "saved machine discovery must opt into the advertised bridge idle timeout". That is a deterministic command-line assertion. It is not the `agent_kind_mismatch` host-environment failure the report blames for all 15.
- Task 11's "same 15 fail identically at base" check ran at 992f6a45 ("stack: mosh-dqw.9"). That commit already contains the fork, 22 files and +10583 lines over dec51b74, including the `src/remote/saved.rs` hook. So the comparison could not separate fork-caused failures from pre-existing ones. The task-11 review accepted it without rerunning (`task-11-review.md:17`).
- Mechanism (checked against the named risk only): `src/remote/saved.rs:35` now sends saved-machine discovery through `mosh::start_ui_bridge`. When the remote advertises the mosh capability, which it does in this test because the remote is the fork binary, the resume branch (`src/mosh/mod.rs:118-184`) dials `remote-client-bridge --x-mosh-session-v1` through `SshLegDialer`. The `--idle-timeout-v1` opt-in that upstream `SshStdioBridge::start` adds for noninteractive saved machines is applied only to the plain-fallback command (`src/mosh/mod.rs:153-158`). `ssh_leg.rs:649` asserts the resume command contains no idle-timeout flag, so the omission is built in, and the spec never mentions the idle timeout.
- Spec impact: design checklist line 340 requires "`cargo clippy` (repo `clippy.toml`) and the full test suite pass". The spec also forbids editing upstream test modules. So this cannot be waived by changing the test. Either the resume path keeps the idle-timeout semantics for saved machines, or the spec is amended with an explicit, justified exception. A third option, opting saved machines out of resume, would change the feature's scope.
- I did not rebuild to reproduce. The evidence is the two recorded nextest logs (`full.log` at head, `base-fail.log` at 992f6a45, where the test fails at the same line), the run-start baseline (where it passes), and the code path above. A one-test rerun at 5608de06 and at 2bd5303c in throwaway worktrees would close the question in minutes.

## Verdict
not ready (run-start baseline shows client_mode federated_client_starts_without_local_and_survives_its_restart passed pre-fork but fails at head; it was misattributed as a pre-existing host failure)

## Must fix before landing
- Restore the upstream `--idle-timeout-v1` saved-machine behaviour on the resume path, or amend the spec with a justified exception, so `client_mode federated_client_starts_without_local_and_survives_its_restart` passes again; it passed at the run-start baseline dec51b74 and fails at head (tests/client_mode.rs:1083).
- Redo the end-of-run regression comparison against the run-start baseline (dec51b74, product tree == base 5608de06), not against 992f6a45, which already contains the fork; the current "all 15 pre-existing" claim in task-11-report.md:49 is false for one test.

## Untested scope
- No end-of-run suite evidence is committed next to the run-start baseline, so the run's claim that the full suite passes is backed only by uncommitted /tmp logs and a comparison made at the wrong base.
- The baseline file records failing test names only, not failure messages, so it cannot alone show that a later failure under the same name has the same cause.
- The idle-timeout regression was not reproduced by a fresh build in this review; the conclusion rests on the recorded nextest logs and the code path.

## Deferred OK
- The `[exited with code 0]` wrapper trailer after `exit=100` in run-start-baseline.txt is cosmetic and could be dropped or labelled in a later run.
- The 14 baseline failures are pre-existing agent-status/host-environment failures outside this epic's scope, as friction.md records.
