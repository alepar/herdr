## diffstat

 HERDR_MOSH_VISION.md                               |  252 +++
 ...6-10-24-mosh-phase1-resumable-session-design.md |  462 +++++
 ...mable-session-roast-design-1-step-back-keys.txt |   22 +
 ...1-resumable-session-roast-design-1-step-back.md |   16 +
 ...mosh-phase1-resumable-session-roast-design-1.md |  193 ++
 ...mosh-phase1-resumable-session-roast-design-2.md |   77 +
 .../amend-design-1.md                              |   47 +
 .../coverage-amend-design-1-review-1.md            |   10 +
 .../coverage-amend-design-1-review-2.md            |   13 +
 .../coverage-findings-round-1.md                   |   17 +
 .../coverage-findings-round-2.md                   |   14 +
 .../coverage-ledger.md                             |   45 +
 .../coverage-round-1-requirements.md               |   18 +
 .../coverage-round-1-review-1.md                   |   34 +
 .../coverage-round-1-review-2.md                   |   30 +
 .../coverage-round-2-requirements.md               |   20 +
 .../coverage-round-2-review-1.md                   |   33 +
 .../coverage-round-2-review-2.md                   |   33 +
 .../evidence/run-start-baseline.txt                |   19 +
 .../findings-ledger-design.md                      |   26 +
 .../findings-overrides-design.md                   |    4 +
 .../friction.md                                    |   11 +
 .../inputs/herdr-mosh-touch-map.md                 |  498 +++++
 .../inputs/herdr-remote-report.md                  |  354 ++++
 .../inputs/mosh-report.md                          |  506 +++++
 .../inputs/task-tree-amend1.json                   |  539 +++++
 .../inputs/task-tree-r1.json                       |  538 +++++
 .../inputs/task-tree-r2.json                       |  538 +++++
 .../run.md                                         |   34 +
 docs/superpowers/specs/INDEX.md                    |    5 +
 scripts/mosh-diff-budget.sh                        |   71 +
 src/cli/status.rs                                  |    2 +
 src/client/mod.rs                                  |    2 +
 src/client/shell/composition.rs                    |    1 +
 src/mosh/link_frames.rs                            |  234 +++
 src/mosh/local_bridge.rs                           | 2179 ++++++++++++++++++++
 src/mosh/mod.rs                                    |  227 ++
 src/mosh/outage.rs                                 |  314 +++
 src/mosh/remote/attacher.rs                        |  517 +++++
 src/mosh/remote/keeper.rs                          | 1487 +++++++++++++
 src/mosh/remote/mod.rs                             |  374 ++++
 src/mosh/ring.rs                                   |  166 ++
 src/mosh/session.rs                                | 1770 ++++++++++++++++
 src/mosh/ssh_leg.rs                                | 1010 +++++++++
 src/remote/attach.rs                               |    3 +-
 src/remote/host.rs                                 |    2 +
 src/remote/saved.rs                                |    2 +-
 src/update.rs                                      |    4 +-
 tests/mosh_fallback.rs                             |  438 ++++
 tests/mosh_harness.rs                              |  864 ++++++++
 tests/mosh_integration.rs                          |  145 ++
 tests/mosh_multi_client.rs                         |  485 +++++
 tests/mosh_resume.rs                               |  478 +++++
 tests/mosh_status.rs                               |   31 +
 54 files changed, 15210 insertions(+), 4 deletions(-)

## generated-input provenance

## Generated-input provenance
Captured range: 5608de0608918aba2f9d501e7793b984a0ca9cfa..2bd5303c2b3ab118792a7b9e10672b487158c931
Selection: complete; selected product/input paths: 54; omitted historical payloads: 0.
diffstat.txt describes selected product/input paths. Reports, findings and collection metadata remain ordinary review inputs.
Open each report/archive at its recorded revision. Omission supplies no finding disposition or verdict.

## report summaries

### task 1
Status: none
Assumptions:
- Module path is `crate::remote::attach::mosh` (mount lives in attach.rs), so rendezvous items use `pub(in crate::remote::attach::mosh)` and are in nested `mosh::remote::rendezvous` module (no glob re-export, avoids an unused-import warning without extra lint allowances). Consumers (mosh-dqw.7/.10) 
- fits_unix_socket_path in platform is private and attach.rs's is test-only, so a fork-local `fits_sun_path` (<=103) is used, per brief.
- Leg::finish closes stream halves before teardown so a child blocked on stdin can exit.
Concerns:
- `#![allow(dead_code)]` at src/mosh/mod.rs is intentional scaffold; mosh-dqw.11 removes it.
- Unit tests for the non-Unix start_ui_bridge gate are not present (cfg(not(unix)) unreachable here).
### task 2
Status: none
Assumptions:
none
Concerns:
none
### task 3
Status: none
Assumptions:
- Script run in the worktree sees origin/master == merge base; counts working-tree changes too.
- src/remote/attach.rs:46-47 still hardcodes the herdr.dev URLs (out of scope; follow-up).
- An early build/strings capture raced with a concurrent edit and was discarded; the recorded results are from a clean re-run.
Concerns:
- Script run in the worktree sees origin/master == merge base; counts working-tree changes too.
- src/remote/attach.rs:46-47 still hardcodes the herdr.dev URLs (out of scope; follow-up).
- An early build/strings capture raced with a concurrent edit and was discarded; the recorded results are from a clean re-run.
### task 4
Status: none
Assumptions:
- Ack timer uses `ack_pending_since` (first unacked delivered byte) instead of the brief's `last_ack_at`, to avoid an Instant at construction; same observable cadence (64 KiB / 200 ms / heartbeat).
- No control queue/outbox: ACK/HEARTBEAT are computed in poll_send.
- On a keeper HELLO event, on_leg_bytes returns immediately; caller must call `on_leg_bytes(now, &[])` after welcome() to drain trailing bytes (documented on the event). Processing also stops after PeerEnd/ResumeRefused.
- leg_down resets last_ack_sent to 0 and set_up re-arms an ack if delivered > 0, so acks lost with a dead leg are reissued.
- welcome(Unknown) returns the refusal bytes without bringing the leg up.
- may_read_source Up limit is min(WINDOW, ring cap).
Concerns:
- Ring overflow can still occur near the cap when a 64 KiB read exceeds remaining space while down (reported, per spec).
- Leg 0 losing WELCOME after the keeper created the session leaves the keeper with an existing token and the local end retrying resume:false -> token-in-use; handled by Task 7/8 policy (plain fallback), not by this module.
### task 5
Status: none
Assumptions:
none
Concerns:
none
### task 6
Status: none
Assumptions:
none
Concerns:
none
### task 7
Status: none
Assumptions:
- Session name for rendezvous per plan Planner notes round 2.
- Refusal trace: welcome{status:"refused",reason}.
- unknown WELCOME also closes the leg (leg is not brought up).
- HELLO is read byte-exact (header + payload), so no trailing bytes are lost before the endpoint takes over.
- A `pending` counter of accepted-but-unanswered connections also defers idle exit (closes a window the brief did not cover).
Concerns:
- If the ring fills to exactly 16 MiB while down, may_read_source turns false and the server is back-pressured
- Silence-verdict path (8 s) is not covered by an in-process test (would need an 8 s sleep); it is covered in session.rs with injected clocks.
- keeper.rs test module is large (~800 lines); not split, per scope.
### task 8
Status: Status: IMPLEMENTED. Branch task-mosh-dqw.8, base b28abb6e (stack: mosh-dqw.4, stacked, not rebased), head 4743fbef, tree 26befd6634f5085544680b3501941e740091fe7f.
Assumptions:
- No bead amendments or comments; bd acceptance used as written.
- Client reading while down: the brief gates reads on `may_read_source()`, but with 64 KiB chunks and an 8 MiB cap the ring can sit exactly at the cap and never overflow. While no leg is up the client reader is therefore always fed, so any byte past the cap deterministically overflows (spec: "one cl
- `BridgeParams` has an extra `trace_dir` field (so the trace is testable without env mutation); start_ui_bridge fills it from HERDR_MOSH_TRACE.
- `resume{replayed_up,replayed_down}`: replayed_up = outbound_end - WELCOME.recv_offset (a side decoder reads WELCOME); replayed_down is null because the local end cannot observe what the keeper replays and session.rs is out of this task's files.
- Linked is injected after resume only if a Resuming was injected in that outage.
- Client EOF / overflow while down keep reconnecting only to deliver END (until give-up, or 5 s for overflow).
Concerns:
- local_bridge.rs is large (~1100 lines code + ~1000 lines tests); not split per brief scope.
- Client disconnect during the leg-0 dial/WELCOME wait is noticed only after WELCOME (the client reader starts then), by design of the pre-WELCOME gate.
- Graceful close waits for the client writer; a client that never reads blocks the bridge thread until should_stop (stop forces shutdown).
- Windows/non-Unix path compiled out; inspection only.
### task 9
Status: Status: IMPLEMENTED. Base b584c85981672c8f347df8968fac9bb2f23db7d9 (stacked on task-mosh-dqw.4). Head 541000cc561f031e605e741c3f7c5ebb74a9cfa0. Source tree 1d100d47631d537dcb39d3023ab9546fbe4775e6.
Assumptions:
- Bead says "Exactly 3 upstream lines" / status.rs "1 hook line"; the frozen plan fixes 4 deliberately (rustfmt splits the attribute off the vec element). Followed the plan; reported as 4.
- Session string hashed for rendezvous is `active_name()` or `DEFAULT_SESSION_NAME` ("default"); the keeper (mosh-dqw.7) must resolve the same way.
- Existing spec scaffold kept `attacher::run` taking args; a `debug_assert` checks it equals `[SESSION_FLAG]`.
Concerns:
- No end-to-end run of the real binary through the attacher (it would spawn a herdr server via ensure_remote_server_running); covered by in-process tests only. keeper::run remains the dqw.7 stub, so a spawned keeper currently exits with Unsupported until dqw.7 merges.
### task 10
Status: none
Assumptions:
none
Concerns:
- Product observation (not fixed, outside this task's scope): after a graceful quit of a `--remote` client, the server can hang up first (`Detach` to the server). The keeper then sends `END{server-eof}`, the bridge exits before its 200 ms final ack lands, and the keeper keeps the session parked unti
- `ring_overflow_*` is slow (about 90 s) and CPU heavy (server renders a flood until the 16 MiB ring fills). Left un-ignored; mark `#[ignore]` if the CI budget needs it.
- The harness' fake ssh requires `perl` (throttle) and `/proc` (keeper discovery); both exist on the dev and CI Linux targets. The `spawn-fail` knob uses `ulimit -S -u 1`, which only blocks forking for non-root users.
- Each test binary also runs the six `support::tests` and the `terminal_screen` unit test (existing pattern for tests that declare `pub mod support`).
### task 11
Status: none
Assumptions:
- Removed task 9's evidence file from the tree because the budget script audits it as upstream; its content remains in task 9's history.
- Windows build not verifiable here; cfg_attr allow keeps non-unix tolerant, but non-unix unused-import warnings (if any) are unmeasured.
- Deferred minors in files touched: mosh-dqw.6 fmt-drift: `cargo fmt --check` is clean (module mounted under rustfmt::skip hook); direct `rustfmt --check src/mosh/ssh_leg.rs` still reports diffs, left alone. Others (1 non-unix gate test, outage stale repaint, encode_link_frame signature, dial_comman
- ClientShellSnapshot absence after resume is covered only by the server_connect-count proxy.
Concerns:
- Removed task 9's evidence file from the tree because the budget script audits it as upstream; its content remains in task 9's history.
- Windows build not verifiable here; cfg_attr allow keeps non-unix tolerant, but non-unix unused-import warnings (if any) are unmeasured.
- Deferred minors in files touched: mosh-dqw.6 fmt-drift: `cargo fmt --check` is clean (module mounted under rustfmt::skip hook); direct `rustfmt --check src/mosh/ssh_leg.rs` still reports diffs, left alone. Others (1 non-unix gate test, outage stale repaint, encode_link_frame signature, dial_comman
- ClientShellSnapshot absence after resume is covered only by the server_connect-count proxy.

## interfaces and seams

New public files:
scripts/mosh-diff-budget.sh
src/mosh/link_frames.rs
src/mosh/local_bridge.rs
src/mosh/mod.rs
src/mosh/outage.rs
src/mosh/remote/attacher.rs
src/mosh/remote/keeper.rs
src/mosh/remote/mod.rs
src/mosh/ring.rs
src/mosh/session.rs
src/mosh/ssh_leg.rs
tests/mosh_fallback.rs
tests/mosh_harness.rs
tests/mosh_integration.rs
tests/mosh_multi_client.rs
tests/mosh_resume.rs
tests/mosh_status.rs

Signatures:
src/mosh/link_frames.rs: pub(super) struct OversizedFrame {
src/mosh/link_frames.rs: pub(super) struct FrameSplitter {
src/mosh/link_frames.rs: pub fn new() -> Self {
src/mosh/link_frames.rs: pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, OversizedFrame> {
src/mosh/link_frames.rs: pub fn held_back(&self) -> usize {
src/mosh/link_frames.rs: pub fn at_boundary(&self) -> bool {
src/mosh/link_frames.rs: pub fn frames_seen(&self) -> u64 {
src/mosh/link_frames.rs: pub fn drop_partial(&mut self) -> usize {
src/mosh/link_frames.rs: pub(super) fn encode_link_frame(status: &LinkStatus) -> Vec<u8> {
src/mosh/local_bridge.rs: pub(super) enum PlainFallback {
src/mosh/local_bridge.rs: pub(super) struct BridgeParams {
src/mosh/local_bridge.rs: pub(super) fn start(
src/mosh/mod.rs: pub(in crate::remote) mod remote;
src/mosh/mod.rs: pub(in crate::remote) const STATUS_CAPABILITY: &str = "x-mosh-resume-bridge-v1";
src/mosh/mod.rs: pub(in crate::remote) const SESSION_FLAG: &str = "--x-mosh-session-v1";
src/mosh/mod.rs: pub(in crate::remote) const KEEPER_FLAG: &str = "--x-mosh-keeper-v1";
src/mosh/mod.rs: pub(super) struct MoshConfig {
src/mosh/mod.rs: pub(super) fn get() -> MoshConfig {
src/mosh/mod.rs: pub(in crate::remote) fn start_ui_bridge(
src/mosh/outage.rs: pub(crate) const LINK_KIND: &str = "x-mosh.link.v1";
src/mosh/outage.rs: pub(crate) enum LinkStatus {
src/mosh/outage.rs: pub(crate) struct OutageState {
src/mosh/outage.rs: pub(crate) fn observe(endpoint_id: &ClientEndpointId, kind: &str, data: &str) -> bool {
src/mosh/outage.rs: pub(crate) fn paint(frame: &mut FrameData, active: &ClientEndpointId, label: &str) {
src/mosh/remote/attacher.rs: pub(super) fn run(args: &[String]) -> io::Result<()> {
src/mosh/remote/keeper.rs: pub(super) fn run(args: &[String]) -> io::Result<()> {
src/mosh/remote/mod.rs: pub(in crate::remote) fn dispatch(args: &[String]) -> Option<io::Result<()>> {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) mod rendezvous {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) struct KeeperRendezvous {
src/mosh/remote/mod.rs: pub(super) fn current_build_id() -> String {
src/mosh/remote/mod.rs: pub(super) fn keeper_hash(
src/mosh/remote/mod.rs: pub(super) fn rendezvous_paths(temp_dir: &Path, euid: u32, hash: &str) -> KeeperRendezvous {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) fn resolve(session: &str) -> KeeperRendezvous {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) fn validate_private_dir(dir: &Path) -> io::Result<()> {
src/mosh/remote/mod.rs: pub(super) fn validate_private_dir_for(dir: &Path, euid: u32) -> io::Result<()> {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) struct RendezvousLock {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) fn try_lock(path: &Path) -> io::Result<Option<RendezvousLock>> {
src/mosh/remote/mod.rs: pub(in crate::remote::attach::mosh) fn lock(path: &Path) -> io::Result<RendezvousLock> {
src/mosh/ring.rs: pub(super) struct RingOverflow {
src/mosh/ring.rs: pub(super) struct ReplayRing {
src/mosh/ring.rs: pub fn new(capacity: usize) -> Self {
src/mosh/ring.rs: pub fn capacity(&self) -> usize {
src/mosh/ring.rs: pub fn start_offset(&self) -> u64 {
src/mosh/ring.rs: pub fn end_offset(&self) -> u64 {
src/mosh/ring.rs: pub fn occupancy(&self) -> usize {
src/mosh/ring.rs: pub fn push(&mut self, data: &[u8]) -> Result<(), RingOverflow> {
src/mosh/ring.rs: pub fn ack(&mut self, offset: u64) -> Result<(), ()> {
src/mosh/ring.rs: pub fn read_at(&self, offset: u64, max: usize) -> Option<Vec<u8>> {
src/mosh/session.rs: pub(super) const SESSION_PROTOCOL_VERSION: u32 = 1;
src/mosh/session.rs: pub(super) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
src/mosh/session.rs: pub(super) const SILENCE_TIMEOUT: Duration = Duration::from_secs(8);
src/mosh/session.rs: pub(super) const PRE_WELCOME_BOUND: Duration = Duration::from_secs(20);
src/mosh/session.rs: pub(super) const LEG_SETUP_MAX: Duration = Duration::from_secs(15);
src/mosh/session.rs: pub(super) const MAX_DATA_CHUNK: usize = 64 * 1024;
src/mosh/session.rs: pub(super) const WINDOW: usize = 2 * 1024 * 1024;
src/mosh/session.rs: pub(super) const ACK_EVERY_BYTES: u64 = 64 * 1024;
src/mosh/session.rs: pub(super) const ACK_EVERY: Duration = Duration::from_millis(200);
src/mosh/session.rs: pub(super) const MAX_CONTROL_PAYLOAD: usize = 4096;
src/mosh/session.rs: pub(super) mod end_reason {
src/mosh/session.rs: pub const KEEPER_UNAVAILABLE: &str = "keeper-unavailable";
src/mosh/session.rs: pub const SERVER_CONNECT_FAILED: &str = "server-connect-failed";
src/mosh/session.rs: pub const PROTOCOL_VERSION: &str = "protocol-version";
src/mosh/session.rs: pub const TOKEN_IN_USE: &str = "token-in-use";
src/mosh/session.rs: pub const SERVER_EOF: &str = "server-eof";
src/mosh/session.rs: pub const RING_OVERFLOW: &str = "ring-overflow";
src/mosh/session.rs: pub const CLIENT_GONE: &str = "client-gone";
src/mosh/session.rs: pub(super) struct LegProtocolError(pub String);
src/mosh/session.rs: pub(super) struct Token([u8; 16]);
src/mosh/session.rs: pub fn generate() -> io::Result<Token> {
src/mosh/session.rs: pub(super) struct Hello {
src/mosh/session.rs: pub(super) enum WelcomeStatus {
src/mosh/session.rs: pub(super) struct Welcome {
src/mosh/session.rs: pub(super) struct End {
src/mosh/session.rs: pub(super) enum SessionMsg {
src/mosh/session.rs: pub fn encode(&self) -> Vec<u8> {
src/mosh/session.rs: pub(super) struct Decoder {
src/mosh/session.rs: pub fn push(&mut self, bytes: &[u8]) {
src/mosh/session.rs: pub fn next(&mut self) -> Result<Option<SessionMsg>, LegProtocolError> {
src/mosh/session.rs: pub(super) enum HelloAnswer {
src/mosh/session.rs: pub(super) fn answer_hello(hello: &Hello, exists: bool) -> HelloAnswer {
src/mosh/session.rs: pub(super) fn setup_deadline(now: Instant, give_up_at: Instant) -> Instant {
src/mosh/session.rs: pub(super) enum Role {
src/mosh/session.rs: pub(super) enum LinkPhase {
src/mosh/session.rs: pub(super) enum SessionEvent {
src/mosh/session.rs: pub(super) enum LinkVerdict {
src/mosh/session.rs: pub(super) struct SessionEndpoint {
src/mosh/session.rs: pub fn new(role: Role, token: Token, ring_cap: usize) -> Self {
src/mosh/session.rs: pub fn token(&self) -> Token {
src/mosh/session.rs: pub fn recv_offset(&self) -> u64 {
src/mosh/session.rs: pub fn last_inbound(&self) -> Option<Instant> {
src/mosh/session.rs: pub fn outbound_end(&self) -> u64 {
src/mosh/session.rs: pub fn fully_acked(&self) -> bool {
src/mosh/session.rs: pub fn is_up(&self) -> bool {
src/mosh/session.rs: pub fn established(&self) -> bool {
src/mosh/session.rs: pub fn hello(&mut self, now: Instant, grace_ms: u32, previous_token: Option<Token>) -> Vec<u8> {
src/mosh/session.rs: pub fn welcome(
src/mosh/session.rs: pub fn refuse(&self, reason: &str) -> Vec<u8> {
src/mosh/session.rs: pub fn end_now(&self, reason: &str) -> Vec<u8> {
src/mosh/session.rs: pub fn on_leg_bytes(
src/mosh/session.rs: pub fn leg_down(&mut self) {
src/mosh/session.rs: pub fn may_read_source(&self) -> bool {
src/mosh/session.rs: pub fn push_source(&mut self, data: &[u8]) -> Result<(), RingOverflow> {
src/mosh/session.rs: pub fn delivered(&mut self, now: Instant, offset: u64) {
src/mosh/session.rs: pub fn poll_send(&mut self, now: Instant) -> Option<Vec<u8>> {
src/mosh/session.rs: pub fn queue_final_end(&mut self, reason: &str) {
src/mosh/session.rs: pub fn poll_timers(&self, now: Instant) -> Option<LinkVerdict> {
src/mosh/session.rs: pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
src/mosh/ssh_leg.rs: pub(super) enum LegKind {
src/mosh/ssh_leg.rs: pub(super) struct LegEndReport {
src/mosh/ssh_leg.rs: pub(super) struct Leg {
src/mosh/ssh_leg.rs: pub(super) fn new(
src/mosh/ssh_leg.rs: pub(super) fn from_child(mut child: std::process::Child) -> io::Result<Leg> {
src/mosh/ssh_leg.rs: pub(super) fn take_reader(&mut self) -> Option<Box<dyn io::Read + Send>> {
src/mosh/ssh_leg.rs: pub(super) fn take_writer(&mut self) -> Option<Box<dyn io::Write + Send>> {
src/mosh/ssh_leg.rs: pub(super) fn mark_welcome(&mut self) {
src/mosh/ssh_leg.rs: pub(super) fn record_pre_welcome_end(&mut self, reason: String) {
src/mosh/ssh_leg.rs: pub(super) fn finish(mut self, kill: bool) -> LegEndReport {
src/mosh/ssh_leg.rs: pub(super) enum LegError {
src/mosh/ssh_leg.rs: pub(super) trait LegDialer: Send {
src/mosh/ssh_leg.rs: pub(super) fn classify_leg(kind: LegKind, end: &LegEndReport) -> LegError {
src/mosh/ssh_leg.rs: pub(super) struct SshLegDialer {
src/mosh/ssh_leg.rs: pub(super) fn new(
src/mosh/ssh_leg.rs: pub(super) fn select_resume_candidate(

Seam contract beads:
✓ mosh-dqw.1 P2 task Seam contract: mosh module scaffold and leg interface

## deferred minors

Task 3 (mosh-dqw.3): minor (deferred): [budget-insertions-only] scripts/mosh-diff-budget.sh: the 20-line budget counts inserted lines only; deletions are not counted
Task 3 (mosh-dqw.3): minor (deferred): [rustfmt-skip-attribute] src/update.rs:25-26: #[rustfmt::skip] added to keep each const one statement per line
Task 1 (mosh-dqw.1): minor (deferred): [missing-evidence] task-1-report.md: test commands summarized without raw output or structured evidence records (sourceTree 769851ce... stated only in prose)
Task 1 (mosh-dqw.1): minor (deferred): [missing-edge-case-test] src/mosh/mod.rs:96: non-Unix start_ui_bridge gate untested (cfg(not(unix)) and not cross-compiled, inspection only)
Task 1 (mosh-dqw.1): minor (deferred): [scaffold-allow] src/mosh/mod.rs:6: crate-wide #![allow(dead_code)]; removal tracked by mosh-dqw.11
Task 2 (mosh-dqw.2): minor (deferred): [untested-structural-criterion] src/client/mod.rs:2095 hook's skip of decode_endpoint_control is enforced by `continue` only, not exercised by a unit test (decoder-ignored test pins harmlessness)
Task 2 (mosh-dqw.2): minor (deferred): [stale-clear-needs-repaint] src/mosh/outage.rs paint: 2 s stale clear only takes effect on the next repaint; accepted by design
Task 5 (mosh-dqw.5): minor (deferred): [signature-deviation] src/mosh/link_frames.rs:89 encode_link_frame takes &LinkStatus rather than data_json as the bead text says; brief specifies typed and it keeps one payload definition
Task 6 (mosh-dqw.6): minor (deferred): [fmt-drift] src/mosh/ssh_leg.rs:~244,~267 (and other long lines): rustfmt --check reports diffs although the report claims cargo fmt --check clean
Task 6 (mosh-dqw.6): minor (deferred): [deferred-to-later-task] 20 s pre-WELCOME bound and END capture live in mosh-dqw.8; only the EOF/no-WELCOME fall-through of classify_leg is tested here
Task 6 (mosh-dqw.6): minor (deferred): [deferred-to-later-task] HERDR_REMOTE_BINARY hint line is not printed by select_resume_candidate; caller in mosh-dqw.8 must print it
Task 6 (mosh-dqw.6): minor (deferred): [error-classification] src/mosh/ssh_leg.rs dial_command: ssh spawn failure is Fatal even on a reconnect leg, where a transient EAGAIN would end the session
Task 6 (mosh-dqw.6): minor (deferred): [blocking-teardown] spawn_leg teardown joins the stderr thread after kill; a ProxyCommand grandchild holding stderr open could block teardown (acknowledged in report)
Task 6 (mosh-dqw.6): minor (deferred): [stderr-on-leg0] noninteractive leg 0 pipes and captures stderr though the brief says no stderr is read on leg 0; used only for today's error message, not matched
Task 4 (mosh-dqw.4): minor (deferred): [final-end-loss] src/mosh/session.rs poll_send: session-final END is consumed on emit; if the leg dies before delivery it is lost, owners (tasks 7/8) must requeue
Task 4 (mosh-dqw.4): minor (deferred): [brief-deviation] ack timer uses ack_pending_since and no control queue instead of brief's last_ack_at/outbox; behaviour equivalent
Task 4 (mosh-dqw.4): minor (deferred): [caller-contract] keeper must call on_leg_bytes(now,&[]) after welcome() to drain bytes following HELLO; documented but easy to miss
Task 4 (mosh-dqw.4): minor (deferred): [missing-evidence] report has no structured evidence array; test command and pass summary given in prose only
Task 9 (mosh-dqw.10): minor (deferred): [late-rejection-side-effect] src/remote/host.rs:26 - unknown --x-mosh-* flag is rejected only after ensure_remote_server_running() has started a server
Task 9 (mosh-dqw.10): minor (deferred): [brittle-assertion] tests/mosh_status.rs:30 - caps.len()==4 breaks when upstream adds a capability; assert containment only
Task 9 (mosh-dqw.10): minor (deferred): [child-not-reaped] src/mosh/remote/attacher.rs keeper_command spawn - spawn().map(drop) never reaps the keeper child
Task 9 (mosh-dqw.10): minor (deferred): [platform-coverage-gap] src/mosh/remote/attacher.rs peer_uid - only linux and macos implementations; other unix targets will not compile
Task 9 (mosh-dqw.10): minor (deferred): [indirect-test-coverage] src/mosh/remote/attacher.rs tests - peer-uid test calls connect_checked directly and run() stdout END plus non-zero exit is not tested end to end
Task 9 (mosh-dqw.10): complete (commits b29ef4c..c823424, fix pass, 1 parked — reason: status.rs hook is 2 lines (4 upstream total) vs the bead's "exactly 3": plan-mandated. rustfmt keeps #[cfg(unix)] on its own line, and any one-line form either drops the Unix gate or adds more upstream lines. The bead has no amendment, so it needs one saying "4 lines". Left for the ledger and the final epic review. — finding: Important (plan-mandated): src/cli/status.rs:300-301 - the bead acceptance "Exactly 3 upstream lines changed" (status.rs "1 hook line") is not met; 4 lines were changed because rustfmt splits `#[cfg(unix)]` off the vec element. The frozen plan and a spec note mandate 4, but the bead has no amendment, so the sources conflict. Resolve with a bead amendment or a one-line form; no other code defect found.)
Task 7 (mosh-dqw.7): minor (deferred): [ring-cap-edge] src/mosh/remote/keeper.rs:583 ring filled to exactly 16 MiB while down back-pressures the server instead of overflowing; session ends at grace rather than closing the server stream immediately
Task 7 (mosh-dqw.7): minor (deferred): [untested-timer-path] src/mosh/remote/keeper.rs:256 silence-verdict path in run_timers has no in-process test (covered by session.rs injected-clock tests)
Task 7 (mosh-dqw.7): minor (deferred): [oversized-test-module] src/mosh/remote/keeper.rs test module is ~800 lines in one file
Task 7 (mosh-dqw.7): minor (deferred): [fragile-helper] src/mosh/remote/keeper.rs:329 is_end identifies END by type byte of an encoded empty END only
Task 8 (mosh-dqw.8): minor (deferred): [doc-drift] resume trace event emits replayed_down as null because the local end cannot observe keeper replay (spec lists replayed_down); report discloses it
Task 8 (mosh-dqw.8): minor (deferred): [oversized-module] src/mosh/local_bridge.rs is ~2175 lines (code plus ~1000 lines of tests) in one file
Task 8 (mosh-dqw.8): minor (deferred): [unverified-platform-gate] non-Unix-local cfg(not(unix)) branch in src/mosh/mod.rs is inspection-only, not cross-compiled
Task 8 (mosh-dqw.8): minor (deferred): [blocking-stop-path] on_stop and close_client busy-sleep-poll on the bridge thread (bounded 500 ms / until should_stop); a client that never reads can hold graceful close until stop
Task 10 (mosh-dqw.9): minor (deferred): [weaker-than-spec-coverage] tests/mosh_resume.rs:398 flow-control row uses 16x1 MiB bracketed pastes and asserts no leg_down/session_end, not the 2 MiB in-flight bound or a single 16 MiB image (brief allows unit coverage)
Task 10 (mosh-dqw.9): minor (deferred): [weaker-than-spec-coverage] row 7 should_stop-first path for --remote not separately exercised; saved-machine quit covers in-process stop
Task 10 (mosh-dqw.9): minor (deferred): [weaker-than-spec-coverage] server-connect-failed and protocol-version are injected as END on leg 0 by the fake ssh rather than produced by a live keeper
Task 10 (mosh-dqw.9): minor (deferred): [weaker-than-spec-coverage] foreign-owned rendezvous dir covered only by the mode-0755 case
Task 10 (mosh-dqw.9): minor (deferred): [product-observation] graceful --remote quit racing server hang-up leaves the keeper session parked until grace (report Concern); no follow-up bead noted
Task 10 (mosh-dqw.9): minor (deferred): [slow-test] ring_overflow_during_an_outage test takes about 90 s and is CPU heavy, not ignored
Task 10 (mosh-dqw.9): minor (deferred): [environment-dependency] harness requires perl and /proc, and spawn-fail knob only works for non-root users
Task 10 (mosh-dqw.9): minor (deferred): [stray-artifact] evidence/mosh-dqw.9/test-summary.log committed to repo though report says four new files only
Task 11 (mosh-dqw.11): minor (deferred): [non-unix-unverified] src/mosh/mod.rs:6 cfg_attr(not(unix), allow(dead_code)) replaces strict check; Windows build/lint not run (toolchain absent)
Task 11 (mosh-dqw.11): minor (deferred): [duplicate-helper] two private trace helpers (keeper.rs, mod.rs) not merged; brief permitted skipping
Task 11 (mosh-dqw.11): minor (deferred): [proxy-coverage] ClientShellSnapshot absence after resume covered only by server_connect count proxy
Task 11 (mosh-dqw.11): minor (deferred): [fmt-drift] rustfmt --check on src/mosh/ssh_leg.rs still reports diffs (deferred from mosh-dqw.6)
Task 11 (mosh-dqw.11): minor (deferred): [missing-evidence] report gives no structured evidence records; 15 pre-existing agent_kind_mismatch failures have no recorded run owner (attribution gap)

## amendments

none
