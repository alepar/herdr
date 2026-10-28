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
#[cfg(unix)]
mod render_reorder;
mod ring;
mod session;
#[cfg(unix)]
mod ssh_config;
mod ssh_leg;
mod window;

use super::{ManagedSshOptions, RemoteHerdr, RemoteSsh, SshStdioBridge};
#[cfg(unix)]
use crate::client::mosh_outage::predict::{PredictMode, ECHO_CAPABILITY};
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
const MOSH_QUIC_ENV: &str = "HERDR_MOSH_QUIC";
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
    /// `HERDR_MOSH_QUIC=0` is the only way to turn the QUIC leg off.
    pub quic_enabled: bool,
    pub grace: Duration,
}

impl MoshConfig {
    fn parse(resume: Option<&str>, grace: Option<&str>, quic: Option<&str>) -> MoshConfig {
        let resume_enabled = resume.map(str::trim) != Some("0");
        let quic_enabled = quic.map(str::trim) != Some("0");
        let grace_secs = grace
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(DEFAULT_GRACE_SECS, |secs| {
                secs.clamp(MIN_GRACE_SECS, MAX_GRACE_SECS)
            });
        MoshConfig {
            resume_enabled,
            quic_enabled,
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
            std::env::var(MOSH_QUIC_ENV).ok().as_deref(),
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
        let ssh_dialer = ssh_leg::SshLegDialer::new(
            target.clone(),
            &candidate.herdr,
            &session_name,
            ssh_options,
            noninteractive,
            idle_opt_in(&candidate.herdr, noninteractive),
        );
        let ssh_target = if quic_eligible(config, &candidate.capabilities) {
            ssh_config::ssh_g(&target, ssh_options)
                .map(|output| ssh_config::parse_ssh_g(&output))
                .map_err(|err| tracing::debug!(%err, "ssh -G failed; ssh legs only"))
                .ok()
        } else {
            None
        };
        let dialer: Box<dyn ssh_leg::LegDialer> = match choose_dialer(
            config,
            &candidate.capabilities,
            ssh_target.as_ref(),
        ) {
            DialerChoice::Ssh => Box::new(ssh_dialer),
            DialerChoice::Quic { dial_host } => {
                match quic_leg::QuicLegDialer::new(ssh_dialer, dial_host, config.grace) {
                    Ok(dialer) => Box::new(dialer),
                    Err(err) => {
                        tracing::debug!(%err, "QUIC dialer unavailable; ssh legs only");
                        Box::new(ssh_leg::SshLegDialer::new(
                            target.clone(),
                            &candidate.herdr,
                            &session_name,
                            ssh_options,
                            noninteractive,
                            idle_opt_in(&candidate.herdr, noninteractive),
                        ))
                    }
                }
            }
        };
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
                render_split: supersede_advertised(&candidate.capabilities),
                echo: echo_wanted(&candidate.capabilities, PredictMode::get()),
            },
            dialer,
            should_stop,
        )
    }
}

/// The remote advertises the render split (`x-mosh-supersede-v1` in the status probe).
#[cfg(unix)]
fn supersede_advertised(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|c| c == link_frames::SUPERSEDE_CAPABILITY)
}

/// The remote advertises echo acknowledgement (`x-mosh-echo-v1`) and prediction is enabled.
#[cfg(unix)]
fn echo_wanted(capabilities: &[String], mode: PredictMode) -> bool {
    mode != PredictMode::Off && capabilities.iter().any(|c| c == ECHO_CAPABILITY)
}

/// Which `LegDialer` `start_ui_bridge` registers.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum DialerChoice {
    Ssh,
    Quic { dial_host: String },
}

/// The checks that decide whether `ssh -G` is worth running at all.
#[cfg(unix)]
fn quic_eligible(config: MoshConfig, capabilities: &[String]) -> bool {
    config.quic_enabled && capabilities.iter().any(|c| c == quic::QUIC_CAPABILITY)
}

/// QUIC only when enabled, the remote advertises it, and `ssh -G` resolved a host without a
/// jump (UDP would bypass the path ssh takes through a jump host).
#[cfg(unix)]
fn choose_dialer(
    config: MoshConfig,
    capabilities: &[String],
    target: Option<&ssh_config::SshTarget>,
) -> DialerChoice {
    if !quic_eligible(config, capabilities) {
        return DialerChoice::Ssh;
    }
    match target {
        Some(ssh_config::SshTarget {
            hostname: Some(host),
            jump: false,
        }) => DialerChoice::Quic {
            dial_host: host.clone(),
        },
        _ => DialerChoice::Ssh,
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
        MoshConfig::parse(None, value, None).grace
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
        let enabled = MoshConfig::parse(None, None, None);
        let killed = MoshConfig::parse(Some("0"), None, None);
        assert!(platform_gate(enabled, false));
        assert!(!platform_gate(killed, false));
        assert!(!platform_gate(enabled, true));
        assert!(!platform_gate(killed, true));
    }

    #[test]
    fn kill_switch_only_on_zero() {
        assert!(!MoshConfig::parse(Some("0"), None, None).resume_enabled);
        assert!(!MoshConfig::parse(Some(" 0 "), None, None).resume_enabled);
        assert!(MoshConfig::parse(None, None, None).resume_enabled);
        assert!(MoshConfig::parse(Some("1"), None, None).resume_enabled);
    }

    #[test]
    fn quic_kill_switch_only_on_zero() {
        assert!(!MoshConfig::parse(None, None, Some("0")).quic_enabled);
        assert!(!MoshConfig::parse(None, None, Some(" 0 ")).quic_enabled);
        assert!(MoshConfig::parse(None, None, None).quic_enabled);
        assert!(MoshConfig::parse(None, None, Some("1")).quic_enabled);
        assert!(MoshConfig::parse(None, None, Some("")).quic_enabled);
    }

    #[cfg(unix)]
    #[test]
    fn echo_flag_needs_the_capability_and_a_prediction_mode() {
        let with = vec![STATUS_CAPABILITY.to_string(), ECHO_CAPABILITY.to_string()];
        assert!(echo_wanted(&with, PredictMode::On));
        assert!(echo_wanted(&with, PredictMode::Adaptive));
        assert!(!echo_wanted(&with, PredictMode::Off));
        assert!(!echo_wanted(&[STATUS_CAPABILITY.to_string()], PredictMode::On));
    }

    #[cfg(unix)]
    #[test]
    fn supersede_flag_follows_the_probe_capability() {
        let with = vec![
            STATUS_CAPABILITY.to_string(),
            link_frames::SUPERSEDE_CAPABILITY.to_string(),
        ];
        assert!(supersede_advertised(&with));
        assert!(!supersede_advertised(&[STATUS_CAPABILITY.to_string()]));
        assert!(!supersede_advertised(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn choose_dialer_table() {
        let on = MoshConfig::parse(None, None, None);
        let off = MoshConfig::parse(None, None, Some("0"));
        let with = vec![quic::QUIC_CAPABILITY.to_string()];
        let without = vec![STATUS_CAPABILITY.to_string()];
        let direct = ssh_config::SshTarget {
            hostname: Some("box.example".into()),
            jump: false,
        };
        let jumped = ssh_config::SshTarget {
            hostname: Some("box.example".into()),
            jump: true,
        };
        let nameless = ssh_config::SshTarget::default();
        let quic = DialerChoice::Quic {
            dial_host: "box.example".into(),
        };
        assert_eq!(choose_dialer(on, &with, Some(&direct)), quic);
        assert_eq!(choose_dialer(off, &with, Some(&direct)), DialerChoice::Ssh);
        assert_eq!(choose_dialer(on, &without, Some(&direct)), DialerChoice::Ssh);
        assert_eq!(choose_dialer(on, &with, Some(&jumped)), DialerChoice::Ssh);
        assert_eq!(choose_dialer(on, &with, Some(&nameless)), DialerChoice::Ssh);
        assert_eq!(choose_dialer(on, &with, None), DialerChoice::Ssh);
        assert!(quic_eligible(on, &with));
        assert!(!quic_eligible(off, &with));
        assert!(!quic_eligible(on, &without));
    }
}
