use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub tracker: TrackerConfig,
    pub polling: PollingConfig,
    pub workspace: WorkspaceConfig,
    pub hooks: HooksConfig,
    pub agent: AgentConfig,
    pub codex: CodexConfig,
    pub server: ServerConfig,
}

#[derive(Debug, Clone)]
pub struct TrackerConfig {
    pub kind: Option<String>,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub project_slug: Option<String>,
    pub assignee: Option<String>,
    pub active_states: Vec<String>,
    pub terminal_states: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PollingConfig {
    pub interval_ms: u64,
}

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct HooksConfig {
    pub after_create: Option<String>,
    pub before_run: Option<String>,
    pub after_run: Option<String>,
    pub before_remove: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_concurrent_agents: usize,
    pub max_turns: u32,
    pub max_retry_backoff_ms: u64,
    pub max_concurrent_agents_by_state: HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct CodexConfig {
    pub command: String,
    pub approval_policy: JsonValue,
    pub thread_sandbox: String,
    pub turn_sandbox_policy: Option<JsonValue>,
    pub turn_timeout_ms: u64,
    pub read_timeout_ms: u64,
    pub stall_timeout_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ServerConfig {
    pub port: Option<u16>,
}

/// CLI-level overrides that take precedence over WORKFLOW.md values.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub api_key: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid_workflow_config: {0}")]
    InvalidWorkflowConfig(String),
    #[error("missing_tracker_kind")]
    MissingTrackerKind,
    #[error("unsupported_tracker_kind: {0}")]
    UnsupportedTrackerKind(String),
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    #[error("missing_codex_command")]
    MissingCodexCommand,
    #[error("invalid_workspace_root: {0}")]
    InvalidWorkspaceRoot(String),
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    tracker: RawTrackerConfig,
    polling: RawPollingConfig,
    workspace: RawWorkspaceConfig,
    hooks: RawHooksConfig,
    agent: RawAgentConfig,
    codex: RawCodexConfig,
    server: RawServerConfig,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawTrackerConfig {
    kind: Option<String>,
    endpoint: Option<String>,
    api_key: Option<String>,
    project_slug: Option<String>,
    assignee: Option<String>,
    active_states: Option<Vec<String>>,
    terminal_states: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawPollingConfig {
    interval_ms: Option<FlexibleInt>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawWorkspaceConfig {
    root: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawHooksConfig {
    after_create: Option<String>,
    before_run: Option<String>,
    after_run: Option<String>,
    before_remove: Option<String>,
    timeout_ms: Option<FlexibleInt>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawAgentConfig {
    max_concurrent_agents: Option<FlexibleInt>,
    max_turns: Option<FlexibleInt>,
    max_retry_backoff_ms: Option<FlexibleInt>,
    max_concurrent_agents_by_state: HashMap<String, JsonValue>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawCodexConfig {
    command: Option<String>,
    approval_policy: Option<JsonValue>,
    thread_sandbox: Option<String>,
    turn_sandbox_policy: Option<JsonValue>,
    turn_timeout_ms: Option<FlexibleInt>,
    read_timeout_ms: Option<FlexibleInt>,
    stall_timeout_ms: Option<FlexibleInt>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct RawServerConfig {
    port: Option<FlexibleInt>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum FlexibleInt {
    Integer(i64),
    String(String),
}

impl FlexibleInt {
    fn parse_i64(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            Self::String(value) => value.trim().parse::<i64>().ok(),
        }
    }
}

impl EffectiveConfig {
    pub fn from_workflow_config(config: &serde_yaml::Mapping) -> Result<Self, ConfigError> {
        let raw: RawConfig = serde_yaml::from_value(serde_yaml::Value::Mapping(config.clone()))
            .map_err(|error| ConfigError::InvalidWorkflowConfig(error.to_string()))?;

        let tracker = TrackerConfig {
            kind: raw.tracker.kind.map(|value| value.trim().to_owned()),
            endpoint: raw
                .tracker
                .endpoint
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "https://api.linear.app/graphql".to_owned()),
            api_key: resolve_tracker_api_key(raw.tracker.api_key),
            project_slug: raw
                .tracker
                .project_slug
                .and_then(|value| normalize_secret(value)),
            assignee: resolve_tracker_assignee(raw.tracker.assignee),
            active_states: raw
                .tracker
                .active_states
                .filter(|states| !states.is_empty())
                .unwrap_or_else(|| vec!["Todo".to_owned(), "In Progress".to_owned()]),
            terminal_states: raw
                .tracker
                .terminal_states
                .filter(|states| !states.is_empty())
                .unwrap_or_else(|| {
                    vec![
                        "Closed".to_owned(),
                        "Cancelled".to_owned(),
                        "Canceled".to_owned(),
                        "Duplicate".to_owned(),
                        "Done".to_owned(),
                    ]
                }),
        };

        let workspace_root = resolve_workspace_root(raw.workspace.root)?;

        let mut state_limits = HashMap::new();
        for (state_name, limit_value) in raw.agent.max_concurrent_agents_by_state {
            let normalized = normalize_issue_state(&state_name);
            if normalized.is_empty() {
                continue;
            }

            let parsed = match limit_value {
                JsonValue::Number(number) => number
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok()),
                JsonValue::String(text) => text.trim().parse::<usize>().ok(),
                _ => None,
            };

            if let Some(limit) = parsed.filter(|value| *value > 0) {
                state_limits.insert(normalized, limit);
            }
        }

        let agent = AgentConfig {
            max_concurrent_agents: parse_positive_usize(raw.agent.max_concurrent_agents.as_ref())
                .unwrap_or(10),
            max_turns: parse_positive_u32(raw.agent.max_turns.as_ref()).unwrap_or(20),
            max_retry_backoff_ms: parse_positive_u64(raw.agent.max_retry_backoff_ms.as_ref())
                .unwrap_or(300_000),
            max_concurrent_agents_by_state: state_limits,
        };

        let hooks = HooksConfig {
            after_create: raw
                .hooks
                .after_create
                .filter(|value| !value.trim().is_empty()),
            before_run: raw
                .hooks
                .before_run
                .filter(|value| !value.trim().is_empty()),
            after_run: raw.hooks.after_run.filter(|value| !value.trim().is_empty()),
            before_remove: raw
                .hooks
                .before_remove
                .filter(|value| !value.trim().is_empty()),
            timeout_ms: parse_positive_u64(raw.hooks.timeout_ms.as_ref()).unwrap_or(60_000),
        };

        let codex = CodexConfig {
            command: raw
                .codex
                .command
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "codex app-server".to_owned()),
            approval_policy: raw.codex.approval_policy.unwrap_or_else(|| {
                serde_json::json!({
                    "reject": {
                        "sandbox_approval": true,
                        "rules": true,
                        "mcp_elicitations": true
                    }
                })
            }),
            thread_sandbox: raw
                .codex
                .thread_sandbox
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "workspace-write".to_owned()),
            turn_sandbox_policy: raw.codex.turn_sandbox_policy,
            turn_timeout_ms: parse_positive_u64(raw.codex.turn_timeout_ms.as_ref())
                .unwrap_or(3_600_000),
            read_timeout_ms: parse_positive_u64(raw.codex.read_timeout_ms.as_ref())
                .unwrap_or(5_000),
            stall_timeout_ms: raw
                .codex
                .stall_timeout_ms
                .as_ref()
                .and_then(FlexibleInt::parse_i64)
                .unwrap_or(300_000),
        };

        let server = ServerConfig {
            port: raw
                .server
                .port
                .as_ref()
                .and_then(FlexibleInt::parse_i64)
                .and_then(|value| u16::try_from(value).ok()),
        };

        Ok(Self {
            tracker,
            polling: PollingConfig {
                interval_ms: parse_positive_u64(raw.polling.interval_ms.as_ref()).unwrap_or(30_000),
            },
            workspace: WorkspaceConfig {
                root: workspace_root,
            },
            hooks,
            agent,
            codex,
            server,
        })
    }

    pub fn apply_overrides(&mut self, overrides: &CliOverrides) {
        if let Some(ref api_key) = overrides.api_key {
            self.tracker.api_key = Some(api_key.clone());
        }
        if let Some(port) = overrides.port {
            self.server.port = Some(port);
        }
    }

    pub fn validate_dispatch(&self) -> Result<(), ConfigError> {
        match self.tracker.kind.as_deref() {
            Some("linear") => {}
            Some(kind) => return Err(ConfigError::UnsupportedTrackerKind(kind.to_owned())),
            None => return Err(ConfigError::MissingTrackerKind),
        }

        if self.tracker.api_key.is_none() {
            return Err(ConfigError::MissingTrackerApiKey);
        }

        if self
            .tracker
            .project_slug
            .as_ref()
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(ConfigError::MissingTrackerProjectSlug);
        }

        if self.codex.command.trim().is_empty() {
            return Err(ConfigError::MissingCodexCommand);
        }

        Ok(())
    }

    pub fn normalized_active_states(&self) -> Vec<String> {
        self.tracker
            .active_states
            .iter()
            .map(|state| normalize_issue_state(state))
            .filter(|state| !state.is_empty())
            .collect()
    }

    pub fn normalized_terminal_states(&self) -> Vec<String> {
        self.tracker
            .terminal_states
            .iter()
            .map(|state| normalize_issue_state(state))
            .filter(|state| !state.is_empty())
            .collect()
    }

    pub fn max_concurrent_agents_for_state(&self, state: &str) -> usize {
        let normalized = normalize_issue_state(state);
        self.agent
            .max_concurrent_agents_by_state
            .get(&normalized)
            .copied()
            .unwrap_or(self.agent.max_concurrent_agents)
    }

    pub fn resolved_turn_sandbox_policy(
        &self,
        workspace: Option<&Path>,
    ) -> Result<JsonValue, ConfigError> {
        if let Some(policy) = &self.codex.turn_sandbox_policy {
            return Ok(policy.clone());
        }

        let writable_root = workspace
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace.root.clone());

        if writable_root.as_os_str().is_empty() {
            return Err(ConfigError::InvalidWorkspaceRoot(
                "workspace root is empty".to_owned(),
            ));
        }

        Ok(JsonValue::Object(JsonMap::from_iter([
            (
                "type".to_owned(),
                JsonValue::String("workspaceWrite".to_owned()),
            ),
            (
                "writableRoots".to_owned(),
                JsonValue::Array(vec![JsonValue::String(
                    writable_root.to_string_lossy().into_owned(),
                )]),
            ),
            (
                "readOnlyAccess".to_owned(),
                JsonValue::Object(JsonMap::from_iter([(
                    "type".to_owned(),
                    JsonValue::String("fullAccess".to_owned()),
                )])),
            ),
            ("networkAccess".to_owned(), JsonValue::Bool(false)),
            ("excludeTmpdirEnvVar".to_owned(), JsonValue::Bool(false)),
            ("excludeSlashTmp".to_owned(), JsonValue::Bool(false)),
        ])))
    }
}

pub fn normalize_issue_state(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn resolve_tracker_api_key(configured: Option<String>) -> Option<String> {
    match configured {
        Some(value) => match env_reference_name(&value) {
            Some(env_name) => env::var(env_name).ok().and_then(normalize_secret),
            None => normalize_secret(value),
        },
        None => env::var("LINEAR_API_KEY").ok().and_then(normalize_secret),
    }
}

fn resolve_tracker_assignee(configured: Option<String>) -> Option<String> {
    match configured {
        Some(value) => match env_reference_name(&value) {
            Some(env_name) => env::var(env_name).ok().and_then(normalize_secret),
            None => normalize_secret(value),
        },
        None => env::var("LINEAR_ASSIGNEE").ok().and_then(normalize_secret),
    }
}

fn resolve_workspace_root(configured: Option<String>) -> Result<PathBuf, ConfigError> {
    let default_root = env::temp_dir().join("symphony_workspaces");
    let raw = match configured {
        Some(value) => match env_reference_name(&value) {
            Some(env_name) => match env::var(env_name) {
                Ok(env_value) if !env_value.trim().is_empty() => env_value,
                _ => return Ok(default_root),
            },
            None => value,
        },
        None => return Ok(default_root),
    };

    let expanded = expand_tilde(raw.trim());
    if expanded.is_empty() {
        return Ok(default_root);
    }

    Ok(PathBuf::from(expanded))
}

fn normalize_secret(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn expand_tilde(value: &str) -> String {
    if value == "~" {
        return home_dir().unwrap_or_else(|| "~".to_owned());
    }

    if let Some(remainder) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return PathBuf::from(home)
                .join(remainder)
                .to_string_lossy()
                .into_owned();
        }
    }

    value.to_owned()
}

fn home_dir() -> Option<String> {
    env::var("HOME").ok()
}

fn env_reference_name(value: &str) -> Option<&str> {
    let remainder = value.strip_prefix('$')?;
    if remainder.is_empty() {
        return None;
    }

    let mut chars = remainder.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }

    if chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        Some(remainder)
    } else {
        None
    }
}

fn parse_positive_u64(value: Option<&FlexibleInt>) -> Option<u64> {
    value
        .and_then(FlexibleInt::parse_i64)
        .and_then(|raw| u64::try_from(raw).ok())
        .filter(|raw| *raw > 0)
}

fn parse_positive_usize(value: Option<&FlexibleInt>) -> Option<usize> {
    value
        .and_then(FlexibleInt::parse_i64)
        .and_then(|raw| usize::try_from(raw).ok())
        .filter(|raw| *raw > 0)
}

fn parse_positive_u32(value: Option<&FlexibleInt>) -> Option<u32> {
    value
        .and_then(FlexibleInt::parse_i64)
        .and_then(|raw| u32::try_from(raw).ok())
        .filter(|raw| *raw > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_and_state_limit_normalization() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
agent:
  max_concurrent_agents_by_state:
    In Progress: "2"
    Invalid: "zero"
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();
        assert_eq!(config.tracker.endpoint, "https://api.linear.app/graphql");
        assert_eq!(config.max_concurrent_agents_for_state("in progress"), 2);
        assert_eq!(config.agent.max_concurrent_agents_by_state.len(), 1);
    }

    #[test]
    fn workspace_root_env_indirection_falls_back_to_default() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
workspace:
  root: $MISSING_VAR
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();
        assert!(config.workspace.root.ends_with("symphony_workspaces"));
    }

    #[test]
    fn assignee_field_parsed_from_config() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
  assignee: me
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();
        assert_eq!(config.tracker.assignee.as_deref(), Some("me"));
    }

    #[test]
    fn assignee_field_defaults_to_none() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();
        // Without LINEAR_ASSIGNEE env var set, default should be None
        // (this test assumes LINEAR_ASSIGNEE is not set in CI)
        if std::env::var("LINEAR_ASSIGNEE").is_err() {
            assert_eq!(config.tracker.assignee, None);
        }
    }

    #[test]
    fn assignee_field_literal_user_id() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
  assignee: "abc-123-user-id"
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();
        assert_eq!(config.tracker.assignee.as_deref(), Some("abc-123-user-id"));
    }

    #[test]
    fn default_approval_policy_is_reject_object() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();

        // The default should be the safer reject object, not the string "never".
        assert!(config.codex.approval_policy.is_object());

        let reject = config.codex.approval_policy.get("reject").expect("missing reject key");
        assert_eq!(reject.get("sandbox_approval"), Some(&JsonValue::Bool(true)));
        assert_eq!(reject.get("rules"), Some(&JsonValue::Bool(true)));
        assert_eq!(reject.get("mcp_elicitations"), Some(&JsonValue::Bool(true)));

        // With an object default, auto_approve_requests should be false.
        let is_auto_approve = matches!(
            config.codex.approval_policy,
            JsonValue::String(ref v) if v == "never"
        );
        assert!(!is_auto_approve);
    }

    #[test]
    fn explicit_never_approval_policy_enables_auto_approve() {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: linear
  project_slug: demo
codex:
  approval_policy: never
"#,
        )
        .unwrap();

        let config = EffectiveConfig::from_workflow_config(&yaml).unwrap();

        // Explicitly setting "never" should still work.
        assert!(matches!(
            config.codex.approval_policy,
            JsonValue::String(ref v) if v == "never"
        ));
    }
}
