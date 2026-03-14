use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// Default buffer capacity for the observability broadcast channel.
const DEFAULT_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Events published on the observability bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum ObservabilityEvent {
    /// A full snapshot of the orchestrator state.
    StateUpdated(OrchestratorSnapshot),

    /// A manual refresh was requested (e.g. by a dashboard user).
    RefreshRequested,
}

// ---------------------------------------------------------------------------
// Snapshot types
// ---------------------------------------------------------------------------

/// Complete point-in-time view of the orchestrator suitable for dashboards
/// and the HTTP API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorSnapshot {
    /// Issues currently being worked on by agents.
    pub running: Vec<RunningIssueInfo>,

    /// Issues waiting for a retry attempt.
    pub retrying: Vec<RetryingIssueInfo>,

    /// Aggregate token usage across all sessions.
    pub token_totals: TokenTotals,

    /// Most recently observed rate-limit information, if any.
    pub rate_limits: Option<RateLimitInfo>,

    /// When this snapshot was captured.
    pub timestamp: DateTime<Utc>,
}

/// Information about a single issue that is actively running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunningIssueInfo {
    /// Tracker-side unique identifier (e.g. Linear issue UUID).
    pub issue_id: String,

    /// Human-readable issue identifier (e.g. "ENG-123").
    pub identifier: String,

    /// Issue title / summary.
    pub title: String,

    /// Current tracker state label (e.g. "In Progress").
    pub state: String,

    /// Path to the on-disk workspace for this issue.
    pub workspace_path: PathBuf,

    /// Codex session id, if one has been established.
    pub session_id: Option<String>,

    /// Cumulative tokens consumed for this issue.
    pub tokens_used: u64,

    /// Number of agent turns completed.
    pub turn_count: u32,

    /// When the current attempt started.
    pub started_at: DateTime<Utc>,

    /// Wall-clock seconds since `started_at`.
    pub elapsed_seconds: u64,
}

/// Information about an issue waiting to be retried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryingIssueInfo {
    /// Tracker-side unique identifier.
    pub issue_id: String,

    /// Human-readable issue identifier.
    pub identifier: String,

    /// How many retry attempts have been made so far.
    pub attempt_count: u32,

    /// When the next retry attempt is scheduled (approximate wall-clock time).
    pub next_retry_at: DateTime<Utc>,

    /// Error message from the most recent failed attempt, if available.
    pub last_error: Option<String>,
}

/// Aggregate token usage counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Rate-limit information forwarded from the agent / model provider.
///
/// Stored as an opaque JSON value because different providers surface
/// different shapes.  Consumers can inspect the value if they need
/// provider-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub raw: Value,
}

// ---------------------------------------------------------------------------
// ObservabilityBus
// ---------------------------------------------------------------------------

/// Broadcast bus for observability events.
///
/// Backed by a `tokio::sync::broadcast` channel.  Publishers call
/// [`publish`](Self::publish) (or the convenience
/// [`publish_snapshot`](Self::publish_snapshot)) and any number of
/// subscribers can independently receive events via
/// [`subscribe`](Self::subscribe).
///
/// Lagging receivers (those that fall behind the buffer) will skip missed
/// events -- this is acceptable for observability where the latest state
/// matters more than historical completeness.
#[derive(Debug, Clone)]
pub struct ObservabilityBus {
    tx: broadcast::Sender<ObservabilityEvent>,
}

impl ObservabilityBus {
    /// Create a new bus with the given buffer capacity.
    ///
    /// `capacity` determines how many events can be buffered before the
    /// oldest events are dropped for slow receivers.  The default is 64.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to the event stream.
    ///
    /// Each subscriber gets its own independent receiver.  If the receiver
    /// falls behind, it will receive a `RecvError::Lagged` and can skip
    /// ahead.
    pub fn subscribe(&self) -> broadcast::Receiver<ObservabilityEvent> {
        self.tx.subscribe()
    }

    /// Publish an arbitrary observability event to all subscribers.
    ///
    /// Returns the number of receivers that received the event.  If there
    /// are no active subscribers the event is silently dropped (mirroring
    /// the Elixir PubSub behaviour where broadcasts succeed even when no
    /// process is listening).
    pub fn publish(&self, event: ObservabilityEvent) -> usize {
        // `broadcast::Sender::send` returns Err when there are zero
        // receivers.  That is fine -- we just treat it as 0 delivered.
        self.tx.send(event).unwrap_or(0)
    }

    /// Convenience wrapper: wrap a snapshot in `StateUpdated` and publish.
    pub fn publish_snapshot(&self, snapshot: OrchestratorSnapshot) -> usize {
        self.publish(ObservabilityEvent::StateUpdated(snapshot))
    }

    /// Returns the current number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for ObservabilityBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_receive_snapshot() {
        let bus = ObservabilityBus::default();
        let mut rx = bus.subscribe();

        let snapshot = OrchestratorSnapshot {
            running: vec![],
            retrying: vec![],
            token_totals: TokenTotals::default(),
            rate_limits: None,
            timestamp: Utc::now(),
        };

        let delivered = bus.publish_snapshot(snapshot.clone());
        assert_eq!(delivered, 1);

        let event = rx.recv().await.unwrap();
        match event {
            ObservabilityEvent::StateUpdated(received) => {
                assert_eq!(received.timestamp, snapshot.timestamp);
            }
            _ => panic!("expected StateUpdated"),
        }
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let bus = ObservabilityBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        let mut rx3 = bus.subscribe();

        let delivered = bus.publish(ObservabilityEvent::RefreshRequested);
        assert_eq!(delivered, 3);

        for rx in [&mut rx1, &mut rx2, &mut rx3] {
            let event = rx.recv().await.unwrap();
            assert!(matches!(event, ObservabilityEvent::RefreshRequested));
        }
    }

    #[test]
    fn publish_with_no_subscribers_does_not_panic() {
        let bus = ObservabilityBus::default();
        let delivered = bus.publish(ObservabilityEvent::RefreshRequested);
        assert_eq!(delivered, 0);
    }

    #[test]
    fn subscriber_count_tracks_receivers() {
        let bus = ObservabilityBus::default();
        assert_eq!(bus.subscriber_count(), 0);

        let _rx1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let _rx2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        drop(_rx1);
        assert_eq!(bus.subscriber_count(), 1);
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snapshot = OrchestratorSnapshot {
            running: vec![RunningIssueInfo {
                issue_id: "id-1".into(),
                identifier: "ENG-42".into(),
                title: "Fix the bug".into(),
                state: "In Progress".into(),
                workspace_path: PathBuf::from("/tmp/ws/eng-42"),
                session_id: Some("sess-abc".into()),
                tokens_used: 1500,
                turn_count: 3,
                started_at: Utc::now(),
                elapsed_seconds: 120,
            }],
            retrying: vec![RetryingIssueInfo {
                issue_id: "id-2".into(),
                identifier: "ENG-43".into(),
                attempt_count: 2,
                next_retry_at: Utc::now(),
                last_error: Some("timeout".into()),
            }],
            token_totals: TokenTotals {
                input_tokens: 1000,
                output_tokens: 500,
                total_tokens: 1500,
            },
            rate_limits: Some(RateLimitInfo {
                raw: serde_json::json!({"requests_remaining": 100}),
            }),
            timestamp: Utc::now(),
        };

        let json = serde_json::to_string(&snapshot).expect("serialize");
        let deserialized: OrchestratorSnapshot =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.running.len(), 1);
        assert_eq!(deserialized.running[0].identifier, "ENG-42");
        assert_eq!(deserialized.retrying.len(), 1);
        assert_eq!(deserialized.retrying[0].attempt_count, 2);
        assert_eq!(deserialized.token_totals.total_tokens, 1500);
    }

    #[test]
    fn event_serialization_roundtrip() {
        let event = ObservabilityEvent::RefreshRequested;
        let json = serde_json::to_string(&event).expect("serialize");
        let deserialized: ObservabilityEvent =
            serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(deserialized, ObservabilityEvent::RefreshRequested));
    }

    #[test]
    fn default_bus_uses_default_capacity() {
        // Smoke test: ensure Default impl works without panicking.
        let bus = ObservabilityBus::default();
        assert_eq!(bus.subscriber_count(), 0);
    }
}
