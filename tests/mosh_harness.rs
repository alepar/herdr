//! Shared helpers for `tests/mosh_*.rs`: a disposable "remote" herdr server, a fake `ssh`
//! that runs the remote command locally, pty-hosted clients, leg control and trace readers.
//!
//! Self-contained on purpose: cargo also builds this file as its own (empty) test target, so
//! it must not reference `crate::support`.

#![cfg(unix)]

#[path = "support/terminal_screen.rs"]
mod terminal_screen;

// The crate is bin-only, so the bootstrap line parser is included by path (the file is
// self-contained on purpose). Its own unit tests ride along in every harness user.
#[allow(dead_code)]
#[path = "../src/mosh/quic_bootstrap.rs"]
mod quic_bootstrap;

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};

pub const SCREEN_COLS: u16 = 80;
pub const SCREEN_ROWS: u16 = 24;

/// Poll `predicate` until it holds or `timeout` passes; the last evaluation decides.
pub fn wait_until<F>(timeout: Duration, interval: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(interval);
    }
    predicate()
}

/// `wait_until` with a 50 ms poll.
pub fn wait_for<F: FnMut() -> bool>(timeout: Duration, predicate: F) -> bool {
    wait_until(timeout, Duration::from_millis(50), predicate)
}

// ---- host-wide timing lock and socket probes ----

/// Host-wide lock of every flood and link-emulation test (nextest runs each test in its own
/// process, so concurrent floods would skew each other's clocks). One owner, one lock file:
/// tests/mosh_pacing.rs and tests/mosh_resume.rs both take this. Held until dropped.
pub struct TimingLock(#[allow(dead_code)] fs::File); // held only for its flock

pub fn timing_lock() -> TimingLock {
    use std::os::unix::io::AsRawFd;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("herdr-mosh-pacing.lock"))
        .expect("open the timing lock");
    // SAFETY: flock(2) on a file descriptor this function owns.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(rc, 0, "flock failed");
    TimingLock(file)
}

/// `ss -xpn` rows of the `keepers` processes: (state, recv_q, send_q, local address).
/// Empty when `ss` cannot be run (it is Linux-only).
fn keeper_unix_sockets(keepers: &[libc::pid_t]) -> Vec<(String, u64, u64, String)> {
    let tags: Vec<String> = keepers.iter().map(|pid| format!("pid={pid},")).collect();
    let Ok(out) = std::process::Command::new("ss").arg("-xpn").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| tags.iter().any(|tag| line.contains(tag.as_str())))
        .filter_map(|line| {
            // Netid State Recv-Q Send-Q Local-Address Local-Inode Peer-Address Peer-Inode Process
            let f: Vec<&str> = line.split_whitespace().collect();
            Some((
                f.get(1)?.to_string(),
                f.get(2)?.parse().ok()?,
                f.get(3)?.parse().ok()?,
                f.get(4)?.to_string(),
            ))
        })
        .collect()
}

/// The backlog metric: Recv-Q of the keeper's server stream (its connected, unbound `*`
/// socket to the herdr server; the keeper's leg sockets are accepted on its rendezvous path):
/// bytes the server wrote that the keeper has not read, the same quantity as FIONREAD.
/// `None` when `ss` fails or the stream is gone.
pub fn keeper_server_backlog(keepers: &[libc::pid_t]) -> Option<u64> {
    keeper_unix_sockets(keepers)
        .into_iter()
        .filter(|(state, _, _, local)| state == "ESTAB" && local == "*")
        .map(|(_, recv_q, _, _)| recv_q)
        .max()
}

/// Send-Q of the keeper's leg sockets: bytes the keeper wrote that the attacher has not read
/// (a lower bound on the downstream bytes the keeper holds unacked).
pub fn keeper_leg_send_q(keepers: &[libc::pid_t]) -> Option<u64> {
    keeper_unix_sockets(keepers)
        .into_iter()
        .filter(|(state, _, _, local)| state == "ESTAB" && local != "*")
        .map(|(_, _, send_q, _)| send_q)
        .max()
}

fn app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn write_executable(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn signal_group(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid {
        // SAFETY: the pid heads a process group this harness created (a pty session leader).
        unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    }
}

struct Spawned {
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl Spawned {
    fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    fn kill_group(&mut self) {
        signal_group(self.pid(), libc::SIGKILL);
        let _ = self.child.wait();
        drop(self._master.take());
    }
}

fn open_pty(cols: u16, rows: u16) -> portable_pty::PtyPair {
    native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap()
}

static BASE_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Knobs a test flips mid-run (files under `<base>/knobs/`; see the fake `ssh` and `herdr`
/// wrappers). A knob file holding a number applies that many times, then removes itself.
pub mod knob {
    pub const FAIL255: &str = "fail255";
    /// Like `FAIL255`, but ssh reports a host key failure.
    pub const HOSTKEY255: &str = "hostkey255";
    /// Leg dies before its ready marker (never answers).
    pub const HANG: &str = "hang";
    /// Leg prints the ready marker and then neither answers HELLO nor exits.
    pub const STALL_WELCOME: &str = "stall-welcome";
    /// Leg prints the ready marker and exits 0 (EOF before WELCOME).
    pub const EOF_WELCOME: &str = "eof-welcome";
    /// Like `EOF_WELCOME`, but only for reconnect legs (leg 0 is left alone).
    pub const EOF_RECONNECT: &str = "eof-reconnect";
    /// Leg answers with the ready marker and the END frame armed by `arm_fake_end`.
    pub const FAKE_END: &str = "fake-end";
    /// `status client --json` no longer lists the resume capability.
    pub const STRIP_CAPABILITY: &str = "strip-capability";
    /// The remote rejects `--x-mosh-session-v1` like a binary that never learned it.
    pub const DOWNGRADE: &str = "downgrade";
    /// The attacher cannot spawn the keeper.
    pub const SPAWN_FAIL: &str = "spawn-fail";
    /// Session legs and plain bridges run through a ~1.3 MB/s throttle in both directions.
    pub const THROTTLE: &str = "throttle";
    /// Session legs and plain bridges started while set run both directions through the
    /// delay line (`<one-way delay ms> <rate bytes/s>`); see `Remote::set_link`. Checked
    /// before `THROTTLE`.
    pub const LINK: &str = "link";
    /// The fake `ssh -G` also reports `hostname 127.0.0.1`, so `start_ui_bridge` registers the
    /// QUIC leg dialer. Without it existing suites keep plain ssh legs.
    pub const QUIC: &str = "quic";
    /// Space-separated capability names the fake `herdr` wrapper removes from
    /// `status client --json`, like `STRIP_CAPABILITY` does for the resume capability.
    pub const STRIP_CAPS: &str = "strip-caps";
    /// A UDP port. Session legs started while it is set pass their stdout through
    /// [`QUIC_PORT_FILTER`], which rewrites the port field of the attacher's bootstrap line.
    pub const QUIC_PORT: &str = "quic-port";
    /// Every session leg ends like an unreachable host (`ssh` exit 255, "Connection timed
    /// out": retryable on a reconnect leg), as a network that drops all traffic does.
    pub const SSH_DOWN: &str = "ssh-down";
}

/// Perl filter the fake `ssh` puts on a session leg's stdout while the `quic-port` knob is set
/// (`perl <path> <port> <command...>`): runs the command, rewrites the port field of the first
/// `herdr-mosh-quic:1 <port> ...` line to `<port>` (a `none` line passes untouched), and then
/// copies bytes raw. Production code gets no address override; this is the test-only seam.
/// Bytes up to and including the ready marker pass at once (the dialer waits for it); when no
/// bootstrap line follows the marker the stream is copied raw from the first non-matching byte.
const QUIC_PORT_FILTER: &str = r##"use strict;
use warnings;

my ($port, @cmd) = @ARGV;
my $MARK = "herdr-remote-output-ready:1\n";
my $PFX = "herdr-mosh-quic:1";
pipe(my $r, my $w) or die "pipe: $!";
my $pid = fork();
die "fork: $!" unless defined $pid;
if (!$pid) {
    close $r;
    open(STDOUT, '>&', $w) or die "dup: $!";
    close $w;
    exec @cmd or die "exec: $!";
}
close $w;
for my $sig (qw(TERM HUP INT)) {
    $SIG{$sig} = sub { kill 'KILL', $pid; exit 143 };
}
binmode STDOUT;
binmode $r;

sub out {
    my ($data) = @_;
    while (length $data) {
        my $n = syswrite(STDOUT, $data);
        if (!defined $n) {
            next if $!{EINTR};
            kill 'KILL', $pid;
            exit 0;
        }
        substr($data, 0, $n, '');
    }
}

sub rewrite {
    my ($line) = @_;
    $line =~ s/^(\Q$PFX\E )\d+ /$1$port /;
    return $line;
}

my $buf = '';
my $state = 0;    # 0: preamble lines, 1: after the marker, 2: raw
while (1) {
    my $n = sysread($r, my $chunk, 65536);
    if (!defined $n) {
        next if $!{EINTR};
        last;
    }
    last if $n == 0;
    if ($state == 2) {
        out($chunk);
        next;
    }
    $buf .= $chunk;
    while ($state < 2) {
        if ($state == 0) {
            my $i = index($buf, "\n");
            last if $i < 0;
            my $line = substr($buf, 0, $i + 1, '');
            if (index($line, $PFX) == 0) {
                out(rewrite($line));
                $state = 2;
            } else {
                out($line);
                $state = 1 if $line eq $MARK;
            }
        } else {
            my $len = length $buf;
            last if $len == 0;
            my $cmp = $len < length($PFX) ? $len : length($PFX);
            if (substr($buf, 0, $cmp) ne substr($PFX, 0, $cmp)) {
                $state = 2;
            } elsif ($len < length($PFX)) {
                last;
            } else {
                my $i = index($buf, "\n");
                if ($i < 0) {
                    $state = 2 if $len > 4096;
                    last;
                }
                out(rewrite(substr($buf, 0, $i + 1, '')));
                $state = 2;
            }
        }
    }
    if ($state == 2 && length $buf) {
        out($buf);
        $buf = '';
    }
}
out($buf) if length $buf;
waitpid($pid, 0);
exit($? >> 8);
"##;

/// Perl delay line the fake `ssh` puts on both directions of a leg while the `link` knob is
/// set: a serializing bottleneck plus propagation delay. Each chunk read at `t` departs at
/// `max(t, previous departure) + len / rate` and is written at `departure + delay`, so every
/// chunk pays serialization, idle line or not. Single threaded (`select`), in-memory queue,
/// no loss.
const DELAY_LINE: &str = r##"use strict;
use warnings;
use Fcntl qw(F_GETFL F_SETFL O_NONBLOCK);
use Time::HiRes qw(time);

my ($delay_ms, $rate) = @ARGV;
die "usage: delay-line.pl <one-way delay ms> <rate bytes/s>\n"
    unless defined $rate && $rate > 0;
my $delay = $delay_ms / 1000;
binmode STDIN;
binmode STDOUT;
my $flags = fcntl(STDOUT, F_GETFL, 0) or die "fcntl: $!";
fcntl(STDOUT, F_SETFL, $flags | O_NONBLOCK) or die "fcntl: $!";

my @queue;            # [deliver_at, bytes], in departure order
my $prev_depart = 0;
my $pending = '';     # due bytes STDOUT has not taken yet
my $eof = 0;
while (1) {
    my $now = time;
    while (@queue && $queue[0][0] <= $now) {
        $pending .= (shift @queue)->[1];
    }
    if (length $pending) {
        my $n = syswrite(STDOUT, $pending);
        if (defined $n) {
            substr($pending, 0, $n, '');
        } elsif (!$!{EAGAIN} && !$!{EINTR}) {
            exit 0;    # the reader is gone
        }
    }
    last if $eof && !@queue && !length $pending;
    my ($rin, $win) = ('', '');
    vec($rin, fileno(STDIN), 1) = 1 unless $eof;
    vec($win, fileno(STDOUT), 1) = 1 if length $pending;
    my $timeout;
    if (@queue) {
        $timeout = $queue[0][0] - time;
        $timeout = 0 if $timeout < 0;
    }
    my ($rout, $wout) = ($rin, $win);
    my $ready = select($rout, $wout, undef, $timeout);
    next if $ready <= 0;
    if (!$eof && vec($rout, fileno(STDIN), 1)) {
        my $n = sysread(STDIN, my $buf, 16384);
        if (!defined $n) {
            $eof = 1 unless $!{EINTR} || $!{EAGAIN};
        } elsif ($n == 0) {
            $eof = 1;
        } else {
            my $t = time;
            my $start = $t > $prev_depart ? $t : $prev_depart;
            $prev_depart = $start + $n / $rate;
            push @queue, [$prev_depart + $delay, $buf];
        }
    }
}
close STDOUT;
"##;

/// Write the delay-line script into `dir` and return its path; run it as
/// `perl <path> <one-way delay ms> <rate bytes/s>`.
pub fn write_delay_line(dir: &Path) -> PathBuf {
    let path = dir.join("delay-line.pl");
    fs::write(&path, DELAY_LINE).unwrap();
    path
}

/// Write the bootstrap-port filter into `dir` and return its path; run it as
/// `perl <path> <port> <command...>`.
pub fn write_quic_port_filter(dir: &Path) -> PathBuf {
    let path = dir.join("quic-port-filter.pl");
    fs::write(&path, QUIC_PORT_FILTER).unwrap();
    path
}

/// The port field of a `herdr-mosh-quic:1 <port> <fingerprint> <token>` line (`None` for the
/// `none` line and anything malformed), parsed with the production parser.
pub fn bootstrap_line_port(line: &str) -> Option<u16> {
    quic_bootstrap::QuicBootstrap::parse_line(line).map(|b| b.port)
}

/// The fingerprint field of a bootstrap line, as lowercase hex.
pub fn bootstrap_line_fingerprint(line: &str) -> Option<String> {
    quic_bootstrap::QuicBootstrap::parse_line(line).map(|b| b.fingerprint.to_string())
}

/// A disposable remote herdr server plus the fake `ssh` that reaches it.
pub struct Remote {
    pub base: PathBuf,
    pub api: PathBuf,
    pub workspace: String,
    pub pane: String,
    server: Option<Spawned>,
    next_client: AtomicU32,
    /// Every keeper pid ever observed, so teardown does not depend on a final scan.
    seen_keepers: Mutex<BTreeSet<libc::pid_t>>,
}

impl Remote {
    pub fn start() -> Remote {
        let n = BASE_COUNTER.fetch_add(1, Ordering::SeqCst);
        // Short fixed base: a long runner TMPDIR would push the keeper socket path past
        // sun_path and make every keeper share the host's /tmp/herdr-mosh-<euid>.
        let base = PathBuf::from("/tmp").join(format!("hm{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for dir in [
            "bin",
            "knobs",
            "trace/keeper",
            "remote/home",
            "remote/config",
            "remote/runtime",
            "remote/tmp",
        ] {
            fs::create_dir_all(base.join(dir)).unwrap();
        }
        fs::create_dir_all(base.join("remote/config").join(app_dir_name())).unwrap();
        fs::write(
            base.join("remote/config")
                .join(app_dir_name())
                .join("config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        let mut remote = Remote {
            api: base.join("remote/runtime/herdr.sock"),
            base,
            workspace: String::new(),
            pane: String::new(),
            server: None,
            next_client: AtomicU32::new(1),
            seen_keepers: Mutex::new(BTreeSet::new()),
        };
        // Panics when the rendezvous dir would not be the one the keeper uses.
        let _ = remote.rendezvous_dir();
        remote.write_wrappers();
        remote.start_server();
        let created = remote.request(json!({
            "id": "ws", "method": "workspace.create",
            "params": {"cwd": remote.base, "focus": true, "label": "mosh"},
        }));
        remote.workspace = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap_or_else(|| panic!("workspace.create: {created}"))
            .to_owned();
        remote.pane = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("workspace.create: {created}"))
            .to_owned();
        remote
    }

    fn remote_dir(&self, name: &str) -> PathBuf {
        self.base.join("remote").join(name)
    }

    /// The remote tmp dir (`TMPDIR` of everything the fake ssh starts): keeper rendezvous root.
    pub fn remote_tmp(&self) -> PathBuf {
        self.remote_dir("tmp")
    }

    /// The keeper rendezvous directory for this remote's environment, mirroring
    /// `rendezvous::resolve`: `<TMPDIR>/herdr-mosh-<euid>` when a keeper socket in it fits
    /// `sun_path` (103 bytes). Otherwise the keeper would use the shared
    /// `/tmp/herdr-mosh-<euid>` and a test would be looking at the wrong directory, so panic.
    pub fn rendezvous_dir(&self) -> PathBuf {
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        let dir = self.remote_tmp().join(format!("herdr-mosh-{euid}"));
        let sock = dir.join("k-000000000000000000000000.sock");
        assert!(
            sock.as_os_str().len() <= 103,
            "keeper socket path would not fit sun_path: {}",
            sock.display()
        );
        dir
    }

    /// The keeper's socket as observed on disk: the single `k-*.sock` in the rendezvous dir.
    pub fn keeper_socket(&self) -> Option<PathBuf> {
        let mut socks: Vec<PathBuf> = fs::read_dir(self.rendezvous_dir())
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("k-") && n.ends_with(".sock"))
            })
            .collect();
        assert!(socks.len() <= 1, "several keeper sockets: {socks:?}");
        socks.pop()
    }

    /// Lines `MKtag` appended to `marks` in the pane's directory by commands typed through
    /// [`Client::type_mark`].
    pub fn marks(&self) -> Vec<String> {
        fs::read_to_string(self.base.join("marks"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Keep typing `MKtag` through `client` until it shows up in `marks` (input is dropped
    /// while a saved machine is still activating).
    pub fn wait_mark(&self, client: &mut Client, tag: &str, timeout: Duration) -> bool {
        wait_until(timeout, Duration::from_millis(500), || {
            if self.marks().contains(&format!("MK{tag}")) {
                return true;
            }
            client.type_mark(tag);
            false
        })
    }

    pub fn trace_dir(&self) -> PathBuf {
        self.base.join("trace")
    }

    fn write_wrappers(&self) {
        let base = &self.base;
        let env = |name: &str| sh_quote(&self.remote_dir(name));
        let knobs = sh_quote(&base.join("knobs"));
        let argv = sh_quote(&base.join("ssh-argv"));
        let legs = sh_quote(&base.join("legs.log"));
        let delay_line = sh_quote(&write_delay_line(&base.join("bin")));
        let links = sh_quote(&base.join("links.log"));
        let port_filter = sh_quote(&write_quic_port_filter(&base.join("bin")));
        let marker = "printf '\\nherdr-remote-output-ready:1\\n'";
        let throttle = "perl -e '$|=1; while (my $n = sysread(STDIN,$b,65536)) { syswrite(STDOUT,$b); select(undef,undef,undef,$n/1300000); }'";
        write_executable(
            &base.join("bin/ssh"),
            &format!(
                r#"#!/bin/sh
export HOME={home} XDG_CONFIG_HOME={config} XDG_RUNTIME_DIR={runtime} XDG_STATE_HOME={runtime}/state HERDR_SOCKET_PATH={api} TMPDIR={tmp} HERDR_MOSH_TRACE={trace} SHELL=/bin/sh
unset HERDR_CLIENT_SOCKET_PATH HERDR_SESSION HERDR_MOSH_GRACE HERDR_MOSH_RESUME HERDR_ENV
if [ -n "$FAKE_SSH_REMOTE_GRACE" ]; then export HERDR_MOSH_GRACE="$FAKE_SSH_REMOTE_GRACE"; fi
K={knobs}
take() {{
  f="$K/$1"; [ -e "$f" ] || return 1
  n=$(cat "$f")
  if [ -n "$n" ]; then if [ "$n" -le 1 ]; then rm -f "$f"; else echo $((n-1)) > "$f"; fi; fi
  return 0
}}
control=; previous=; configuration=
for arg do
  if [ "$previous" = -S ]; then control=$arg; fi
  if [ "$arg" = -G ]; then configuration=yes; fi
  previous=$arg; last=$arg
done
if [ -n "$configuration" ]; then
  printf 'controlpath %s\n' "$control"
  if [ -e "$K/quic" ]; then printf 'hostname 127.0.0.1\n'; fi
  exit 0
fi
printf '%s %s\n' "$FAKE_SSH_CLIENT" "$(printf '%s' "$*" | tr '\n' ' ')" >> {argv}
kind=other
case "$last" in
  *--x-mosh-session-v1*) kind=session ;;
  *remote-client-bridge*) kind=plain ;;
esac
if [ "$kind" = session ]; then
  printf '%s %s\n' "$FAKE_SSH_CLIENT" "$$" >> {legs}
  if [ -e "$K/ssh-down" ]; then echo 'ssh: connect to host test-only port 22: Connection timed out' >&2; exit 255; fi
  if take fail255; then echo 'Permission denied (publickey).' >&2; exit 255; fi
  if take hostkey255; then echo 'Host key verification failed.' >&2; exit 255; fi
  if take hang; then exec sleep 600; fi
  if take stall-welcome; then {marker}; exec sleep 600; fi
  if take eof-welcome; then {marker}; exit 0; fi
  case "$last$*" in
    *ControlPath=none*) if take eof-reconnect; then {marker}; exit 0; fi ;;
  esac
  if take fake-end; then {marker}; cat "$K/fake-end.bytes"; exit 0; fi
fi
if [ "$kind" = session ] && [ -e "$K/quic-port" ]; then
  exec perl {port_filter} "$(cat "$K/quic-port")" /bin/sh -c "$last"
fi
if [ "$kind" != other ] && [ -e "$K/link" ]; then
  read -r delay rate < "$K/link"
  printf '%s %s %s %s %s\n' "$FAKE_SSH_CLIENT" "$kind" "$$" "$delay" "$rate" >> {links}
  perl {delay_line} "$delay" "$rate" | /bin/sh -c "$last" | perl {delay_line} "$delay" "$rate"
  exit $?
fi
if [ "$kind" != other ] && [ -e "$K/throttle" ]; then
  {throttle} | /bin/sh -c "$last" | {throttle}
  exit $?
fi
exec /bin/sh -c "$last"
"#,
                home = env("home"),
                config = env("config"),
                runtime = env("runtime"),
                api = sh_quote(&self.api),
                tmp = env("tmp"),
                trace = sh_quote(&self.trace_dir().join("keeper")),
                knobs = knobs,
                argv = argv,
                legs = legs,
                marker = marker,
                throttle = throttle,
                delay_line = delay_line,
                links = links,
                port_filter = port_filter,
            ),
        );
        write_executable(
            &base.join("bin/herdr"),
            &format!(
                r#"#!/bin/sh
K={knobs}
REAL={real}
case "$*" in
  *status\ client*--json*)
    if [ -e "$K/strip-caps" ]; then
      script=
      for cap in $(cat "$K/strip-caps"); do script="$script;s/$cap/x-herdr-test-removed/g"; done
      "$REAL" "$@" | sed "${{script#;}}"
      exit $?
    fi
    if [ -e "$K/strip-capability" ]; then
      "$REAL" "$@" | sed 's/x-mosh-resume-bridge-v1/x-herdr-test-removed/g'
      exit $?
    fi ;;
  *--x-mosh-session-v1*)
    if [ -e "$K/downgrade" ]; then echo 'unsupported remote client bridge option' >&2; exit 2; fi
    if [ -e "$K/spawn-fail" ]; then
      if ulimit -S -u 1 2>/dev/null || ulimit -S -p 1 2>/dev/null; then : > "$K/spawn-fail.applied"; fi
    fi ;;
esac
exec "$REAL" "$@"
"#,
                knobs = knobs,
                real = sh_quote(Path::new(env!("CARGO_BIN_EXE_herdr"))),
            ),
        );
    }

    pub fn start_server(&mut self) {
        assert!(self.server.is_none(), "server already running");
        let pair = open_pty(SCREEN_COLS, SCREEN_ROWS);
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        cmd.arg("server");
        cmd.env_clear();
        cmd.env("PATH", std::env::var("PATH").unwrap_or_default());
        cmd.env("HOME", self.remote_dir("home"));
        cmd.env("TMPDIR", self.remote_tmp());
        cmd.env("XDG_CONFIG_HOME", self.remote_dir("config"));
        cmd.env("XDG_RUNTIME_DIR", self.remote_dir("runtime"));
        cmd.env("HERDR_SOCKET_PATH", &self.api);
        cmd.env("SHELL", "/bin/sh");
        cmd.env("HERDR_DISABLE_SOUND", "1");
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        self.server = Some(Spawned {
            _master: Some(pair.master),
            child,
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        while !(self.api.exists() && UnixStream::connect(&self.api).is_ok()) {
            assert!(
                Instant::now() < deadline,
                "remote server socket never appeared"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// Hard-stop the remote server (its panes get SIGHUP via the closed pty).
    pub fn stop_server(&mut self) {
        if let Some(mut server) = self.server.take() {
            server.kill_group();
        }
    }

    pub fn request(&self, request: Value) -> Value {
        let mut stream = UnixStream::connect(&self.api).expect("connect remote API");
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad API reply {line:?}: {e}"))
    }

    /// Recent pane text (scrollback and screen) of the first pane.
    pub fn pane_text(&self) -> String {
        self.pane_text_of(&self.pane)
    }

    pub fn pane_text_of(&self, pane: &str) -> String {
        let reply = self.request(json!({
            "id": "read", "method": "pane.read",
            "params": {"pane_id": pane, "source": "recent", "lines": 2000},
        }));
        reply["result"]["read"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// Run a shell command in the first pane through the API (not through any client).
    pub fn pane_run(&self, command: &str) {
        let reply = self.request(json!({
            "id": "run", "method": "pane.send_input",
            "params": {"pane_id": self.pane, "text": command, "keys": ["Enter"]},
        }));
        assert_eq!(reply["result"]["type"], "ok", "{reply}");
    }

    pub fn tab_count(&self) -> usize {
        let reply = self.request(json!({
            "id": "tabs", "method": "tab.list",
            "params": {"workspace_id": self.workspace},
        }));
        reply["result"]["tabs"].as_array().map_or(0, Vec::len)
    }

    // ---- knobs and ssh logs -----------------------------------------------------------

    pub fn set_knob(&self, name: &str, content: &str) {
        fs::write(self.base.join("knobs").join(name), content).unwrap();
    }

    pub fn clear_knob(&self, name: &str) {
        let _ = fs::remove_file(self.base.join("knobs").join(name));
    }

    /// Route session legs and plain bridges started from now on through the delay line:
    /// `one_way_delay_ms` each way and `rate` bytes/s of serialization. Running legs keep
    /// their path.
    pub fn set_link(&self, one_way_delay_ms: u64, rate: u64) {
        assert!(rate > 0, "link rate must be positive");
        self.set_knob(knob::LINK, &format!("{one_way_delay_ms} {rate}\n"));
    }

    /// Legs started from now on go direct again.
    pub fn clear_link(&self) {
        self.clear_knob(knob::LINK);
    }

    /// `<kind> <ssh pid> <delay ms> <rate>` for every leg or bridge of `client` that went
    /// through the delay line (`kind` is `session` or `plain`), in start order.
    pub fn linked_legs(&self, client: &str) -> Vec<String> {
        let prefix = format!("{client} ");
        fs::read_to_string(self.base.join("links.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix))
            .map(str::to_owned)
            .collect()
    }

    /// Argument lines (`$*`) of every ssh the given client ran, in order.
    pub fn ssh_argv(&self, client: &str) -> Vec<String> {
        let prefix = format!("{client} ");
        fs::read_to_string(self.base.join("ssh-argv"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
            .collect()
    }

    pub fn session_legs(&self, client: &str) -> Vec<String> {
        self.ssh_argv(client)
            .into_iter()
            .filter(|line| line.contains("--x-mosh-session-v1"))
            .collect()
    }

    pub fn plain_bridges(&self, client: &str) -> Vec<String> {
        self.ssh_argv(client)
            .into_iter()
            .filter(|line| line.contains("remote-client-bridge") && !line.contains("--x-mosh-"))
            .collect()
    }

    /// Pids of every session leg the client started, oldest first (the attacher's pid: the
    /// fake ssh `exec`s into it).
    pub fn leg_pids(&self, client: &str) -> Vec<libc::pid_t> {
        let prefix = format!("{client} ");
        fs::read_to_string(self.base.join("legs.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix(&prefix)?.trim().parse().ok())
            .collect()
    }

    pub fn wait_leg_count(&self, client: &str, count: usize, timeout: Duration) -> bool {
        wait_for(timeout, || self.leg_pids(client).len() >= count)
    }

    pub fn current_leg(&self, client: &str) -> libc::pid_t {
        *self
            .leg_pids(client)
            .last()
            .unwrap_or_else(|| panic!("client {client} started no leg"))
    }

    /// Signal the client's newest leg.
    pub fn signal_leg(&self, client: &str, signal: libc::c_int) {
        let pid = self.current_leg(client);
        // SAFETY: plain kill(2) on a process this harness started.
        let rc = unsafe { libc::kill(pid, signal) };
        assert_eq!(rc, 0, "kill({pid}, {signal}) failed");
    }

    // ---- traces --------------------------------------------------------------------

    /// Keeper trace lines (`keeper.jsonl`).
    pub fn keeper_events(&self) -> Vec<Value> {
        read_jsonl(&self.trace_dir().join("keeper/keeper.jsonl"))
    }

    pub fn keeper_event_count(&self, event: &str) -> usize {
        self.keeper_events()
            .iter()
            .filter(|line| line["event"] == event)
            .count()
    }

    pub fn wait_keeper_event(&self, event: &str, count: usize, timeout: Duration) -> bool {
        wait_for(timeout, || self.keeper_event_count(event) >= count)
    }

    // ---- processes -------------------------------------------------------------------

    /// Keeper daemons belonging to this remote (command line has the keeper flag and the
    /// environment names this base directory). Every pid returned is remembered for teardown.
    pub fn keeper_pids(&self) -> Vec<libc::pid_t> {
        let pids = self.scan_keepers();
        self.seen_keepers
            .lock()
            .unwrap()
            .extend(pids.iter().copied());
        pids
    }

    #[cfg(target_os = "linux")]
    fn scan_keepers(&self) -> Vec<libc::pid_t> {
        // Trailing slash: `hm1-1` must not match the environment of `hm1-10`.
        let needle = format!("{}/", self.base.display());
        let mut pids = Vec::new();
        let Ok(entries) = fs::read_dir("/proc") else {
            return pids;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            if !String::from_utf8_lossy(&cmdline).contains("--x-mosh-keeper-v1") {
                continue;
            }
            let Ok(environ) = fs::read(entry.path().join("environ")) else {
                continue;
            };
            if String::from_utf8_lossy(&environ).contains(&needle) {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        pids
    }

    /// `ps` lines (`pid command`, environment appended by `-E` on macOS) of this remote's
    /// keepers. Unverified on a Linux host.
    #[cfg(not(target_os = "linux"))]
    fn keeper_ps_lines(&self) -> Vec<(libc::pid_t, String)> {
        let needle = format!("{}/", self.base.display());
        let Ok(out) = std::process::Command::new("ps")
            .args(["-A", "-ww", "-E", "-o", "pid=,command="])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|line| line.contains("--x-mosh-keeper-v1") && line.contains(&needle))
            .filter_map(|line| {
                let line = line.trim_start();
                let pid = line.split_whitespace().next()?.parse().ok()?;
                Some((pid, line.to_owned()))
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    fn scan_keepers(&self) -> Vec<libc::pid_t> {
        let mut pids: Vec<_> = self.keeper_ps_lines().into_iter().map(|(p, _)| p).collect();
        pids.sort_unstable();
        pids
    }

    /// `pid ppid state cmdline` of each keeper, for assertion messages.
    #[cfg(target_os = "linux")]
    pub fn keeper_details(&self) -> Vec<String> {
        self.keeper_pids()
            .into_iter()
            .map(|pid| {
                let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                let after = stat
                    .rsplit_once(')')
                    .map_or("", |(_, rest)| rest)
                    .trim()
                    .to_owned();
                let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                format!(
                    "{pid} [{after:.20}] {}",
                    String::from_utf8_lossy(&cmdline).replace('\0', " ")
                )
            })
            .collect()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn keeper_details(&self) -> Vec<String> {
        let lines = self.keeper_ps_lines();
        self.seen_keepers
            .lock()
            .unwrap()
            .extend(lines.iter().map(|(p, _)| *p));
        lines.into_iter().map(|(_, line)| line).collect()
    }

    pub fn server_pid(&self) -> Option<u32> {
        self.server.as_ref().and_then(Spawned::pid)
    }

    // ---- clients ---------------------------------------------------------------------

    fn client_dirs(&self, id: &str) -> ClientDirs {
        let root = self.base.join(id);
        let dirs = ClientDirs {
            home: root.join("home"),
            config: root.join("config"),
            runtime: root.join("runtime"),
            tmp: root.join("tmp"),
            trace: self.trace_dir().join(id),
        };
        for dir in [
            &dirs.home,
            &dirs.config,
            &dirs.runtime,
            &dirs.tmp,
            &dirs.trace,
        ] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::create_dir_all(dirs.config.join(app_dir_name())).unwrap();
        fs::write(
            dirs.config.join(app_dir_name()).join("config.toml"),
            "onboarding = false\n",
        )
        .unwrap();
        dirs
    }

    /// `herdr --remote test-only` in a pty.
    pub fn spawn_remote_client(&self, opts: &ClientOptions) -> Client {
        self.spawn_client(&["--remote", "test-only"], opts, false)
    }

    /// `herdr client` with `test-only` as the one saved machine.
    pub fn spawn_saved_client(&self, opts: &ClientOptions) -> Client {
        self.spawn_client(&["client"], opts, true)
    }

    fn spawn_client(&self, args: &[&str], opts: &ClientOptions, saved: bool) -> Client {
        let id = format!("c{}", self.next_client.fetch_add(1, Ordering::SeqCst));
        let dirs = self.client_dirs(&id);
        if saved {
            let catalog_dir = dirs
                .runtime
                .join("state")
                .join(app_dir_name())
                .join("client");
            fs::create_dir_all(&catalog_dir).unwrap();
            let profile = "0123456789abcdef0123456789abcdef";
            fs::write(
                catalog_dir.join("endpoints.json"),
                json!({
                    "version": 1, "selected_profile": profile,
                    "ssh": [{"id": profile, "label": "Test remote", "target": "test-only",
                             "session": "default", "enabled": true}],
                })
                .to_string(),
            )
            .unwrap();
        }
        let (cols, rows) = opts.size;
        let pair = open_pty(cols, rows);
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
        cmd.args(args);
        cmd.env_clear();
        let path = format!(
            "{}:{}",
            self.base.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        cmd.env("PATH", path);
        cmd.env("HOME", &dirs.home);
        cmd.env("TMPDIR", &dirs.tmp);
        cmd.env("XDG_CONFIG_HOME", &dirs.config);
        cmd.env("XDG_RUNTIME_DIR", &dirs.runtime);
        cmd.env("XDG_STATE_HOME", dirs.runtime.join("state"));
        cmd.env("HERDR_SOCKET_PATH", dirs.runtime.join("herdr.sock"));
        cmd.env("HERDR_DISABLE_SOUND", "1");
        cmd.env("SHELL", "/bin/sh");
        cmd.env("TERM", "xterm-256color");
        cmd.env("FAKE_SSH_CLIENT", &id);
        cmd.env("HERDR_MOSH_TRACE", &dirs.trace);
        if let Some(grace) = opts.grace_secs {
            cmd.env("HERDR_MOSH_GRACE", grace.to_string());
        }
        if !opts.resume {
            cmd.env("HERDR_MOSH_RESUME", "0");
        }
        for (key, value) in &opts.env {
            cmd.env(key, value);
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let output: Arc<Mutex<Vec<u8>>> = Arc::default();
        let sink = Arc::clone(&output);
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = std::io::Read::read(&mut reader, &mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .extend_from_slice(&buf[..n]);
            }
        });
        Client {
            id,
            size: opts.size,
            dirs,
            spawned: Spawned {
                _master: Some(pair.master),
                child,
            },
            writer,
            output,
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // A live handoff leaves a replacement server outside our process group; ask whichever
        // server owns the API socket to stop and wait for it to go away.
        let _ = UnixStream::connect(&self.api).and_then(|mut s| {
            writeln!(s, r#"{{"id":"stop","method":"server.stop","params":{{}}}}"#)
        });
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            UnixStream::connect(&self.api).is_err()
        });
        let mut doomed = self.keeper_pids();
        doomed.extend(self.seen_keepers.lock().unwrap().iter().copied());
        for pid in doomed {
            // SAFETY: pid was verified by command line and environment to be this remote's
            // keeper; killing one that already exited is harmless within a short test.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        if let Some(mut server) = self.server.take() {
            server.kill_group();
        }
        debug_assert!(self.rendezvous_dir().starts_with(&self.base));
        for _ in 0..3 {
            if fs::remove_dir_all(&self.base).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

struct ClientDirs {
    home: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
    tmp: PathBuf,
    trace: PathBuf,
}

pub struct ClientOptions {
    /// `HERDR_MOSH_GRACE` seconds; `None` leaves the default.
    pub grace_secs: Option<u32>,
    /// `false` sets the `HERDR_MOSH_RESUME=0` kill switch.
    pub resume: bool,
    pub env: Vec<(String, String)>,
    /// Terminal size `(cols, rows)`; bigger screens mean bigger frames from the server.
    pub size: (u16, u16),
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            grace_secs: Some(60),
            resume: true,
            env: Vec::new(),
            size: (SCREEN_COLS, SCREEN_ROWS),
        }
    }
}

impl ClientOptions {
    pub fn grace(secs: u32) -> Self {
        ClientOptions {
            grace_secs: Some(secs),
            ..Default::default()
        }
    }
}

/// A herdr client process in a pty; killed with its process group (its ssh legs) on drop.
pub struct Client {
    pub id: String,
    size: (u16, u16),
    dirs: ClientDirs,
    spawned: Spawned,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Client {
    pub fn output_bytes(&self) -> Vec<u8> {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn output_len(&self) -> usize {
        self.output.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// The emulated visible screen.
    pub fn screen(&self) -> String {
        terminal_screen::text(&self.output_bytes(), self.size.0, self.size.1)
    }

    /// The emulated visible screen after only the first `len` output bytes (a stopwatch can
    /// find the first output prefix that shows something without paying parse time per poll).
    pub fn screen_prefix(&self, len: usize) -> String {
        let output = self.output_bytes();
        terminal_screen::text(&output[..len.min(output.len())], self.size.0, self.size.1)
    }

    /// Everything the client wrote to its terminal, lossily decoded (includes text printed
    /// after the TUI left the alternate screen).
    pub fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output_bytes()).into_owned()
    }

    pub fn type_keys(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// Type a command that appends `MKtag` to `marks` in the pane's directory.
    pub fn type_mark(&mut self, tag: &str) {
        self.type_keys(&format!("printf 'MK%s\\n' {tag} >> marks\r"));
    }

    pub fn type_bytes(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    pub fn wait_screen(&self, needle: &str, timeout: Duration) -> bool {
        wait_for(timeout, || self.screen().contains(needle))
    }

    /// Local trace lines (`local.jsonl`).
    pub fn events(&self) -> Vec<Value> {
        read_jsonl(&self.dirs.trace.join("local.jsonl"))
    }

    pub fn event_count(&self, event: &str) -> usize {
        self.events()
            .iter()
            .filter(|line| line["event"] == event)
            .count()
    }

    pub fn wait_event(&self, event: &str, count: usize, timeout: Duration) -> bool {
        wait_for(timeout, || self.event_count(event) >= count)
    }

    /// `true` once a `welcome{resumed}` follows the first `leg_down`.
    pub fn has_welcome_after_resume(&self, timeout: Duration) -> bool {
        wait_for(timeout, || self.has_welcome("resumed"))
    }

    pub fn has_welcome(&self, status: &str) -> bool {
        self.events()
            .iter()
            .any(|line| line["event"] == "welcome" && line["status"] == status)
    }

    pub fn pid(&self) -> Option<u32> {
        self.spawned.pid()
    }

    /// Wait until the client shows remote output and its first leg is up.
    pub fn wait_attached(&self, remote: &Remote, timeout: Duration) {
        let marker = format!("READY_{}", self.id.to_uppercase());
        remote.pane_run(&format!("printf 'READY_%s\\n' {}", self.id.to_uppercase()));
        assert!(
            self.wait_screen(&marker, timeout),
            "client never showed remote output: {}",
            self.screen()
        );
    }

    /// `true` once the client process has exited (within `timeout`).
    pub fn wait_exit(&mut self, timeout: Duration) -> bool {
        wait_for(timeout, || {
            matches!(self.spawned.child.try_wait(), Ok(Some(_)))
        })
    }

    pub fn exited(&mut self) -> bool {
        matches!(self.spawned.child.try_wait(), Ok(Some(_)))
    }

    pub fn dirs_runtime(&self) -> &Path {
        &self.dirs.runtime
    }

    pub fn home(&self) -> &Path {
        &self.dirs.home
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.spawned.kill_group();
    }
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Encoded session `END{reason}` frame (`[6][u32 LE len][json]`), for the `fake-end` knob.
pub fn encode_end(reason: &str) -> Vec<u8> {
    let payload = json!({ "reason": reason }).to_string().into_bytes();
    let mut out = vec![6u8];
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

impl Remote {
    /// Arm the `fake-end` knob: session legs answer HELLO with `END{reason}` instead of
    /// reaching a keeper (a keeper of another protocol version, or one that cannot reach the
    /// server).
    pub fn arm_fake_end(&self, reason: &str) {
        fs::write(self.base.join("knobs/fake-end.bytes"), encode_end(reason)).unwrap();
        self.set_knob(knob::FAKE_END, "");
    }
}

fn pid_alive(pid: libc::pid_t) -> bool {
    if cfg!(target_os = "linux") {
        // A zombie still answers kill(pid, 0); its state letter says it is gone.
        return fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .and_then(|(_, rest)| rest.trim().chars().next())
                .is_some_and(|state| state != 'Z' && state != 'X')
        });
    }
    // SAFETY: signal 0 only checks that the pid exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

impl Remote {
    /// Number of session-leg ssh processes of `client` (from `legs.log`) still alive: the
    /// attacher the fake ssh `exec`s into, or the port filter wrapping it.
    pub fn ssh_session_children_alive(&self, client: &str) -> usize {
        self.leg_pids(client)
            .into_iter()
            .filter(|pid| pid_alive(*pid))
            .count()
    }

    /// Run the remote attacher once with the QUIC flag through the fake ssh (stdin closed, so
    /// it exits without opening a session connection) and return its bootstrap line
    /// (`herdr-mosh-quic:1 ...`, without the newline). The line goes through the port filter
    /// while the `quic-port` knob is set. Starts the keeper if none runs, and its acceptor.
    pub fn quic_bootstrap_line(&self) -> String {
        use std::process::{Command, Stdio};
        let herdr = sh_quote(&self.base.join("bin/herdr"));
        let mut child = Command::new(self.base.join("bin/ssh"))
            .args(["-T", "test-only"])
            .arg(format!(
                "exec {herdr} remote-client-bridge --x-mosh-quic-v1 --x-mosh-session-v1"
            ))
            .env("FAKE_SSH_CLIENT", "prime")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the priming attacher");
        let stdout = child.stdout.take().expect("attacher stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.starts_with("herdr-mosh-quic:1") {
                    let _ = tx.send(line);
                    return;
                }
            }
        });
        let line = rx.recv_timeout(Duration::from_secs(15));
        let _ = child.kill();
        let _ = child.wait();
        line.expect("the priming attacher printed no bootstrap line")
    }

    /// Prime the keeper's acceptor and route QUIC through a [`UdpRelay`]: marks the remote as
    /// QUIC-capable for `ssh -G` (`knob::QUIC`), reads the keeper's real bootstrap line, starts
    /// a relay to that UDP port, and sets `knob::QUIC_PORT` to the relay's client-facing port,
    /// so every session leg started from now on reports the relay as the keeper's address. A
    /// keeper with no session exits after 30 s idle, so start the client right after.
    pub fn start_quic_relay(&self, seed: u64) -> UdpRelay {
        self.clear_knob(knob::QUIC_PORT);
        self.set_knob(knob::QUIC, "");
        let line = self.quic_bootstrap_line();
        let port = bootstrap_line_port(&line)
            .unwrap_or_else(|| panic!("the keeper offered no QUIC bootstrap: {line:?}"));
        let relay = UdpRelay::start(std::net::SocketAddr::from(([127, 0, 0, 1], port)), seed);
        self.set_knob(knob::QUIC_PORT, &relay.local_addr().port().to_string());
        relay
    }
}

// ---- UDP relay (link emulation for QUIC legs) ----

/// Packet counters of a [`UdpRelay`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    pub forwarded_up: u64,   // client -> target
    pub forwarded_down: u64, // target -> client
    pub dropped_up: u64,
    pub dropped_down: u64,
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn chance(&mut self, p: f64) -> bool {
        ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64) < p
    }
}

struct RelayState {
    delay: Duration,
    loss_up: f64,
    loss_down: f64,
    rng_up: SplitMix64,
    rng_down: SplitMix64,
    peer: Option<std::net::SocketAddr>,
}

#[derive(Default)]
struct RelayCounters {
    forwarded_up: std::sync::atomic::AtomicU64,
    forwarded_down: std::sync::atomic::AtomicU64,
    dropped_up: std::sync::atomic::AtomicU64,
    dropped_down: std::sync::atomic::AtomicU64,
}

type Queued = (Instant, Vec<u8>);

const RELAY_POLL: Duration = Duration::from_millis(20);

fn relay_socket() -> Arc<std::net::UdpSocket> {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind relay socket");
    socket
        .set_read_timeout(Some(RELAY_POLL))
        .expect("relay read timeout");
    Arc::new(socket)
}

/// Userspace UDP relay between a client-facing local port and `target`: one-way delay,
/// seeded random loss per direction, and `rebind` of the upstream-facing socket.
/// Owns: both relay sockets, the delay queues, the PRNG and the counters. Tests never open raw
/// sockets for link faults themselves; a new network fault is a new knob here.
pub struct UdpRelay {
    client_side: Arc<std::net::UdpSocket>,
    upstream: Arc<Mutex<Arc<std::net::UdpSocket>>>,
    state: Arc<Mutex<RelayState>>,
    counters: Arc<RelayCounters>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl UdpRelay {
    /// Bind 127.0.0.1:0 on both sides and start forwarding to `target`. `seed` fixes the loss
    /// sequence. Delay 0, loss 0.
    pub fn start(target: std::net::SocketAddr, seed: u64) -> UdpRelay {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;

        let client_side = relay_socket();
        let upstream = Arc::new(Mutex::new(relay_socket()));
        let state = Arc::new(Mutex::new(RelayState {
            delay: Duration::ZERO,
            loss_up: 0.0,
            loss_down: 0.0,
            rng_up: SplitMix64(seed),
            rng_down: SplitMix64(seed ^ 0x9e37_79b9_7f4a_7c15),
            peer: None,
        }));
        let counters = Arc::new(RelayCounters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (up_tx, up_rx) = mpsc::channel::<Queued>();
        let (down_tx, down_rx) = mpsc::channel::<Queued>();
        let mut threads = Vec::new();

        {
            let (sock, state, counters, stop) = (
                client_side.clone(),
                state.clone(),
                counters.clone(),
                stop.clone(),
            );
            threads.push(thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                while !stop.load(Ordering::Relaxed) {
                    let Ok((n, from)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    let due = {
                        let mut st = state.lock().expect("relay state");
                        st.peer = Some(from);
                        let p = st.loss_up;
                        if st.rng_up.chance(p) {
                            counters.dropped_up.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                        Instant::now() + st.delay
                    };
                    let _ = up_tx.send((due, buf[..n].to_vec()));
                }
            }));
        }
        {
            let (upstream, counters, stop) = (upstream.clone(), counters.clone(), stop.clone());
            threads.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((due, bytes)) = up_rx.recv_timeout(RELAY_POLL) else {
                        continue;
                    };
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    let sock = upstream.lock().expect("relay upstream").clone();
                    let _ = sock.send_to(&bytes, target);
                    counters.forwarded_up.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        {
            let (upstream, state, counters, stop) = (
                upstream.clone(),
                state.clone(),
                counters.clone(),
                stop.clone(),
            );
            threads.push(thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                while !stop.load(Ordering::Relaxed) {
                    let sock = upstream.lock().expect("relay upstream").clone();
                    let Ok((n, _)) = sock.recv_from(&mut buf) else {
                        continue;
                    };
                    let due = {
                        let mut st = state.lock().expect("relay state");
                        let p = st.loss_down;
                        if st.rng_down.chance(p) {
                            counters.dropped_down.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                        Instant::now() + st.delay
                    };
                    let _ = down_tx.send((due, buf[..n].to_vec()));
                }
            }));
        }
        {
            let (sock, state, counters, stop) = (
                client_side.clone(),
                state.clone(),
                counters.clone(),
                stop.clone(),
            );
            threads.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((due, bytes)) = down_rx.recv_timeout(RELAY_POLL) else {
                        continue;
                    };
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    let peer = state.lock().expect("relay state").peer;
                    if let Some(peer) = peer {
                        let _ = sock.send_to(&bytes, peer);
                        counters.forwarded_down.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }

        UdpRelay {
            client_side,
            upstream,
            state,
            counters,
            stop,
            threads,
        }
    }

    /// The address clients send to (the "server" address a dialer is pointed at).
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.client_side.local_addr().expect("relay local addr")
    }

    /// The relay's current upstream-facing address (what the target sees as the source).
    pub fn upstream_addr(&self) -> std::net::SocketAddr {
        self.upstream
            .lock()
            .expect("relay upstream")
            .local_addr()
            .expect("relay upstream addr")
    }

    /// One-way delay applied to each direction.
    pub fn set_delay(&self, one_way: Duration) {
        self.state.lock().expect("relay state").delay = one_way;
    }

    /// Loss probability in [0, 1] per direction (1.0 blocks UDP).
    pub fn set_loss(&self, up: f64, down: f64) {
        let mut st = self.state.lock().expect("relay state");
        st.loss_up = up;
        st.loss_down = down;
    }

    /// Replace the upstream-facing socket with a fresh one on a new local port.
    pub fn rebind(&self) {
        *self.upstream.lock().expect("relay upstream") = relay_socket();
    }

    pub fn stats(&self) -> RelayStats {
        let c = &self.counters;
        RelayStats {
            forwarded_up: c.forwarded_up.load(Ordering::SeqCst),
            forwarded_down: c.forwarded_down.load(Ordering::SeqCst),
            dropped_up: c.dropped_up.load(Ordering::SeqCst),
            dropped_down: c.dropped_down.load(Ordering::SeqCst),
        }
    }
}

impl Drop for UdpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}
