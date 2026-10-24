//! Public fork API for the resumable remote session.
//!
//! Owns: `start_ui_bridge`, the status capability constant, the `--x-mosh-*` flag
//! constants and the env knobs (`HERDR_MOSH_RESUME=0` kill switch, `HERDR_MOSH_GRACE`
//! seconds, default 60, clamped 5..=3600). Does not own: any I/O.
#![allow(dead_code)] // scaffold: items are filled and wired by mosh-dqw.4..8, 10

mod link_frames;
mod local_bridge;
pub(in crate::remote) mod remote;
mod ring;
mod session;
mod ssh_leg;

use super::{ManagedSshOptions, RemoteHerdr, RemoteSsh, SshStdioBridge};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Advertised as one entry of upstream `endpoint_capabilities` in `status client --json`
/// (hook owned by mosh-dqw.10).
pub(in crate::remote) const STATUS_CAPABILITY: &str = "x-mosh-resume-bridge-v1";
/// Remote session-attacher flag (`remote-client-bridge` argument).
pub(in crate::remote) const SESSION_FLAG: &str = "--x-mosh-session-v1";
/// Remote keeper flag (`remote-client-bridge` argument).
pub(in crate::remote) const KEEPER_FLAG: &str = "--x-mosh-keeper-v1";

const MOSH_RESUME_ENV: &str = "HERDR_MOSH_RESUME";
const MOSH_GRACE_ENV: &str = "HERDR_MOSH_GRACE";
const DEFAULT_GRACE_SECS: u64 = 60;
const MIN_GRACE_SECS: u64 = 5;
const MAX_GRACE_SECS: u64 = 3600;

/// Local-only: the remote attacher/keeper never read these; grace reaches the keeper only
/// via `HELLO.grace_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MoshConfig {
    pub resume_enabled: bool,
    pub grace: Duration,
}

impl MoshConfig {
    fn parse(resume: Option<&str>, grace: Option<&str>) -> MoshConfig {
        let resume_enabled = resume.map(str::trim) != Some("0");
        let grace_secs = grace
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(DEFAULT_GRACE_SECS, |secs| {
                secs.clamp(MIN_GRACE_SECS, MAX_GRACE_SECS)
            });
        MoshConfig {
            resume_enabled,
            grace: Duration::from_secs(grace_secs),
        }
    }
}

/// Process environment config, read once.
pub(super) fn get() -> MoshConfig {
    static CONFIG: OnceLock<MoshConfig> = OnceLock::new();
    *CONFIG.get_or_init(|| {
        MoshConfig::parse(
            std::env::var(MOSH_RESUME_ENV).ok().as_deref(),
            std::env::var(MOSH_GRACE_ENV).ok().as_deref(),
        )
    })
}

/// The single plain-vs-resume choice, and the choice of `LegDialer` kind for resume.
///
/// Owns: reading env config once (kill switch; grace clamped 5..=3600 s), the Windows-remote
/// and non-Unix-local checks, capability probe and candidate re-selection (implemented in
/// `ssh_leg`), choosing the `LegDialer`, and returning the `SshStdioBridge`.
/// Must not: be called from anywhere except the two swap points (`run_remote` and
/// `connect_saved_ssh`); callers never branch on mosh support.
///
/// Scaffold: every call currently takes the plain path (mosh-dqw.8 fills the resume branch).
pub(in crate::remote) fn start_ui_bridge(
    ssh: &RemoteSsh,
    target: String,
    remote_herdr: RemoteHerdr,
    local_socket: PathBuf,
    session_name: String,
    ssh_options: Option<&ManagedSshOptions>,
    noninteractive: bool,
    should_stop: Arc<AtomicBool>,
) -> io::Result<SshStdioBridge> {
    // The resume branch (mosh-dqw.8) is `#[cfg(unix)]` only; non-Unix local always goes plain.
    #[cfg(not(unix))]
    {
        let _ = ssh;
        return SshStdioBridge::start(
            target,
            remote_herdr,
            local_socket,
            session_name,
            ssh_options,
            noninteractive,
            should_stop,
        );
    }
    #[cfg(unix)]
    {
        let _ = ssh;
        SshStdioBridge::start(
            target,
            remote_herdr,
            local_socket,
            session_name,
            ssh_options,
            noninteractive,
            should_stop,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grace(value: Option<&str>) -> Duration {
        MoshConfig::parse(None, value).grace
    }

    #[test]
    fn grace_defaults_and_clamps() {
        assert_eq!(grace(None), Duration::from_secs(60));
        assert_eq!(grace(Some("1")), Duration::from_secs(5));
        assert_eq!(grace(Some("99999")), Duration::from_secs(3600));
        assert_eq!(grace(Some("abc")), Duration::from_secs(60));
        assert_eq!(grace(Some("")), Duration::from_secs(60));
        assert_eq!(grace(Some("120")), Duration::from_secs(120));
    }

    #[test]
    fn kill_switch_only_on_zero() {
        assert!(!MoshConfig::parse(Some("0"), None).resume_enabled);
        assert!(!MoshConfig::parse(Some(" 0 "), None).resume_enabled);
        assert!(MoshConfig::parse(None, None).resume_enabled);
        assert!(MoshConfig::parse(Some("1"), None).resume_enabled);
    }
}
