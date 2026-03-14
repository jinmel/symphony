pub mod linear;
pub mod memory;

use crate::config::EffectiveConfig;
use crate::issue::Issue;
use thiserror::Error;

pub use linear::{LinearTrackerClient, execute_raw_graphql};
pub use memory::MemoryTracker;

#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("unsupported_tracker_kind")]
    UnsupportedTrackerKind,
    #[error("missing_tracker_api_key")]
    MissingTrackerApiKey,
    #[error("missing_tracker_project_slug")]
    MissingTrackerProjectSlug,
    #[error("linear_api_request: {0}")]
    LinearApiRequest(String),
    #[error("linear_api_status: {0}")]
    LinearApiStatus(u16),
    #[error("linear_graphql_errors: {0}")]
    LinearGraphqlErrors(String),
    #[error("linear_unknown_payload")]
    LinearUnknownPayload,
    #[error("linear_missing_end_cursor")]
    LinearMissingEndCursor,
    #[error("comment_create_failed")]
    CommentCreateFailed,
    #[error("issue_update_failed")]
    IssueUpdateFailed,
    #[error("state_not_found: {0}")]
    StateNotFound(String),
}

/// Pluggable tracker abstraction.
///
/// Implementations include the [`LinearTrackerClient`] for real Linear API
/// access and [`MemoryTracker`] for tests and local development.
pub trait Tracker: Send + Sync {
    fn fetch_candidate_issues(
        &self,
        config: &EffectiveConfig,
    ) -> impl std::future::Future<Output = Result<Vec<Issue>, TrackerError>> + Send;

    fn fetch_issues_by_states(
        &self,
        config: &EffectiveConfig,
        states: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Issue>, TrackerError>> + Send;

    fn fetch_issue_states_by_ids(
        &self,
        config: &EffectiveConfig,
        ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Issue>, TrackerError>> + Send;

    fn create_comment(
        &self,
        config: &EffectiveConfig,
        issue_id: &str,
        body: &str,
    ) -> impl std::future::Future<Output = Result<(), TrackerError>> + Send;

    fn update_issue_state(
        &self,
        config: &EffectiveConfig,
        issue_id: &str,
        state_name: &str,
    ) -> impl std::future::Future<Output = Result<(), TrackerError>> + Send;
}

/// Concrete tracker dispatcher that selects the right backend based on
/// `config.tracker.kind`.
///
/// This preserves backward compatibility with code that uses `TrackerClient`
/// directly (e.g. the orchestrator).
#[derive(Debug, Clone)]
pub struct TrackerClient {
    linear: LinearTrackerClient,
}

impl Default for TrackerClient {
    fn default() -> Self {
        Self {
            linear: LinearTrackerClient::new(),
        }
    }
}

impl TrackerClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn fetch_candidate_issues(
        &self,
        config: &EffectiveConfig,
    ) -> Result<Vec<Issue>, TrackerError> {
        match config.tracker.kind.as_deref() {
            Some("linear") => self.linear.fetch_candidate_issues(config).await,
            _ => Err(TrackerError::UnsupportedTrackerKind),
        }
    }

    pub async fn fetch_issues_by_states(
        &self,
        config: &EffectiveConfig,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        match config.tracker.kind.as_deref() {
            Some("linear") => self.linear.fetch_issues_by_states(config, states).await,
            _ => Err(TrackerError::UnsupportedTrackerKind),
        }
    }

    pub async fn fetch_issue_states_by_ids(
        &self,
        config: &EffectiveConfig,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        match config.tracker.kind.as_deref() {
            Some("linear") => self.linear.fetch_issue_states_by_ids(config, ids).await,
            _ => Err(TrackerError::UnsupportedTrackerKind),
        }
    }

    pub async fn create_comment(
        &self,
        config: &EffectiveConfig,
        issue_id: &str,
        body: &str,
    ) -> Result<(), TrackerError> {
        match config.tracker.kind.as_deref() {
            Some("linear") => self.linear.create_comment(config, issue_id, body).await,
            _ => Err(TrackerError::UnsupportedTrackerKind),
        }
    }

    pub async fn update_issue_state(
        &self,
        config: &EffectiveConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        match config.tracker.kind.as_deref() {
            Some("linear") => {
                self.linear
                    .update_issue_state(config, issue_id, state_name)
                    .await
            }
            _ => Err(TrackerError::UnsupportedTrackerKind),
        }
    }
}
