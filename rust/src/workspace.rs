use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::{error, info, warn};

use crate::config::EffectiveConfig;
use crate::error::truncate_for_log;
use crate::issue::Issue;
use crate::path_safety::{PathSafetyError, validate_workspace_path};
use crate::ssh;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub path: PathBuf,
    pub workspace_key: String,
    pub created_now: bool,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(transparent)]
    Path(#[from] PathSafetyError),
    #[error("workspace_io: {0}")]
    Io(String),
    #[error("workspace_hook_failed: {hook}: {reason}")]
    HookFailed { hook: &'static str, reason: String },
    #[error("workspace_hook_timeout: {hook}: {timeout_ms}")]
    HookTimeout { hook: &'static str, timeout_ms: u64 },
    #[error("workspace_ssh: {0}")]
    Ssh(String),
    #[error("workspace_remote_failed: host={host}: {reason}")]
    RemoteFailed { host: String, reason: String },
}

pub async fn prepare_workspace(
    config: &EffectiveConfig,
    issue_identifier: &str,
) -> Result<Workspace, WorkspaceError> {
    let workspace_key = sanitize_identifier(issue_identifier);
    let workspace_path = config.workspace.root.join(&workspace_key);
    let validated_path = validate_workspace_path(&config.workspace.root, &workspace_path)?;

    if let Some(parent) = validated_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| WorkspaceError::Io(error.to_string()))?;
    }

    let metadata = tokio::fs::symlink_metadata(&validated_path).await;
    let created_now = match metadata {
        Ok(metadata) if metadata.is_dir() => false,
        Ok(_) => {
            match tokio::fs::remove_file(&validated_path).await {
                Ok(()) => {}
                Err(_) => {
                    tokio::fs::remove_dir_all(&validated_path)
                        .await
                        .map_err(|error| WorkspaceError::Io(error.to_string()))?;
                }
            }
            tokio::fs::create_dir_all(&validated_path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(&validated_path)
                .await
                .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            true
        }
        Err(error) => return Err(WorkspaceError::Io(error.to_string())),
    };

    let workspace = Workspace {
        path: validated_path,
        workspace_key,
        created_now,
    };

    if workspace.created_now {
        if let Some(script) = &config.hooks.after_create {
            if let Err(error) = run_hook(
                script,
                &workspace.path,
                "after_create",
                config.hooks.timeout_ms,
            )
            .await
            {
                let _ = tokio::fs::remove_dir_all(&workspace.path).await;
                return Err(error);
            }
        }
    }

    cleanup_temp_artifacts(&workspace.path).await?;
    Ok(workspace)
}

pub async fn run_before_run_hook(
    config: &EffectiveConfig,
    workspace: &Workspace,
    issue: &Issue,
) -> Result<(), WorkspaceError> {
    cleanup_temp_artifacts(&workspace.path).await?;

    if let Some(script) = &config.hooks.before_run {
        info!(
            issue_id = %issue.id,
            issue_identifier = %issue.identifier,
            workspace = %workspace.path.display(),
            hook = "before_run",
            "running workspace hook"
        );
        run_hook(
            script,
            &workspace.path,
            "before_run",
            config.hooks.timeout_ms,
        )
        .await?;
    }

    Ok(())
}

pub async fn run_after_run_hook(config: &EffectiveConfig, workspace: &Workspace, issue: &Issue) {
    if let Some(script) = &config.hooks.after_run {
        if let Err(error) = run_hook(
            script,
            &workspace.path,
            "after_run",
            config.hooks.timeout_ms,
        )
        .await
        {
            warn!(
                issue_id = %issue.id,
                issue_identifier = %issue.identifier,
                workspace = %workspace.path.display(),
                error = %error,
                "after_run hook failed and was ignored"
            );
        }
    }
}

pub async fn remove_issue_workspace(
    config: &EffectiveConfig,
    issue_identifier: &str,
) -> Result<(), WorkspaceError> {
    let workspace_key = sanitize_identifier(issue_identifier);
    let workspace_path = validate_workspace_path(
        &config.workspace.root,
        &config.workspace.root.join(&workspace_key),
    )?;
    remove_workspace_path(config, &workspace_path).await
}

pub async fn remove_workspace_path(
    config: &EffectiveConfig,
    workspace_path: &Path,
) -> Result<(), WorkspaceError> {
    let workspace_path = workspace_path.to_path_buf();

    if tokio::fs::try_exists(&workspace_path)
        .await
        .map_err(|error| WorkspaceError::Io(error.to_string()))?
    {
        if let Some(script) = &config.hooks.before_remove {
            if let Err(error) = run_hook(
                script,
                &workspace_path,
                "before_remove",
                config.hooks.timeout_ms,
            )
            .await
            {
                warn!(
                    workspace = %workspace_path.display(),
                    error = %error,
                    "before_remove hook failed and was ignored"
                );
            }
        }
        tokio::fs::remove_dir_all(&workspace_path)
            .await
            .map_err(|error| WorkspaceError::Io(error.to_string()))?;
    }

    Ok(())
}

/// Summary of a batch workspace removal operation across multiple hosts.
#[derive(Debug, Clone, Default)]
pub struct RemovalSummary {
    /// Hosts (or "local") where removal succeeded.
    pub successes: Vec<String>,
    /// `(host_or_local, error_message)` pairs for hosts where removal failed.
    pub failures: Vec<(String, String)>,
}

impl RemovalSummary {
    /// Returns `true` when every target succeeded.
    pub fn all_succeeded(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Remove the workspace for a given issue identifier across all configured
/// worker hosts (and locally when no remote hosts are configured).
///
/// For local-only setups: removes the directory at `workspace_root/sanitized_identifier`.
/// For remote hosts: calls [`remove_remote`] for each host.
///
/// Errors on individual hosts are logged but do not cause the overall operation
/// to fail. The returned [`RemovalSummary`] reports which hosts succeeded and
/// which failed.
pub async fn remove_issue_workspaces(
    identifier: &str,
    workspace_root: &Path,
    worker_hosts: &[String],
    timeout_ms: u64,
) -> RemovalSummary {
    let safe_id = sanitize_identifier(identifier);
    let mut summary = RemovalSummary::default();

    if worker_hosts.is_empty() {
        // Local-only: remove the directory under workspace_root.
        let workspace_path = workspace_root.join(&safe_id);
        match tokio::fs::try_exists(&workspace_path).await {
            Ok(true) => {
                if let Err(err) = tokio::fs::remove_dir_all(&workspace_path).await {
                    warn!(
                        identifier = identifier,
                        workspace = %workspace_path.display(),
                        error = %err,
                        "failed to remove local workspace"
                    );
                    summary
                        .failures
                        .push(("local".to_owned(), err.to_string()));
                } else {
                    info!(
                        identifier = identifier,
                        workspace = %workspace_path.display(),
                        "removed local workspace"
                    );
                    summary.successes.push("local".to_owned());
                }
            }
            Ok(false) => {
                // Nothing to remove.
                summary.successes.push("local".to_owned());
            }
            Err(err) => {
                warn!(
                    identifier = identifier,
                    workspace = %workspace_path.display(),
                    error = %err,
                    "failed to check local workspace existence"
                );
                summary
                    .failures
                    .push(("local".to_owned(), err.to_string()));
            }
        }
    } else {
        // Remote hosts: remove workspace on each host.
        for host in worker_hosts {
            let remote_path = workspace_root.join(&safe_id);
            let remote_path_str = remote_path.to_string_lossy();

            match remove_remote(host, &remote_path_str, timeout_ms).await {
                Ok(()) => {
                    info!(
                        identifier = identifier,
                        host = host.as_str(),
                        workspace = %remote_path_str,
                        "removed remote workspace"
                    );
                    summary.successes.push(host.clone());
                }
                Err(err) => {
                    warn!(
                        identifier = identifier,
                        host = host.as_str(),
                        workspace = %remote_path_str,
                        error = %err,
                        "failed to remove remote workspace"
                    );
                    summary.failures.push((host.clone(), err.to_string()));
                }
            }
        }
    }

    summary
}

pub fn sanitize_identifier(identifier: &str) -> String {
    let sanitized: String = identifier
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() {
        "issue".to_owned()
    } else {
        sanitized
    }
}

async fn cleanup_temp_artifacts(workspace: &Path) -> Result<(), WorkspaceError> {
    for artifact in ["tmp", ".elixir_ls"] {
        let path = workspace.join(artifact);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.is_dir() => {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            }
            Ok(_) => {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|error| WorkspaceError::Io(error.to_string()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(WorkspaceError::Io(error.to_string())),
        }
    }

    Ok(())
}

async fn run_hook(
    script: &str,
    workspace: &Path,
    hook: &'static str,
    timeout_ms: u64,
) -> Result<(), WorkspaceError> {
    let mut command = Command::new("sh");
    command.arg("-lc").arg(script);
    command.current_dir(workspace);
    command.kill_on_drop(true);

    let output = timeout(Duration::from_millis(timeout_ms), command.output())
        .await
        .map_err(|_| WorkspaceError::HookTimeout { hook, timeout_ms })?
        .map_err(|error| WorkspaceError::HookFailed {
            hook,
            reason: error.to_string(),
        })?;

    if output.status.success() {
        if !output.stdout.is_empty() || !output.stderr.is_empty() {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            info!(
                workspace = %workspace.display(),
                hook = hook,
                output = %truncate_for_log(combined.trim(), 1_000),
                "workspace hook completed"
            );
        }
        Ok(())
    } else {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        error!(
            workspace = %workspace.display(),
            hook = hook,
            status = ?output.status.code(),
            output = %truncate_for_log(combined.trim(), 1_000),
            "workspace hook failed"
        );
        Err(WorkspaceError::HookFailed {
            hook,
            reason: format!(
                "status={:?} output={}",
                output.status.code(),
                truncate_for_log(combined.trim(), 1_000)
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Remote workspace operations (via SSH)
// ---------------------------------------------------------------------------

/// Marker token emitted by the remote workspace creation script, used to
/// identify the structured output line among arbitrary shell noise.
const REMOTE_WORKSPACE_MARKER: &str = "__SYMPHONY_WORKSPACE__";

/// Create a workspace directory on a remote host via SSH.
///
/// Returns `(resolved_path, created_now)` where `resolved_path` is the
/// canonical path on the remote machine and `created_now` indicates whether
/// the directory was freshly created.
pub async fn create_remote(
    host: &str,
    path: &str,
    timeout_ms: u64,
) -> Result<(String, bool), WorkspaceError> {
    let script = format!(
        r#"set -eu
{assign}
if [ -d "$workspace" ]; then
  created=0
elif [ -e "$workspace" ]; then
  rm -rf "$workspace"
  mkdir -p "$workspace"
  created=1
else
  mkdir -p "$workspace"
  created=1
fi
cd "$workspace"
printf '%s\t%s\t%s\n' '{marker}' "$created" "$(pwd -P)""#,
        assign = remote_shell_assign("workspace", path),
        marker = REMOTE_WORKSPACE_MARKER,
    );

    let output = ssh::run_with_timeout(host, &script, timeout_ms)
        .await
        .map_err(|error| WorkspaceError::Ssh(error.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(WorkspaceError::RemoteFailed {
            host: host.to_owned(),
            reason: format!(
                "status={:?} output={}",
                output.status.code(),
                truncate_for_log(
                    format!("{}{}", stdout, stderr).trim(),
                    2_048,
                )
                .as_str(),
            ),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_remote_workspace_output(&stdout).map_err(|reason| WorkspaceError::RemoteFailed {
        host: host.to_owned(),
        reason,
    })
}

/// Remove a workspace directory on a remote host via SSH.
pub async fn remove_remote(
    host: &str,
    path: &str,
    timeout_ms: u64,
) -> Result<(), WorkspaceError> {
    let script = format!(
        "{assign}\nrm -rf \"$workspace\"",
        assign = remote_shell_assign("workspace", path),
    );

    let output = ssh::run_with_timeout(host, &script, timeout_ms)
        .await
        .map_err(|error| WorkspaceError::Ssh(error.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(WorkspaceError::RemoteFailed {
            host: host.to_owned(),
            reason: format!(
                "remove failed: status={:?} output={}",
                output.status.code(),
                truncate_for_log(
                    format!("{}{}", stdout, stderr).trim(),
                    2_048,
                )
                .as_str(),
            ),
        });
    }

    Ok(())
}

/// Run a workspace hook (e.g. `after_create`, `before_run`) on a remote host
/// via SSH. The hook command is executed with the workspace directory as the
/// working directory.
pub async fn run_hook_remote(
    host: &str,
    hook_command: &str,
    workspace_path: &str,
    hook_name: &'static str,
    timeout_ms: u64,
) -> Result<(), WorkspaceError> {
    info!(
        host = host,
        workspace = workspace_path,
        hook = hook_name,
        "running remote workspace hook"
    );

    let command = format!(
        "cd {} && {}",
        ssh::shell_escape(workspace_path),
        hook_command,
    );

    let output = ssh::run_with_timeout(host, &command, timeout_ms)
        .await
        .map_err(|error| match error {
            ssh::SshError::Timeout(ms) => WorkspaceError::HookTimeout {
                hook: hook_name,
                timeout_ms: ms,
            },
            other => WorkspaceError::Ssh(other.to_string()),
        })?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}{}", stdout, stderr);
        if !combined.trim().is_empty() {
            info!(
                host = host,
                workspace = workspace_path,
                hook = hook_name,
                output = %truncate_for_log(combined.trim(), 1_000),
                "remote workspace hook completed"
            );
        }
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{}{}", stdout, stderr);
        error!(
            host = host,
            workspace = workspace_path,
            hook = hook_name,
            status = ?output.status.code(),
            output = %truncate_for_log(combined.trim(), 1_000),
            "remote workspace hook failed"
        );
        Err(WorkspaceError::HookFailed {
            hook: hook_name,
            reason: format!(
                "host={} status={:?} output={}",
                host,
                output.status.code(),
                truncate_for_log(combined.trim(), 1_000),
            ),
        })
    }
}

/// Generate a shell snippet that assigns a variable and expands `~` to
/// `$HOME`, matching the Elixir `remote_shell_assign/2` helper.
fn remote_shell_assign(variable_name: &str, raw_path: &str) -> String {
    format!(
        r#"{var}={escaped}
case "${var}" in
  '~') {var}="$HOME" ;;
  '~/'*) {var}="$HOME/${{{var}#~/}}" ;;
esac"#,
        var = variable_name,
        escaped = ssh::shell_escape(raw_path),
    )
}

/// Parse the structured output from the remote workspace creation script.
///
/// Looks for a line with format:
///   `__SYMPHONY_WORKSPACE__<TAB>0|1<TAB>/resolved/path`
fn parse_remote_workspace_output(output: &str) -> Result<(String, bool), String> {
    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() == 3
            && parts[0] == REMOTE_WORKSPACE_MARKER
            && (parts[1] == "0" || parts[1] == "1")
            && !parts[2].is_empty()
        {
            let created = parts[1] == "1";
            return Ok((parts[2].to_owned(), created));
        }
    }

    Err(format!(
        "invalid remote workspace output: {}",
        truncate_for_log(output.trim(), 512),
    ))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::config::EffectiveConfig;

    fn base_config(root: &Path) -> EffectiveConfig {
        EffectiveConfig {
            tracker: crate::config::TrackerConfig {
                kind: Some("linear".to_owned()),
                endpoint: "https://api.linear.app/graphql".to_owned(),
                api_key: Some("token".to_owned()),
                project_slug: Some("demo".to_owned()),
                assignee: None,
                active_states: vec!["Todo".to_owned()],
                terminal_states: vec!["Done".to_owned()],
            },
            polling: crate::config::PollingConfig {
                interval_ms: 30_000,
            },
            workspace: crate::config::WorkspaceConfig {
                root: root.to_path_buf(),
            },
            hooks: crate::config::HooksConfig {
                after_create: None,
                before_run: None,
                after_run: None,
                before_remove: None,
                timeout_ms: 1_000,
            },
            agent: crate::config::AgentConfig {
                max_concurrent_agents: 1,
                max_turns: 1,
                max_retry_backoff_ms: 1_000,
                max_concurrent_agents_by_state: Default::default(),
            },
            codex: crate::config::CodexConfig {
                command: "codex app-server".to_owned(),
                approval_policy: serde_json::Value::String("never".to_owned()),
                thread_sandbox: "workspace-write".to_owned(),
                turn_sandbox_policy: None,
                turn_timeout_ms: 1_000,
                read_timeout_ms: 1_000,
                stall_timeout_ms: 1_000,
            },
            server: crate::config::ServerConfig::default(),
            worker: crate::worker::WorkerConfig::default(),
            observability: crate::config::ObservabilityConfig {
                dashboard_enabled: true,
                refresh_ms: 1_000,
                render_interval_ms: 16,
            },
        }
    }

    #[tokio::test]
    async fn creates_and_reuses_workspaces() {
        let dir = TempDir::new().unwrap();
        let config = base_config(dir.path());
        let first = prepare_workspace(&config, "ABC-1").await.unwrap();
        assert!(first.created_now);
        let second = prepare_workspace(&config, "ABC-1").await.unwrap();
        assert!(!second.created_now);
    }

    #[test]
    fn parse_remote_workspace_output_created() {
        let output = "some noise\n__SYMPHONY_WORKSPACE__\t1\t/home/user/ws/issue-1\nmore noise\n";
        let (path, created) = parse_remote_workspace_output(output).unwrap();
        assert_eq!(path, "/home/user/ws/issue-1");
        assert!(created);
    }

    #[test]
    fn parse_remote_workspace_output_existing() {
        let output = "__SYMPHONY_WORKSPACE__\t0\t/tmp/ws/abc\n";
        let (path, created) = parse_remote_workspace_output(output).unwrap();
        assert_eq!(path, "/tmp/ws/abc");
        assert!(!created);
    }

    #[test]
    fn parse_remote_workspace_output_invalid() {
        let output = "no marker here\n";
        assert!(parse_remote_workspace_output(output).is_err());
    }

    #[test]
    fn parse_remote_workspace_output_empty_path_rejected() {
        let output = "__SYMPHONY_WORKSPACE__\t1\t\n";
        assert!(parse_remote_workspace_output(output).is_err());
    }

    #[test]
    fn remote_shell_assign_expands_tilde() {
        let snippet = remote_shell_assign("workspace", "~/projects/ws");
        assert!(snippet.contains("workspace="));
        assert!(snippet.contains("$HOME"));
    }

    #[tokio::test]
    async fn remove_issue_workspaces_local_removes_directory() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let ws_path = root.join("ABC-1");
        tokio::fs::create_dir_all(&ws_path).await.unwrap();
        tokio::fs::write(ws_path.join("file.txt"), b"data")
            .await
            .unwrap();

        let summary =
            remove_issue_workspaces("ABC-1", root, &[], 5_000).await;
        assert!(summary.all_succeeded());
        assert_eq!(summary.successes, vec!["local"]);
        assert!(!ws_path.exists());
    }

    #[tokio::test]
    async fn remove_issue_workspaces_local_nonexistent_succeeds() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        let summary =
            remove_issue_workspaces("NONEXIST-1", root, &[], 5_000).await;
        assert!(summary.all_succeeded());
        assert_eq!(summary.successes, vec!["local"]);
    }

    #[tokio::test]
    async fn remove_issue_workspaces_sanitizes_identifier() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // "AB/CD" sanitizes to "AB_CD"
        let ws_path = root.join("AB_CD");
        tokio::fs::create_dir_all(&ws_path).await.unwrap();

        let summary =
            remove_issue_workspaces("AB/CD", root, &[], 5_000).await;
        assert!(summary.all_succeeded());
        assert!(!ws_path.exists());
    }

    #[test]
    fn removal_summary_all_succeeded() {
        let mut summary = RemovalSummary::default();
        assert!(summary.all_succeeded());
        summary.successes.push("local".to_owned());
        assert!(summary.all_succeeded());
        summary
            .failures
            .push(("host-a".to_owned(), "timeout".to_owned()));
        assert!(!summary.all_succeeded());
    }
}
