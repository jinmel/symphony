//! HTTP server providing observability API endpoints.
//!
//! The server exposes a JSON API matching the Elixir implementation:
//!
//! - `GET  /api/v1/state`              -- full orchestrator snapshot
//! - `POST /api/v1/refresh`            -- trigger a manual poll refresh
//! - `GET  /api/v1/:issue_identifier`  -- per-issue status
//!
//! # Integration
//!
//! ```ignore
//! use std::sync::Arc;
//! use tokio_util::sync::CancellationToken;
//! use symphony::server::{ObservabilityServer, SharedSnapshot};
//!
//! let snapshot = SharedSnapshot::default();
//! let shutdown = CancellationToken::new();
//! let server = ObservabilityServer::new("127.0.0.1", 4000, snapshot.clone(), shutdown.clone());
//! tokio::spawn(server.run());
//!
//! // From the orchestrator loop, update the snapshot periodically:
//! // snapshot.update(|s| { s.running = ...; }).await;
//! ```

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{DateTime, Utc};

use crate::web_dashboard;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::info;

// ---------------------------------------------------------------------------
// Snapshot data structures
// ---------------------------------------------------------------------------

/// Thread-safe handle to the orchestrator snapshot.
///
/// The orchestrator updates this periodically; the HTTP handlers read it.
#[derive(Debug, Clone)]
pub struct SharedSnapshot {
    inner: Arc<RwLock<OrchestratorSnapshot>>,
    refresh_tx: Arc<tokio::sync::Notify>,
}

impl Default for SharedSnapshot {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(OrchestratorSnapshot::default())),
            refresh_tx: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

impl SharedSnapshot {
    /// Replace the current snapshot atomically.
    pub async fn set(&self, snapshot: OrchestratorSnapshot) {
        *self.inner.write().await = snapshot;
    }

    /// Apply a mutation to the current snapshot.
    pub async fn update<F: FnOnce(&mut OrchestratorSnapshot)>(&self, f: F) {
        let mut guard = self.inner.write().await;
        f(&mut guard);
    }

    /// Read the current snapshot.
    pub async fn get(&self) -> OrchestratorSnapshot {
        self.inner.read().await.clone()
    }

    /// Signal that a manual refresh has been requested.
    pub fn notify_refresh(&self) {
        self.refresh_tx.notify_one();
    }

    /// Returns a clone of the internal [`Notify`] so the orchestrator can
    /// await refresh requests.
    pub fn refresh_notifier(&self) -> Arc<tokio::sync::Notify> {
        self.refresh_tx.clone()
    }
}

/// Point-in-time view of orchestrator state, suitable for JSON serialization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrchestratorSnapshot {
    pub running: Vec<RunningIssueSnapshot>,
    pub retrying: Vec<RetryingIssueSnapshot>,
    pub codex_totals: TokenTotals,
    pub rate_limits: Option<Value>,
}

/// Snapshot of a currently-running issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningIssueSnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub state: String,
    pub worker_host: Option<String>,
    pub workspace_path: Option<PathBuf>,
    pub session_id: Option<String>,
    pub turn_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
    pub tokens: TokenTotals,
}

/// Snapshot of an issue in retry/backoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryingIssueSnapshot {
    pub issue_id: String,
    pub issue_identifier: String,
    pub attempt: u32,
    pub due_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<PathBuf>,
}

/// Aggregated token counts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// HTTP server for the observability API.
pub struct ObservabilityServer {
    host: IpAddr,
    port: u16,
    snapshot: SharedSnapshot,
    shutdown: CancellationToken,
}

impl ObservabilityServer {
    pub fn new(
        host: &str,
        port: u16,
        snapshot: SharedSnapshot,
        shutdown: CancellationToken,
    ) -> Self {
        let parsed_host: IpAddr = host.parse().unwrap_or_else(|_| {
            tracing::warn!(
                host,
                "invalid server host, falling back to 127.0.0.1"
            );
            IpAddr::from([127, 0, 0, 1])
        });
        Self {
            host: parsed_host,
            port,
            snapshot,
            shutdown,
        }
    }

    /// Run the server until the shutdown token is cancelled.
    pub async fn run(self) -> std::io::Result<()> {
        let app = build_router(self.snapshot);
        let addr = SocketAddr::from((self.host, self.port));

        let listener = TcpListener::bind(addr).await?;
        info!(port = self.port, "observability server listening");

        axum::serve(listener, app)
            .with_graceful_shutdown(self.shutdown.cancelled_owned())
            .await?;

        info!("observability server shut down");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

fn build_router(snapshot: SharedSnapshot) -> Router {
    Router::new()
        .route("/", get(web_dashboard::handle_dashboard))
        .route("/dashboard.css", get(web_dashboard::handle_dashboard_css))
        .route("/api/v1/state", get(handle_state))
        .route("/api/v1/refresh", post(handle_refresh))
        .route("/api/v1/{issue_identifier}", get(handle_issue))
        .fallback(handle_not_found)
        .with_state(snapshot)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_state(State(snapshot): State<SharedSnapshot>) -> impl IntoResponse {
    let snap = snapshot.get().await;
    let now = Utc::now();

    let running: Vec<Value> = snap.running.iter().map(running_entry_json).collect();
    let retrying: Vec<Value> = snap.retrying.iter().map(retry_entry_json).collect();

    Json(json!({
        "generated_at": format_datetime(&now),
        "counts": {
            "running": snap.running.len(),
            "retrying": snap.retrying.len()
        },
        "running": running,
        "retrying": retrying,
        "codex_totals": {
            "input_tokens": snap.codex_totals.input_tokens,
            "output_tokens": snap.codex_totals.output_tokens,
            "total_tokens": snap.codex_totals.total_tokens
        },
        "rate_limits": snap.rate_limits
    }))
}

async fn handle_refresh(State(snapshot): State<SharedSnapshot>) -> impl IntoResponse {
    let now = Utc::now();
    snapshot.notify_refresh();
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "requested_at": format_datetime(&now),
            "status": "refresh_requested"
        })),
    )
}

async fn handle_issue(
    State(snapshot): State<SharedSnapshot>,
    Path(issue_identifier): Path<String>,
) -> Response {
    let snap = snapshot.get().await;

    let running = snap
        .running
        .iter()
        .find(|entry| entry.issue_identifier == issue_identifier);
    let retrying = snap
        .retrying
        .iter()
        .find(|entry| entry.issue_identifier == issue_identifier);

    if running.is_none() && retrying.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "issue_not_found",
                    "message": "Issue not found"
                }
            })),
        )
            .into_response();
    }

    let issue_id = running
        .map(|r| r.issue_id.as_str())
        .or_else(|| retrying.map(|r| r.issue_id.as_str()));

    let status = match (running, retrying) {
        (Some(_), _) => "running",
        (None, Some(_)) => "retrying",
        _ => "unknown",
    };

    let workspace_path = running
        .and_then(|r| r.workspace_path.as_ref())
        .or_else(|| retrying.and_then(|r| r.workspace_path.as_ref()))
        .map(|p| p.display().to_string());

    let worker_host = running
        .and_then(|r| r.worker_host.as_ref())
        .or_else(|| retrying.and_then(|r| r.worker_host.as_ref()));

    let restart_count = retrying
        .map(|r| r.attempt.saturating_sub(1))
        .unwrap_or(0);
    let current_retry_attempt = retrying.map(|r| r.attempt).unwrap_or(0);

    let running_detail = running.map(|r| {
        json!({
            "worker_host": r.worker_host,
            "workspace_path": r.workspace_path,
            "session_id": r.session_id,
            "turn_count": r.turn_count,
            "state": r.state,
            "started_at": r.started_at.as_ref().map(format_datetime),
            "last_event": r.last_event,
            "last_message": r.last_message,
            "last_event_at": r.last_event_at.as_ref().map(format_datetime),
            "tokens": {
                "input_tokens": r.tokens.input_tokens,
                "output_tokens": r.tokens.output_tokens,
                "total_tokens": r.tokens.total_tokens
            }
        })
    });

    let retry_detail = retrying.map(|r| {
        json!({
            "attempt": r.attempt,
            "due_at": r.due_at.as_ref().map(format_datetime),
            "error": r.error,
            "worker_host": r.worker_host,
            "workspace_path": r.workspace_path
        })
    });

    let recent_events: Vec<Value> = running
        .and_then(|r| {
            r.last_event_at.as_ref().map(|at| {
                vec![json!({
                    "at": format_datetime(at),
                    "event": r.last_event,
                    "message": r.last_message
                })]
            })
        })
        .unwrap_or_default();

    let last_error = retrying.and_then(|r| r.error.as_ref());

    Json(json!({
        "issue_identifier": issue_identifier,
        "issue_id": issue_id,
        "status": status,
        "workspace": {
            "path": workspace_path,
            "host": worker_host
        },
        "attempts": {
            "restart_count": restart_count,
            "current_retry_attempt": current_retry_attempt
        },
        "running": running_detail,
        "retry": retry_detail,
        "logs": {
            "codex_session_logs": []
        },
        "recent_events": recent_events,
        "last_error": last_error,
        "tracked": {}
    }))
    .into_response()
}

async fn handle_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "code": "not_found",
                "message": "Route not found"
            }
        })),
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_datetime(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn running_entry_json(entry: &RunningIssueSnapshot) -> Value {
    json!({
        "issue_id": entry.issue_id,
        "issue_identifier": entry.issue_identifier,
        "state": entry.state,
        "worker_host": entry.worker_host,
        "workspace_path": entry.workspace_path,
        "session_id": entry.session_id,
        "turn_count": entry.turn_count,
        "last_event": entry.last_event,
        "last_message": entry.last_message,
        "started_at": entry.started_at.as_ref().map(format_datetime),
        "last_event_at": entry.last_event_at.as_ref().map(format_datetime),
        "tokens": {
            "input_tokens": entry.tokens.input_tokens,
            "output_tokens": entry.tokens.output_tokens,
            "total_tokens": entry.tokens.total_tokens
        }
    })
}

fn retry_entry_json(entry: &RetryingIssueSnapshot) -> Value {
    json!({
        "issue_id": entry.issue_id,
        "issue_identifier": entry.issue_identifier,
        "attempt": entry.attempt,
        "due_at": entry.due_at.as_ref().map(format_datetime),
        "error": entry.error,
        "worker_host": entry.worker_host,
        "workspace_path": entry.workspace_path
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_snapshot() -> SharedSnapshot {
        SharedSnapshot::default()
    }

    #[tokio::test]
    async fn state_returns_empty_snapshot() {
        let snapshot = test_snapshot();
        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["counts"]["running"], 0);
        assert_eq!(json["counts"]["retrying"], 0);
        assert!(json["generated_at"].is_string());
    }

    #[tokio::test]
    async fn state_returns_running_issues() {
        let snapshot = test_snapshot();
        snapshot
            .update(|s| {
                s.running.push(RunningIssueSnapshot {
                    issue_id: "id-1".to_owned(),
                    issue_identifier: "PROJ-1".to_owned(),
                    state: "In Progress".to_owned(),
                    worker_host: None,
                    workspace_path: Some(PathBuf::from("/tmp/ws/PROJ-1")),
                    session_id: Some("sess-abc".to_owned()),
                    turn_count: 3,
                    last_event: Some("message".to_owned()),
                    last_message: Some("Working on it".to_owned()),
                    started_at: Some(Utc::now()),
                    last_event_at: Some(Utc::now()),
                    tokens: TokenTotals {
                        input_tokens: 100,
                        output_tokens: 50,
                        total_tokens: 150,
                    },
                });
            })
            .await;

        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["counts"]["running"], 1);
        assert_eq!(json["running"][0]["issue_identifier"], "PROJ-1");
        assert_eq!(json["running"][0]["tokens"]["total_tokens"], 150);
    }

    #[tokio::test]
    async fn refresh_returns_202() {
        let snapshot = test_snapshot();
        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/refresh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert!(json["requested_at"].is_string());
        assert_eq!(json["status"], "refresh_requested");
    }

    #[tokio::test]
    async fn issue_not_found_returns_404() {
        let snapshot = test_snapshot();
        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/PROJ-999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["error"]["code"], "issue_not_found");
    }

    #[tokio::test]
    async fn issue_found_returns_detail() {
        let snapshot = test_snapshot();
        snapshot
            .update(|s| {
                s.running.push(RunningIssueSnapshot {
                    issue_id: "id-2".to_owned(),
                    issue_identifier: "PROJ-42".to_owned(),
                    state: "In Progress".to_owned(),
                    worker_host: Some("host-1".to_owned()),
                    workspace_path: Some(PathBuf::from("/tmp/ws/PROJ-42")),
                    session_id: Some("sess-xyz".to_owned()),
                    turn_count: 5,
                    last_event: Some("tool_use".to_owned()),
                    last_message: Some("Editing file".to_owned()),
                    started_at: Some(Utc::now()),
                    last_event_at: Some(Utc::now()),
                    tokens: TokenTotals {
                        input_tokens: 200,
                        output_tokens: 100,
                        total_tokens: 300,
                    },
                });
            })
            .await;

        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/PROJ-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["issue_identifier"], "PROJ-42");
        assert_eq!(json["status"], "running");
        assert_eq!(json["running"]["session_id"], "sess-xyz");
        assert_eq!(json["workspace"]["host"], "host-1");
    }

    #[tokio::test]
    async fn retrying_issue_returns_detail() {
        let snapshot = test_snapshot();
        snapshot
            .update(|s| {
                s.retrying.push(RetryingIssueSnapshot {
                    issue_id: "id-3".to_owned(),
                    issue_identifier: "PROJ-7".to_owned(),
                    attempt: 3,
                    due_at: Some(Utc::now() + chrono::Duration::seconds(30)),
                    error: Some("agent crashed".to_owned()),
                    worker_host: None,
                    workspace_path: Some(PathBuf::from("/tmp/ws/PROJ-7")),
                });
            })
            .await;

        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/PROJ-7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["issue_identifier"], "PROJ-7");
        assert_eq!(json["status"], "retrying");
        assert_eq!(json["attempts"]["restart_count"], 2);
        assert_eq!(json["attempts"]["current_retry_attempt"], 3);
        assert_eq!(json["last_error"], "agent crashed");
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let snapshot = test_snapshot();
        let app = build_router(snapshot);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/unknown/path")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["error"]["code"], "not_found");
    }
}
