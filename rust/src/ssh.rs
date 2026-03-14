//! SSH utilities for remote workspace management.
//!
//! Provides host parsing, shell escaping, and subprocess-based SSH execution
//! mirroring the Elixir `SymphonyElixir.SSH` module.

use std::env;
use std::process::Output;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

/// Configuration for an SSH connection to a specific host.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// Host string, optionally including port (e.g. `"host:22"`, `"[::1]:2222"`).
    pub host: String,
    /// Explicit port override. Takes precedence over a port parsed from `host`.
    pub port: Option<u16>,
    /// Path to a custom SSH config file, passed via `-F`.
    pub ssh_config_file: Option<String>,
}

#[derive(Debug, Error)]
pub enum SshError {
    #[error("ssh_not_found: could not locate ssh executable")]
    SshNotFound,
    #[error("ssh_command_failed: {0}")]
    CommandFailed(String),
    #[error("ssh_timeout: command timed out after {0}ms")]
    Timeout(u64),
}

/// Parsed SSH target with destination and optional port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub destination: String,
    pub port: Option<u16>,
}

/// Parse a `"host:port"` string. Supports IPv6 bracket notation `[::1]:22`.
///
/// If the host string contains an unbracketed `:` (i.e. a bare IPv6 address
/// without port), the entire string is treated as the destination with no port.
pub fn parse_host(host_str: &str) -> SshTarget {
    let trimmed = host_str.trim();

    // Try to match a trailing `:port` segment.
    if let Some(colon_pos) = trimmed.rfind(':') {
        let destination_part = &trimmed[..colon_pos];
        let port_part = &trimmed[colon_pos + 1..];

        if let Ok(port) = port_part.parse::<u16>() {
            if valid_port_destination(destination_part) {
                return SshTarget {
                    destination: destination_part.to_owned(),
                    port: Some(port),
                };
            }
        }
    }

    SshTarget {
        destination: trimmed.to_owned(),
        port: None,
    }
}

/// A destination is valid for `"host:port"` splitting when it is non-empty and
/// either contains no `:` (simple hostname / IPv4) or is a bracketed IPv6
/// literal like `[::1]`.
fn valid_port_destination(destination: &str) -> bool {
    if destination.is_empty() {
        return false;
    }
    if !destination.contains(':') {
        return true;
    }
    // Accept bracketed IPv6, e.g. "[::1]"
    destination.contains('[') && destination.contains(']')
}

/// Shell-escape a value using single quotes, matching the Elixir behaviour.
///
/// Any embedded single-quote is replaced with `'"'"'` (end the single-quoted
/// string, insert an escaped single-quote via double-quotes, then resume the
/// single-quoted string).
pub fn shell_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            escaped.push_str("'\"'\"'");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

/// Wrap a command so it runs inside a login shell on the remote side.
pub fn remote_shell_command(command: &str) -> String {
    format!("bash -lc {}", shell_escape(command))
}

/// Build the SSH argument list for a given host string and command.
fn ssh_args(host_str: &str, command: &str) -> Vec<String> {
    let target = parse_host(host_str);
    let mut args: Vec<String> = Vec::new();

    // Support custom SSH config file via environment variable.
    if let Ok(config_path) = env::var("SYMPHONY_SSH_CONFIG") {
        if !config_path.is_empty() {
            args.push("-F".to_owned());
            args.push(config_path);
        }
    }

    // Disable pseudo-terminal allocation.
    args.push("-T".to_owned());

    if let Some(port) = target.port {
        args.push("-p".to_owned());
        args.push(port.to_string());
    }

    args.push(target.destination);
    args.push(remote_shell_command(command));

    args
}

/// Execute a command on a remote host via SSH.
pub async fn run(host: &str, command: &str) -> Result<Output, SshError> {
    let args = ssh_args(host, command);

    let output = Command::new("ssh")
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| SshError::CommandFailed(error.to_string()))?;

    Ok(output)
}

/// Execute a command on a remote host via SSH with a timeout.
pub async fn run_with_timeout(
    host: &str,
    command: &str,
    timeout_ms: u64,
) -> Result<Output, SshError> {
    let args = ssh_args(host, command);

    let mut cmd = Command::new("ssh");
    cmd.args(&args).kill_on_drop(true);

    let output = timeout(Duration::from_millis(timeout_ms), cmd.output())
        .await
        .map_err(|_| SshError::Timeout(timeout_ms))?
        .map_err(|error| SshError::CommandFailed(error.to_string()))?;

    Ok(output)
}

/// Execute a command on a remote host via SSH, using an explicit `SshConfig`.
///
/// The `SshConfig.ssh_config_file` takes precedence over the
/// `SYMPHONY_SSH_CONFIG` environment variable, and `SshConfig.port` takes
/// precedence over a port parsed from the host string.
pub async fn run_with_config(config: &SshConfig, command: &str) -> Result<Output, SshError> {
    let target = parse_host(&config.host);
    let mut args: Vec<String> = Vec::new();

    // Custom SSH config file takes precedence over env var.
    if let Some(ref config_path) = config.ssh_config_file {
        args.push("-F".to_owned());
        args.push(config_path.clone());
    } else if let Ok(config_path) = env::var("SYMPHONY_SSH_CONFIG") {
        if !config_path.is_empty() {
            args.push("-F".to_owned());
            args.push(config_path);
        }
    }

    args.push("-T".to_owned());

    // Explicit port on SshConfig overrides parsed host port.
    let port = config.port.or(target.port);
    if let Some(port) = port {
        args.push("-p".to_owned());
        args.push(port.to_string());
    }

    args.push(target.destination);
    args.push(remote_shell_command(command));

    let output = Command::new("ssh")
        .args(&args)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| SshError::CommandFailed(error.to_string()))?;

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_simple() {
        let target = parse_host("example.com");
        assert_eq!(
            target,
            SshTarget {
                destination: "example.com".to_owned(),
                port: None,
            }
        );
    }

    #[test]
    fn parse_host_with_port() {
        let target = parse_host("example.com:2222");
        assert_eq!(
            target,
            SshTarget {
                destination: "example.com".to_owned(),
                port: Some(2222),
            }
        );
    }

    #[test]
    fn parse_host_ipv6_bracketed_with_port() {
        let target = parse_host("[::1]:2222");
        assert_eq!(
            target,
            SshTarget {
                destination: "[::1]".to_owned(),
                port: Some(2222),
            }
        );
    }

    #[test]
    fn parse_host_ipv6_bare_no_port() {
        // A bare IPv6 address without brackets should NOT be split on the last colon.
        let target = parse_host("::1");
        assert_eq!(
            target,
            SshTarget {
                destination: "::1".to_owned(),
                port: None,
            }
        );
    }

    #[test]
    fn parse_host_ipv6_full_bare() {
        let target = parse_host("2001:db8::1");
        assert_eq!(
            target,
            SshTarget {
                destination: "2001:db8::1".to_owned(),
                port: None,
            }
        );
    }

    #[test]
    fn parse_host_whitespace_trimmed() {
        let target = parse_host("  host.example.com:22  ");
        assert_eq!(
            target,
            SshTarget {
                destination: "host.example.com".to_owned(),
                port: Some(22),
            }
        );
    }

    #[test]
    fn parse_host_user_at_host() {
        let target = parse_host("user@host.example.com:22");
        assert_eq!(
            target,
            SshTarget {
                destination: "user@host.example.com".to_owned(),
                port: Some(22),
            }
        );
    }

    #[test]
    fn parse_host_no_port_number() {
        // Trailing colon with non-numeric text should not parse as port.
        let target = parse_host("host.example.com:abc");
        assert_eq!(
            target,
            SshTarget {
                destination: "host.example.com:abc".to_owned(),
                port: None,
            }
        );
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\"'\"'s'");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_special_chars() {
        assert_eq!(shell_escape("a b; rm -rf /"), "'a b; rm -rf /'");
    }

    #[test]
    fn remote_shell_command_wraps() {
        let cmd = remote_shell_command("echo hello");
        assert_eq!(cmd, "bash -lc 'echo hello'");
    }

    #[test]
    fn remote_shell_command_escapes_quotes() {
        let cmd = remote_shell_command("echo 'world'");
        assert_eq!(cmd, "bash -lc 'echo '\"'\"'world'\"'\"''");
    }
}
