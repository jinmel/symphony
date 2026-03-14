use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;
use tokio::time::{self, Duration};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::EffectiveConfig;
use crate::error::truncate_for_log;
use crate::issue::Issue;
use crate::path_safety::validate_workspace_path;
use crate::tracker::{TrackerError, execute_raw_graphql};

const INITIALIZE_ID: i64 = 1;
const THREAD_START_ID: i64 = 2;
const TURN_START_ID: i64 = 3;
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AgentUpdate {
    pub event: String,
    pub timestamp: DateTime<Utc>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub codex_app_server_pid: Option<String>,
    pub usage: Option<Value>,
    pub rate_limits: Option<Value>,
    pub raw_payload: Option<Value>,
    pub raw_line: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TurnResult {
    pub session_id: String,
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("codex_not_found: {0}")]
    CodexNotFound(String),
    #[error("invalid_workspace_cwd: {0}")]
    InvalidWorkspaceCwd(String),
    #[error("response_timeout")]
    ResponseTimeout,
    #[error("turn_timeout")]
    TurnTimeout,
    #[error("port_exit: {0}")]
    PortExit(String),
    #[error("response_error: {0}")]
    ResponseError(String),
    #[error("turn_failed: {0}")]
    TurnFailed(String),
    #[error("turn_cancelled: {0}")]
    TurnCancelled(String),
    #[error("turn_input_required")]
    TurnInputRequired,
    #[error("approval_required")]
    ApprovalRequired,
    #[error("cancelled")]
    Cancelled,
    #[error("io: {0}")]
    Io(String),
}

pub struct AppServerSession {
    child: Child,
    stdin: ChildStdin,
    stdout: FramedRead<tokio::process::ChildStdout, LinesCodec>,
    stderr_task: JoinHandle<()>,
    workspace: PathBuf,
    thread_id: String,
    codex_app_server_pid: Option<String>,
    approval_policy: Value,
    thread_sandbox: String,
    turn_sandbox_policy: Value,
    auto_approve_requests: bool,
    config: EffectiveConfig,
}

impl AppServerSession {
    pub async fn start(
        config: &EffectiveConfig,
        workspace: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, AgentError> {
        let validated_workspace = validate_workspace_path(&config.workspace.root, workspace)
            .map_err(|error| AgentError::InvalidWorkspaceCwd(error.to_string()))?;

        let mut command = Command::new("bash");
        command.arg("-lc").arg(&config.codex.command);
        command.current_dir(&validated_workspace);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|error| AgentError::CodexNotFound(error.to_string()))?;

        let codex_app_server_pid = child.id().map(|pid| pid.to_string());
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Io("missing stdin".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Io("missing stdout".to_owned()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AgentError::Io("missing stderr".to_owned()))?;

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let message = truncate_for_log(line.trim(), 1_000);
                if message.to_ascii_lowercase().contains("error")
                    || message.to_ascii_lowercase().contains("warn")
                {
                    warn!(line = %message, "codex stderr");
                } else {
                    debug!(line = %message, "codex stderr");
                }
            }
        });

        let mut session = Self {
            child,
            stdin,
            stdout: FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_BYTES)),
            stderr_task,
            workspace: validated_workspace.clone(),
            thread_id: String::new(),
            codex_app_server_pid,
            approval_policy: config.codex.approval_policy.clone(),
            thread_sandbox: config.codex.thread_sandbox.clone(),
            turn_sandbox_policy: config
                .resolved_turn_sandbox_policy(Some(&validated_workspace))
                .map_err(|error| AgentError::InvalidWorkspaceCwd(error.to_string()))?,
            auto_approve_requests: matches!(
                config.codex.approval_policy,
                Value::String(ref value) if value == "never"
            ),
            config: config.clone(),
        };

        let result = async {
            session
                .send_message(json!({
                    "id": INITIALIZE_ID,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {
                            "name": "symphony",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "capabilities": {}
                    }
                }))
                .await?;
            session.await_response(INITIALIZE_ID, cancellation).await?;
            session
                .send_message(json!({
                    "method": "initialized",
                    "params": {}
                }))
                .await?;

            session
                .send_message(json!({
                    "id": THREAD_START_ID,
                    "method": "thread/start",
                    "params": {
                        "approvalPolicy": session.approval_policy,
                        "sandbox": session.thread_sandbox,
                        "cwd": session.workspace.to_string_lossy().into_owned(),
                        "dynamicTools": dynamic_tool_specs()
                    }
                }))
                .await?;

            let response = session
                .await_response(THREAD_START_ID, cancellation)
                .await?;
            let thread_id = response
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::ResponseError(response.to_string()))?;
            session.thread_id = thread_id.to_owned();
            Ok(())
        }
        .await;

        match result {
            Ok(()) => Ok(session),
            Err(error) => {
                session.stop().await;
                Err(error)
            }
        }
    }

    pub async fn run_turn<F>(
        &mut self,
        prompt: &str,
        issue: &Issue,
        cancellation: &CancellationToken,
        mut on_update: F,
    ) -> Result<TurnResult, AgentError>
    where
        F: FnMut(AgentUpdate) + Send,
    {
        self.send_message(json!({
            "id": TURN_START_ID,
            "method": "turn/start",
            "params": {
                "threadId": self.thread_id,
                "input": [{ "type": "text", "text": prompt }],
                "cwd": self.workspace.to_string_lossy().into_owned(),
                "title": format!("{}: {}", issue.identifier, issue.title),
                "approvalPolicy": self.approval_policy,
                "sandboxPolicy": self.turn_sandbox_policy
            }
        }))
        .await?;

        let response = self.await_response(TURN_START_ID, cancellation).await?;
        let turn_id = response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::ResponseError(response.to_string()))?
            .to_owned();
        let session_id = format!("{}-{}", self.thread_id, turn_id);

        on_update(AgentUpdate {
            event: "session_started".to_owned(),
            timestamp: Utc::now(),
            session_id: Some(session_id.clone()),
            thread_id: Some(self.thread_id.clone()),
            turn_id: Some(turn_id.clone()),
            codex_app_server_pid: self.codex_app_server_pid.clone(),
            usage: None,
            rate_limits: None,
            raw_payload: Some(response),
            raw_line: None,
        });

        let deadline = Instant::now() + Duration::from_millis(self.config.codex.turn_timeout_ms);

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| AgentError::Io(error.to_string()))?
            {
                return Err(AgentError::PortExit(
                    status
                        .code()
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| "signal".to_owned()),
                ));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(AgentError::TurnTimeout);
            }

            let timeout_duration = deadline.saturating_duration_since(now);
            let next_line = time::timeout(timeout_duration, self.stdout.next())
                .await
                .map_err(|_| AgentError::TurnTimeout)?;

            let Some(line) = next_line else {
                return Err(AgentError::PortExit("stdout_closed".to_owned()));
            };
            let line = line.map_err(|error| AgentError::Io(error.to_string()))?;
            let payload = match serde_json::from_str::<Value>(&line) {
                Ok(payload) => payload,
                Err(_) => {
                    on_update(self.make_update("malformed", None, None, Some(line)));
                    continue;
                }
            };

            let method = payload.get("method").and_then(Value::as_str);
            if let Some(method) = method {
                match method {
                    "turn/completed" => {
                        on_update(self.make_update(
                            "turn_completed",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line),
                        ));
                        return Ok(TurnResult {
                            session_id,
                            thread_id: self.thread_id.clone(),
                            turn_id,
                        });
                    }
                    "turn/failed" => {
                        on_update(self.make_update(
                            "turn_failed",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line.clone()),
                        ));
                        return Err(AgentError::TurnFailed(payload.to_string()));
                    }
                    "turn/cancelled" => {
                        on_update(self.make_update(
                            "turn_cancelled",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line.clone()),
                        ));
                        return Err(AgentError::TurnCancelled(payload.to_string()));
                    }
                    "item/tool/requestUserInput" => {
                        on_update(self.make_update(
                            "turn_input_required",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line.clone()),
                        ));
                        return Err(AgentError::TurnInputRequired);
                    }
                    "item/tool/call" => {
                        self.handle_tool_call(&payload, &line, &mut on_update)
                            .await?;
                    }
                    "item/commandExecution/requestApproval"
                    | "execCommandApproval"
                    | "applyPatchApproval"
                    | "item/fileChange/requestApproval" => {
                        self.handle_approval_request(method, &payload, &line, &mut on_update)
                            .await?;
                    }
                    _ if needs_input(method, &payload) => {
                        on_update(self.make_update(
                            "turn_input_required",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line.clone()),
                        ));
                        return Err(AgentError::TurnInputRequired);
                    }
                    _ => {
                        on_update(self.make_update(
                            "notification",
                            Some(payload.clone()),
                            extract_rate_limits(&payload),
                            Some(line),
                        ));
                    }
                }
            } else {
                on_update(self.make_update(
                    "other_message",
                    Some(payload.clone()),
                    extract_rate_limits(&payload),
                    Some(line),
                ));
            }
        }
    }

    pub async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stderr_task.abort();
    }

    async fn handle_approval_request<F>(
        &mut self,
        method: &str,
        payload: &Value,
        line: &str,
        on_update: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(AgentUpdate) + Send,
    {
        if !self.auto_approve_requests {
            on_update(self.make_update(
                "approval_required",
                Some(payload.clone()),
                extract_rate_limits(payload),
                Some(line.to_owned()),
            ));
            return Err(AgentError::ApprovalRequired);
        }

        let decision = match method {
            "execCommandApproval" | "applyPatchApproval" => "approved_for_session",
            _ => "acceptForSession",
        };

        let id = payload
            .get("id")
            .cloned()
            .ok_or_else(|| AgentError::ResponseError(payload.to_string()))?;
        self.send_message(json!({
            "id": id,
            "result": {
                "decision": decision
            }
        }))
        .await?;

        on_update(self.make_update(
            "approval_auto_approved",
            Some(payload.clone()),
            extract_rate_limits(payload),
            Some(line.to_owned()),
        ));
        Ok(())
    }

    async fn handle_tool_call<F>(
        &mut self,
        payload: &Value,
        line: &str,
        on_update: &mut F,
    ) -> Result<(), AgentError>
    where
        F: FnMut(AgentUpdate) + Send,
    {
        let tool_call_id = payload.get("id").cloned().unwrap_or(Value::Null);
        let params = payload.get("params");

        let tool_name = params
            .and_then(|p| {
                p.get("tool")
                    .or_else(|| p.get("name"))
                    .and_then(Value::as_str)
            })
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        match tool_name {
            Some("linear_graphql") => {
                let arguments = params
                    .and_then(|p| p.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));

                let result = execute_linear_graphql_tool(&self.config, arguments).await;

                let event = if result
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "tool_call_completed"
                } else {
                    "tool_call_failed"
                };

                self.send_message(json!({
                    "id": tool_call_id,
                    "result": result
                }))
                .await?;

                on_update(self.make_update(
                    event,
                    Some(payload.clone()),
                    extract_rate_limits(payload),
                    Some(line.to_owned()),
                ));
            }
            _ => {
                self.send_message(json!({
                    "id": tool_call_id,
                    "result": {
                        "success": false,
                        "output": "unsupported_tool_call",
                        "contentItems": [{
                            "type": "inputText",
                            "text": "unsupported_tool_call"
                        }]
                    }
                }))
                .await?;
                on_update(self.make_update(
                    "unsupported_tool_call",
                    Some(payload.clone()),
                    extract_rate_limits(payload),
                    Some(line.to_owned()),
                ));
            }
        }

        Ok(())
    }

    async fn send_message(&mut self, value: Value) -> Result<(), AgentError> {
        let encoded = serde_json::to_vec(&value)
            .map_err(|error| AgentError::ResponseError(error.to_string()))?;
        self.stdin
            .write_all(&encoded)
            .await
            .map_err(|error| AgentError::Io(error.to_string()))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| AgentError::Io(error.to_string()))
    }

    async fn await_response(
        &mut self,
        request_id: i64,
        cancellation: &CancellationToken,
    ) -> Result<Value, AgentError> {
        let deadline = Instant::now() + Duration::from_millis(self.config.codex.read_timeout_ms);

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if Instant::now() >= deadline {
                return Err(AgentError::ResponseTimeout);
            }

            let timeout_duration = deadline.saturating_duration_since(Instant::now());
            let next_line = time::timeout(timeout_duration, self.stdout.next())
                .await
                .map_err(|_| AgentError::ResponseTimeout)?;
            let Some(line) = next_line else {
                return Err(AgentError::PortExit("stdout_closed".to_owned()));
            };
            let line = line.map_err(|error| AgentError::Io(error.to_string()))?;

            let payload = match serde_json::from_str::<Value>(&line) {
                Ok(payload) => payload,
                Err(_) => {
                    debug!(line = %truncate_for_log(line.trim(), 1_000), "ignoring malformed startup line");
                    continue;
                }
            };

            match payload.get("id").and_then(Value::as_i64) {
                Some(id) if id == request_id => {
                    if let Some(error) = payload.get("error") {
                        return Err(AgentError::ResponseError(error.to_string()));
                    }

                    return payload
                        .get("result")
                        .cloned()
                        .ok_or_else(|| AgentError::ResponseError(payload.to_string()));
                }
                _ => {
                    debug!(payload = %truncate_for_log(&payload.to_string(), 1_000), "ignoring async message while waiting for response");
                }
            }
        }
    }

    fn make_update(
        &self,
        event: &str,
        raw_payload: Option<Value>,
        rate_limits: Option<Value>,
        raw_line: Option<String>,
    ) -> AgentUpdate {
        AgentUpdate {
            event: event.to_owned(),
            timestamp: Utc::now(),
            session_id: None,
            thread_id: if self.thread_id.is_empty() {
                None
            } else {
                Some(self.thread_id.clone())
            },
            turn_id: None,
            codex_app_server_pid: self.codex_app_server_pid.clone(),
            usage: raw_payload.as_ref().and_then(extract_usage),
            rate_limits,
            raw_payload,
            raw_line,
        }
    }
}

impl Drop for AppServerSession {
    fn drop(&mut self) {
        if let Some(id) = self.child.id() {
            info!(pid = id, "stopping codex app-server");
        }
    }
}

fn dynamic_tool_specs() -> Value {
    json!([
        {
            "name": "linear_graphql",
            "description": "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "GraphQL query or mutation document to execute against Linear."
                    },
                    "variables": {
                        "type": ["object", "null"],
                        "description": "Optional GraphQL variables object.",
                        "additionalProperties": true
                    }
                }
            }
        }
    ])
}

async fn execute_linear_graphql_tool(config: &EffectiveConfig, arguments: Value) -> Value {
    // Extract query from arguments (may be a JSON object or a plain string)
    let (query, variables) = match &arguments {
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return tool_error_response(
                    "`linear_graphql` requires a non-empty `query` string.",
                );
            }
            (trimmed.to_owned(), json!({}))
        }
        Value::Object(map) => {
            let query = match map.get("query").and_then(Value::as_str) {
                Some(q) if !q.trim().is_empty() => q.trim().to_owned(),
                _ => {
                    return tool_error_response(
                        "`linear_graphql` requires a non-empty `query` string.",
                    );
                }
            };

            let variables = match map.get("variables") {
                Some(Value::Object(_)) => map.get("variables").cloned().unwrap(),
                Some(Value::Null) | None => json!({}),
                Some(_) => {
                    return tool_error_response(
                        "`linear_graphql.variables` must be a JSON object when provided.",
                    );
                }
            };

            (query, variables)
        }
        _ => {
            return tool_error_response(
                "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`.",
            );
        }
    };

    let api_key = match config.tracker.api_key.as_deref() {
        Some(key) if !key.is_empty() => key,
        _ => {
            return tool_error_response(
                "Symphony is missing Linear auth. Set `linear.api_key` in `WORKFLOW.md` or export `LINEAR_API_KEY`.",
            );
        }
    };

    match execute_raw_graphql(&config.tracker.endpoint, api_key, &query, variables).await {
        Ok(body) => {
            let has_errors = body
                .get("errors")
                .and_then(Value::as_array)
                .is_some_and(|errors| !errors.is_empty());
            let output = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
            json!({
                "success": !has_errors,
                "output": output,
                "contentItems": [{
                    "type": "inputText",
                    "text": output
                }]
            })
        }
        Err(TrackerError::LinearApiStatus(status)) => tool_error_response(
            &format!("Linear GraphQL request failed with HTTP {status}."),
        ),
        Err(TrackerError::LinearApiRequest(reason)) => tool_error_response(&format!(
            "Linear GraphQL request failed before receiving a successful response: {reason}"
        )),
        Err(error) => {
            tool_error_response(&format!("Linear GraphQL tool execution failed: {error}"))
        }
    }
}

fn tool_error_response(message: &str) -> Value {
    let payload = json!({
        "error": {
            "message": message
        }
    });
    let output = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string());
    json!({
        "success": false,
        "output": output,
        "contentItems": [{
            "type": "inputText",
            "text": output
        }]
    })
}

fn needs_input(method: &str, payload: &Value) -> bool {
    matches!(
        method,
        "turn/input_required"
            | "turn/needs_input"
            | "turn/need_input"
            | "turn/request_input"
            | "turn/request_response"
            | "turn/provide_input"
            | "turn/approval_required"
    ) || payload_contains_input_flags(payload)
}

fn payload_contains_input_flags(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "requiresInput" | "needsInput" | "input_required" | "inputRequired"
            ) && value.as_bool() == Some(true)
                || payload_contains_input_flags(value)
        }),
        Value::Array(items) => items.iter().any(payload_contains_input_flags),
        _ => false,
    }
}

fn extract_usage(payload: &Value) -> Option<Value> {
    payload
        .get("usage")
        .cloned()
        .or_else(|| recursive_find(payload, &["usage"]))
}

fn extract_rate_limits(payload: &Value) -> Option<Value> {
    recursive_find(payload, &["rate_limits", "rateLimits"])
}

fn recursive_find(payload: &Value, keys: &[&str]) -> Option<Value> {
    match payload {
        Value::Object(map) => {
            for (key, value) in map {
                if keys.iter().any(|candidate| candidate == key) {
                    return Some(value.clone());
                }
                if let Some(found) = recursive_find(value, keys) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(|value| recursive_find(value, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_detection_handles_nested_flags() {
        let payload = json!({
            "params": {
                "needsInput": true
            }
        });
        assert!(needs_input("turn/updated", &payload));
    }

    #[test]
    fn dynamic_tool_specs_contains_linear_graphql() {
        let specs = dynamic_tool_specs();
        let tools = specs.as_array().expect("should be an array");
        assert_eq!(tools.len(), 1);
        let tool = &tools[0];
        assert_eq!(tool.get("name").and_then(Value::as_str), Some("linear_graphql"));
        let schema = tool.get("inputSchema").expect("should have inputSchema");
        let required = schema.get("required").and_then(Value::as_array).expect("should have required");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("query"));
        let props = schema.get("properties").and_then(Value::as_object).expect("should have properties");
        assert!(props.contains_key("query"));
        assert!(props.contains_key("variables"));
    }

    #[test]
    fn tool_error_response_returns_structured_json() {
        let result = tool_error_response("test error");
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).expect("should have output");
        assert!(output.contains("test error"));
        let items = result.get("contentItems").and_then(Value::as_array).expect("should have contentItems");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].get("type").and_then(Value::as_str), Some("inputText"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_missing_query_in_object() {
        let config = test_config();
        let result = execute_linear_graphql_tool(&config, json!({"variables": {}})).await;
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("requires a non-empty `query` string"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_empty_string_query() {
        let config = test_config();
        let result = execute_linear_graphql_tool(&config, json!("   ")).await;
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("requires a non-empty `query` string"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_invalid_variables() {
        let config = test_config();
        let result = execute_linear_graphql_tool(
            &config,
            json!({"query": "{ viewer { id } }", "variables": "not_an_object"}),
        )
        .await;
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("must be a JSON object"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_invalid_argument_type() {
        let config = test_config();
        let result = execute_linear_graphql_tool(&config, json!(42)).await;
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("expects either a GraphQL query string"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_missing_api_key() {
        let mut config = test_config();
        config.tracker.api_key = None;
        let result = execute_linear_graphql_tool(
            &config,
            json!({"query": "{ viewer { id } }"}),
        )
        .await;
        assert_eq!(result.get("success").and_then(Value::as_bool), Some(false));
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("missing Linear auth"));
    }

    #[tokio::test]
    async fn execute_linear_graphql_tool_null_variables_treated_as_empty() {
        // This test validates that null variables don't cause an error.
        // The actual API call will fail (no real endpoint), but it should
        // get past argument validation.
        let mut config = test_config();
        config.tracker.api_key = None; // intentionally missing to fail early
        let result = execute_linear_graphql_tool(
            &config,
            json!({"query": "{ viewer { id } }", "variables": null}),
        )
        .await;
        // Should fail on missing API key, not on variable validation
        let output = result.get("output").and_then(Value::as_str).unwrap();
        assert!(output.contains("missing Linear auth"));
    }

    fn test_config() -> EffectiveConfig {
        EffectiveConfig {
            tracker: crate::config::TrackerConfig {
                kind: Some("linear".to_owned()),
                endpoint: "https://api.linear.app/graphql".to_owned(),
                api_key: Some("test-token".to_owned()),
                project_slug: Some("demo".to_owned()),
                assignee: None,
                active_states: vec!["Todo".to_owned()],
                terminal_states: vec!["Done".to_owned()],
            },
            polling: crate::config::PollingConfig {
                interval_ms: 30_000,
            },
            workspace: crate::config::WorkspaceConfig {
                root: std::path::PathBuf::from("/tmp/test"),
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
                command: "echo test".to_owned(),
                approval_policy: json!("never"),
                thread_sandbox: "workspace-write".to_owned(),
                turn_sandbox_policy: None,
                turn_timeout_ms: 60_000,
                read_timeout_ms: 5_000,
                stall_timeout_ms: 300_000,
            },
            server: crate::config::ServerConfig { port: None },
        }
    }
}
