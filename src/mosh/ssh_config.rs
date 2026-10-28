//! `ssh -G` resolution of the dial target. Owns: running `ssh -G <target>` with the managed
//! options and parsing its hostname and jump settings. Does not own: the QUIC registration
//! decision (`mod.rs`) or any connection.

use super::super::ManagedSshOptions;
use super::super as attach;
use std::io;
use std::process::{Command, Stdio};

/// What `ssh -G` says about a target.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SshTarget {
    /// The resolved `hostname` (the address QUIC dials).
    pub hostname: Option<String>,
    /// A `proxyjump` or `proxycommand` is configured, so UDP would bypass the path ssh takes.
    pub jump: bool,
}

/// Parse `ssh -G` output. Keys are case-insensitive; `proxyjump` and `proxycommand` count
/// unless the value is `none`.
pub(super) fn parse_ssh_g(output: &str) -> SshTarget {
    let mut target = SshTarget::default();
    for line in output.lines() {
        let line = line.trim();
        let (key, value) = match line.split_once(char::is_whitespace) {
            Some((key, value)) => (key, value.trim()),
            None => (line, ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "hostname" if target.hostname.is_none() && !value.is_empty() => {
                target.hostname = Some(value.to_string());
            }
            "proxyjump" | "proxycommand"
                if !value.is_empty() && !value.eq_ignore_ascii_case("none") =>
            {
                target.jump = true;
            }
            _ => {}
        }
    }
    target
}

/// Run `ssh -G <target>` with the managed options applied (stdin null, stderr captured).
pub(super) fn ssh_g(target: &str, options: Option<&ManagedSshOptions>) -> io::Result<String> {
    let mut command = Command::new("ssh");
    attach::apply_managed_ssh_options(&mut command, options);
    let output = command
        .arg("-G")
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "ssh -G failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ssh_g_extracts_hostname() {
        let parsed = parse_ssh_g("user bob\nhostname box.example.net\nport 22\n");
        assert_eq!(parsed.hostname.as_deref(), Some("box.example.net"));
        assert!(!parsed.jump);
        assert_eq!(parse_ssh_g("port 22\n").hostname, None);
    }

    #[test]
    fn parse_ssh_g_detects_proxyjump() {
        let parsed = parse_ssh_g("hostname h\nproxyjump bastion\n");
        assert!(parsed.jump);
        assert_eq!(parsed.hostname.as_deref(), Some("h"));
        assert!(parse_ssh_g("ProxyJump bastion\n").jump);
    }

    #[test]
    fn parse_ssh_g_detects_proxycommand() {
        assert!(parse_ssh_g("hostname h\nproxycommand nc %h %p\n").jump);
    }

    #[test]
    fn parse_ssh_g_none_values_are_not_jumps() {
        assert!(!parse_ssh_g("proxyjump none\nproxycommand none\n").jump);
        assert!(!parse_ssh_g("proxyjump None\n").jump);
    }
}
