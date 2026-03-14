use std::path::{Path, PathBuf};

use thiserror::Error;
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use tracing::{error, info, warn};

use crate::config::EffectiveConfig;
use crate::error::truncate_for_log;
use crate::issue::Issue;
use crate::path_safety::{PathSafetyError, validate_workspace_path};

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
}
