//! In-memory tracker adapter for tests and local development.
//!
//! Mirrors the Elixir `SymphonyElixir.Tracker.Memory` module. Issues are
//! stored in a `Vec<Issue>` behind a lock. The `create_comment` and
//! `update_issue_state` methods record events into an internal log that tests
//! can inspect.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::config::EffectiveConfig;
use crate::issue::Issue;

use super::{Tracker, TrackerError};

/// A recorded event from a memory tracker mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryTrackerEvent {
    CommentCreated {
        issue_id: String,
        body: String,
    },
    StateUpdated {
        issue_id: String,
        state_name: String,
    },
}

/// Shared interior state for [`MemoryTracker`].
#[derive(Debug, Default)]
struct Inner {
    issues: Vec<Issue>,
    events: Vec<MemoryTrackerEvent>,
}

/// In-memory [`Tracker`] implementation.
///
/// All data lives in-process -- no network calls are made. Useful for unit
/// tests and local development loops.
#[derive(Debug, Clone, Default)]
pub struct MemoryTracker {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryTracker {
    /// Create an empty memory tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a memory tracker pre-populated with issues.
    pub fn with_issues(issues: Vec<Issue>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                issues,
                events: Vec::new(),
            })),
        }
    }

    /// Replace the full issue set.
    pub fn set_issues(&self, issues: Vec<Issue>) {
        let mut inner = self.inner.lock().expect("memory tracker lock poisoned");
        inner.issues = issues;
    }

    /// Return a snapshot of all recorded events.
    pub fn events(&self) -> Vec<MemoryTrackerEvent> {
        let inner = self.inner.lock().expect("memory tracker lock poisoned");
        inner.events.clone()
    }

    /// Clear recorded events.
    pub fn clear_events(&self) {
        let mut inner = self.inner.lock().expect("memory tracker lock poisoned");
        inner.events.clear();
    }
}

impl Tracker for MemoryTracker {
    async fn fetch_candidate_issues(
        &self,
        _config: &EffectiveConfig,
    ) -> Result<Vec<Issue>, TrackerError> {
        let inner = self.inner.lock().expect("memory tracker lock poisoned");
        Ok(inner.issues.clone())
    }

    async fn fetch_issues_by_states(
        &self,
        _config: &EffectiveConfig,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let normalized: HashSet<String> = states
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .collect();

        let inner = self.inner.lock().expect("memory tracker lock poisoned");
        Ok(inner
            .issues
            .iter()
            .filter(|issue| normalized.contains(&issue.state.trim().to_ascii_lowercase()))
            .cloned()
            .collect())
    }

    async fn fetch_issue_states_by_ids(
        &self,
        _config: &EffectiveConfig,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();

        let inner = self.inner.lock().expect("memory tracker lock poisoned");
        Ok(inner
            .issues
            .iter()
            .filter(|issue| wanted.contains(issue.id.as_str()))
            .cloned()
            .collect())
    }

    async fn create_comment(
        &self,
        _config: &EffectiveConfig,
        issue_id: &str,
        body: &str,
    ) -> Result<(), TrackerError> {
        let mut inner = self.inner.lock().expect("memory tracker lock poisoned");
        inner.events.push(MemoryTrackerEvent::CommentCreated {
            issue_id: issue_id.to_owned(),
            body: body.to_owned(),
        });
        Ok(())
    }

    async fn update_issue_state(
        &self,
        _config: &EffectiveConfig,
        issue_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        let mut inner = self.inner.lock().expect("memory tracker lock poisoned");
        inner.events.push(MemoryTrackerEvent::StateUpdated {
            issue_id: issue_id.to_owned(),
            state_name: state_name.to_owned(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> EffectiveConfig {
        let yaml = serde_yaml::from_str::<serde_yaml::Mapping>(
            r#"
tracker:
  kind: memory
  project_slug: test
"#,
        )
        .unwrap();
        EffectiveConfig::from_workflow_config(&yaml).unwrap()
    }

    fn sample_issues() -> Vec<Issue> {
        vec![
            Issue {
                id: "issue-1".into(),
                identifier: "TEST-1".into(),
                title: "First issue".into(),
                description: None,
                priority: Some(1),
                state: "Todo".into(),
                branch_name: None,
                url: None,
                assignee_id: None,
                labels: vec![],
                blocked_by: vec![],
                assigned_to_worker: true,
                created_at: None,
                updated_at: None,
            },
            Issue {
                id: "issue-2".into(),
                identifier: "TEST-2".into(),
                title: "Second issue".into(),
                description: Some("desc".into()),
                priority: Some(2),
                state: "In Progress".into(),
                branch_name: None,
                url: None,
                assignee_id: None,
                labels: vec!["bug".into()],
                blocked_by: vec![],
                assigned_to_worker: true,
                created_at: None,
                updated_at: None,
            },
        ]
    }

    #[tokio::test]
    async fn fetch_candidate_issues_returns_all() {
        let tracker = MemoryTracker::with_issues(sample_issues());
        let config = test_config();

        let issues = tracker.fetch_candidate_issues(&config).await.unwrap();
        assert_eq!(issues.len(), 2);
    }

    #[tokio::test]
    async fn fetch_issues_by_states_filters_correctly() {
        let tracker = MemoryTracker::with_issues(sample_issues());
        let config = test_config();

        let states = vec!["todo".to_owned()];
        let issues = tracker
            .fetch_issues_by_states(&config, &states)
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "issue-1");

        let states = vec!["In Progress".to_owned()];
        let issues = tracker
            .fetch_issues_by_states(&config, &states)
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "issue-2");

        let states = vec!["Todo".to_owned(), "In Progress".to_owned()];
        let issues = tracker
            .fetch_issues_by_states(&config, &states)
            .await
            .unwrap();
        assert_eq!(issues.len(), 2);
    }

    #[tokio::test]
    async fn fetch_issue_states_by_ids_filters_correctly() {
        let tracker = MemoryTracker::with_issues(sample_issues());
        let config = test_config();

        let ids = vec!["issue-2".to_owned()];
        let issues = tracker
            .fetch_issue_states_by_ids(&config, &ids)
            .await
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].identifier, "TEST-2");
    }

    #[tokio::test]
    async fn create_comment_records_event() {
        let tracker = MemoryTracker::new();
        let config = test_config();

        tracker
            .create_comment(&config, "issue-1", "Hello world")
            .await
            .unwrap();

        let events = tracker.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            MemoryTrackerEvent::CommentCreated {
                issue_id: "issue-1".into(),
                body: "Hello world".into(),
            }
        );
    }

    #[tokio::test]
    async fn update_issue_state_records_event() {
        let tracker = MemoryTracker::new();
        let config = test_config();

        tracker
            .update_issue_state(&config, "issue-1", "Done")
            .await
            .unwrap();

        let events = tracker.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            MemoryTrackerEvent::StateUpdated {
                issue_id: "issue-1".into(),
                state_name: "Done".into(),
            }
        );
    }

    #[tokio::test]
    async fn clear_events_works() {
        let tracker = MemoryTracker::new();
        let config = test_config();

        tracker
            .create_comment(&config, "issue-1", "test")
            .await
            .unwrap();
        assert_eq!(tracker.events().len(), 1);

        tracker.clear_events();
        assert_eq!(tracker.events().len(), 0);
    }

    #[tokio::test]
    async fn set_issues_replaces_all() {
        let tracker = MemoryTracker::new();
        let config = test_config();

        let issues = tracker.fetch_candidate_issues(&config).await.unwrap();
        assert_eq!(issues.len(), 0);

        tracker.set_issues(sample_issues());
        let issues = tracker.fetch_candidate_issues(&config).await.unwrap();
        assert_eq!(issues.len(), 2);
    }
}
