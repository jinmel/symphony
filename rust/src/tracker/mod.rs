mod linear;

use thiserror::Error;

use crate::config::EffectiveConfig;
use crate::issue::Issue;

pub use linear::{LinearTrackerClient, execute_raw_graphql};

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
}

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
}
