//! Public fork API for the resumable remote session.
//!
//! Owns: `start_ui_bridge`, the status capability constant, the `--x-mosh-*` flag
//! constants and the env knobs (`HERDR_MOSH_RESUME=0` kill switch, `HERDR_MOSH_GRACE`
//! seconds, default 60, clamped 5..=3600). Does not own: any I/O.
// The resume stack (session, ring, link frames, ssh leg, remote) is only reached from the
// unix bridge, so a non-unix build sees it as unused; unix builds keep dead-code strict.
#![cfg_attr(not(unix), allow(dead_code))]

mod link_frames;
#[cfg(unix)]
mod local_bridge;
pub(in crate::remote) mod remote;
#[cfg(unix)]
mod quic;
#[cfg(unix)]
mod quic_bootstrap;
#[cfg(unix)]
mod quic_leg;
mod ring;
mod session;
mod ssh_leg;
mod window;

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
/// Remote attacher flag that asks for the QUIC bootstrap line. It goes FIRST in the argument
/// list: upstream's `remote-client-bridge` gate admits a list only when its first argument
/// starts with `--x-mosh-`.
pub(in crate::remote) const QUIC_FLAG: &str = "--x-mosh-quic-v1";
/// Remote keeper flag (`remote-client-bridge` argument).
pub(in crate::remote) const KEEPER_FLAG: &str = "--x-mosh-keeper-v1";

const MOSH_RESUME_ENV: &str = "HERDR_MOSH_RESUME";
const MOSH_GRACE_ENV: &str = "HERDR_MOSH_GRACE";
#[cfg(unix)]
const MOSH_TRACE_ENV: &str = "HERDR_MOSH_TRACE";
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
    // The resume branch is `#[cfg(unix)]` only; non-Unix local always goes plain. This gate is
    // compiled out off Unix and is covered by inspection only (not cross-compiled in tests).
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
        let config = get();
        if !platform_gate(config, remote_herdr.platform.is_windows()) {
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
        let candidate = match ssh_leg::select_resume_candidate(ssh, &remote_herdr) {
            Ok(Some(candidate)) => candidate,
            Ok(None) => {
                if !noninteractive {
                    eprintln!(
                        "herdr: remote herdr lacks {STATUS_CAPABILITY}; using the plain bridge (set HERDR_REMOTE_BINARY to a resume-capable build)"
                    );
                }
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
            Err(err) => {
                tracing::debug!(%err, "mosh capability probe failed; plain bridge");
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
        };
        // Exactly `SshStdioBridge::start`'s command, from the ORIGINAL (not re-selected) binary.
        let plain_command = plain_bridge_command(&remote_herdr, &session_name, noninteractive);
        let dialer = ssh_leg::SshLegDialer::new(
            target.clone(),
            &candidate,
            &session_name,
            ssh_options,
            noninteractive,
            idle_opt_in(&candidate, noninteractive),
        );
        local_bridge::start(
            local_bridge::BridgeParams {
                local_socket,
                grace: config.grace,
                noninteractive,
                plain: local_bridge::PlainFallback::Ssh {
                    target,
                    remote_command: plain_command,
                    ssh_options: ssh_options.cloned(),
                    noninteractive,
                },
                trace_dir: std::env::var_os(MOSH_TRACE_ENV).map(PathBuf::from),
            },
            Box::new(dialer),
            should_stop,
        )
    }
}

/// `true` means try the resume path; `false` (kill switch or a Windows remote) takes the plain
/// upstream bridge.
/// Upstream's idle opt-in rule for `remote-client-bridge` (saved machines only, when the
/// remote advertises it). Shared by the plain command and the resume leg command.
#[cfg(unix)]
fn idle_opt_in(herdr: &RemoteHerdr, noninteractive: bool) -> bool {
    noninteractive && herdr.bridge_idle_timeout
}

/// Exactly `SshStdioBridge::start`'s remote command for `herdr`.
#[cfg(unix)]
fn plain_bridge_command(herdr: &RemoteHerdr, session_name: &str, noninteractive: bool) -> String {
    herdr
        .executable
        .bridge_command_with_idle_timeout(session_name, idle_opt_in(herdr, noninteractive))
}

fn platform_gate(config: MoshConfig, remote_is_windows: bool) -> bool {
    config.resume_enabled && !remote_is_windows
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
    fn platform_gate_takes_resume_only_for_enabled_posix_remotes() {
        let enabled = MoshConfig::parse(None, None);
        let killed = MoshConfig::parse(Some("0"), None);
        assert!(platform_gate(enabled, false));
        assert!(!platform_gate(killed, false));
        assert!(!platform_gate(enabled, true));
        assert!(!platform_gate(killed, true));
    }

    #[test]
    fn kill_switch_only_on_zero() {
        assert!(!MoshConfig::parse(Some("0"), None).resume_enabled);
        assert!(!MoshConfig::parse(Some(" 0 "), None).resume_enabled);
        assert!(MoshConfig::parse(None, None).resume_enabled);
        assert!(MoshConfig::parse(Some("1"), None).resume_enabled);
    }
}
