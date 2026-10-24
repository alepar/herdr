//! Leg abstraction and its ssh implementation: per-leg command construction, spawn/kill,
//! failure classification, reconnect back-off. Does not own session bytes.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Which leg of a session is being dialed: leg 0 (first connection, today's options) or a
/// reconnect leg (noninteractive, ControlMaster bypassed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LegKind {
    Initial,
    Reconnect,
}

/// Structured end report of one leg; classification input for `LegDialer::classify`.
#[derive(Debug, Default)]
pub(super) struct LegEndReport {
    pub exit_status: Option<ExitStatus>,
    /// Captured on reconnect legs only.
    pub stderr: Option<String>,
    pub welcome_reached: bool,
    /// `END` reason seen before `WELCOME`.
    pub pre_welcome_end: Option<String>,
    /// Pre-`WELCOME` bound, setup deadline or stop.
    pub killed_by_bridge: bool,
}

type Teardown = Box<dyn FnOnce(bool) -> (Option<ExitStatus>, Option<String>) + Send>;

/// One live connection to the keeper side (transport-neutral).
///
/// Owns: a duplex byte stream, a teardown handle, and a structured end report (`ExitStatus`,
/// captured stderr on reconnect legs, whether `WELCOME` was reached, any pre-`WELCOME` `END`
/// reason). For ssh it owns the ssh child directly: no socketpair, no `bridge_child`.
/// Hides: which process or transport carries the bytes.
/// Must not: know session semantics (it never interprets bytes).
/// Lifecycle: produced by a `LegDialer`, owned by the connection run; teardown kills the
/// child on silence, protocol error or session end. Dropping an unfinished leg tears it down
/// with `kill = true`.
pub(super) struct Leg {
    reader: Option<Box<dyn io::Read + Send>>,
    writer: Option<Box<dyn io::Write + Send>>,
    /// Argument: whether to kill the underlying process before reaping it.
    teardown: Option<Teardown>,
    welcome_reached: bool,
    pre_welcome_end: Option<String>,
}

impl Leg {
    pub(super) fn new(
        reader: Box<dyn io::Read + Send>,
        writer: Box<dyn io::Write + Send>,
        teardown: Teardown,
    ) -> Leg {
        Leg {
            reader: Some(reader),
            writer: Some(writer),
            teardown: Some(teardown),
            welcome_reached: false,
            pre_welcome_end: None,
        }
    }

    /// Wrap a spawned child whose stdin/stdout are the leg's byte stream.
    pub(super) fn from_child(mut child: Child) -> io::Result<Leg> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("leg child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("leg child has no stdout"))?;
        Ok(Leg::new(
            Box::new(stdout),
            Box::new(stdin),
            Box::new(move |kill| {
                if kill {
                    let _ = child.kill();
                }
                (child.wait().ok(), None)
            }),
        ))
    }

    /// The pumps run on separate threads, so each half is taken once.
    pub(super) fn take_reader(&mut self) -> Option<Box<dyn io::Read + Send>> {
        self.reader.take()
    }

    pub(super) fn take_writer(&mut self) -> Option<Box<dyn io::Write + Send>> {
        self.writer.take()
    }

    pub(super) fn mark_welcome(&mut self) {
        self.welcome_reached = true;
    }

    pub(super) fn record_pre_welcome_end(&mut self, reason: String) {
        self.pre_welcome_end = Some(reason);
    }

    /// Tear the leg down (killing the process when `kill`) and return its end report.
    pub(super) fn finish(mut self, kill: bool) -> LegEndReport {
        let (exit_status, stderr) = match self.teardown.take() {
            Some(teardown) => {
                // Close our stream halves first so a child waiting on stdin can exit.
                self.writer = None;
                self.reader = None;
                teardown(kill)
            }
            None => (None, None),
        };
        LegEndReport {
            exit_status,
            stderr,
            welcome_reached: self.welcome_reached,
            pre_welcome_end: self.pre_welcome_end.take(),
            killed_by_bridge: kill,
        }
    }
}

impl Drop for Leg {
    fn drop(&mut self) {
        if let Some(teardown) = self.teardown.take() {
            self.writer = None;
            self.reader = None;
            teardown(true);
        }
    }
}

/// Why a leg could not be made or ended, each carrying the error that explains it.
/// `Retryable` (including silence or EOF before `WELCOME` on a reconnect leg, within the
/// give-up deadline), `Fatal` (auth/host key at exit 255 on a reconnect leg, or a positive
/// remote refusal), `Unsupported` (any remote-side failure before `WELCOME` on leg 0 only,
/// which falls back to the plain bridge). Kept distinct.
#[derive(Debug)]
pub(super) enum LegError {
    Retryable(io::Error),
    Fatal(io::Error),
    Unsupported(io::Error),
}

/// "Produce the next leg."
///
/// Owns (`SshLegDialer`, mosh-dqw.6): per-leg remote command via upstream builders, leg-0 vs
/// reconnect options, the back-off schedule, and failure classification from the leg's end
/// report. Hides: ssh arguments and ControlMaster bypass. Must not: touch session bytes or the
/// give-up deadline (the caller owns the deadline and stop flag and stops dialing when either
/// fires). A new transport is a new impl registered in `start_ui_bridge`; callers never build
/// ssh commands.
/// Lifecycle: created by `start_ui_bridge`, owned by the `LocalBridge` for the bridge's lifetime.
pub(super) trait LegDialer: Send {
    /// Produce the next leg. The caller owns the give-up deadline and stop flag and stops
    /// dialing when either fires; a dialer may return `Retryable` early when `stop` is set.
    fn dial(&mut self, kind: LegKind, stop: &AtomicBool) -> Result<Leg, LegError>;
    /// Classify a finished leg from its structured end report.
    fn classify(&self, kind: LegKind, end: &LegEndReport) -> LegError;
    /// Delay before reconnect attempt `attempt` (0-based): 1, 2, 4, 8, 8... s.
    fn backoff(&self, attempt: u32) -> Duration;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    fn spawn(script: &str) -> Child {
        Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh")
    }

    #[test]
    fn finish_reports_exit_code() {
        let leg = Leg::from_child(spawn("exit 3")).unwrap();
        let report = leg.finish(false);
        assert_eq!(report.exit_status.and_then(|s| s.code()), Some(3));
        assert!(!report.killed_by_bridge);
    }

    #[test]
    fn finish_kill_returns_promptly() {
        let leg = Leg::from_child(spawn("sleep 30")).unwrap();
        let started = Instant::now();
        let report = leg.finish(true);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(report.killed_by_bridge);
        assert_eq!(
            report.exit_status.and_then(|s| s.signal()),
            Some(libc::SIGKILL)
        );
    }

    #[test]
    fn drop_kills_unfinished_leg() {
        let leg = Leg::from_child(spawn("sleep 30")).unwrap();
        let started = Instant::now();
        drop(leg);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stream_echoes_through_child() {
        let mut leg = Leg::from_child(spawn("cat")).unwrap();
        let mut writer = leg.take_writer().unwrap();
        let mut reader = leg.take_reader().unwrap();
        writer.write_all(b"ping").unwrap();
        writer.flush().unwrap();
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        assert!(leg.take_writer().is_none());
        drop(writer);
        drop(reader);
        leg.finish(true);
    }

    #[test]
    fn welcome_and_pre_welcome_end_are_reported() {
        let mut leg = Leg::from_child(spawn("exit 0")).unwrap();
        leg.mark_welcome();
        leg.record_pre_welcome_end("nope".into());
        let report = leg.finish(false);
        assert!(report.welcome_reached);
        assert_eq!(report.pre_welcome_end.as_deref(), Some("nope"));
    }
}
