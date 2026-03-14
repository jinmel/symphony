use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::agent::{AgentError, AgentUpdate, AppServerSession};
use crate::config::{ConfigError, EffectiveConfig, normalize_issue_state};
use crate::dashboard::{
    self, DashboardState, RateLimitInfo as DashboardRateLimitInfo, ThroughputTracker,
};
use crate::issue::Issue;
use crate::prompt::{PromptError, build_prompt, continuation_prompt};
use crate::pubsub::{self, ObservabilityBus, OrchestratorSnapshot as PubSubSnapshot};
use crate::server::{self, OrchestratorSnapshot as ServerSnapshot, SharedSnapshot};
use crate::tracker::{TrackerClient, TrackerError};
use crate::workflow::{WorkflowRuntime, WorkflowStore, WorkflowStoreError};
use crate::workspace::{
    Workspace, WorkspaceError, prepare_workspace, remove_issue_workspace, remove_workspace_path,
    run_after_run_hook, run_before_run_hook, sanitize_identifier,
};

const CONTINUATION_RETRY_DELAY_MS: u64 = 1_000;
const FAILURE_RETRY_BASE_MS: u64 = 10_000;

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error(transparent)]
    Workflow(#[from] WorkflowStoreError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Handles to observability subsystems that the orchestrator pushes state into.
///
/// All fields are optional so that the orchestrator can run in a "headless"
/// mode (e.g. in tests) without any observability wiring.
pub struct ObservabilityHandles {
    /// Broadcast bus for observability events (pubsub subscribers).
    pub bus: Option<ObservabilityBus>,
    /// Shared snapshot consumed by the HTTP server.
    pub shared_snapshot: Option<SharedSnapshot>,
    /// Watch channel sender for the terminal dashboard.
    pub dashboard_tx: Option<watch::Sender<DashboardState>>,
}

impl std::fmt::Debug for ObservabilityHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObservabilityHandles")
            .field("bus", &self.bus.is_some())
            .field("shared_snapshot", &self.shared_snapshot.is_some())
            .field("dashboard_tx", &self.dashboard_tx.is_some())
            .finish()
    }
}

impl Default for ObservabilityHandles {
    fn default() -> Self {
        Self {
            bus: None,
            shared_snapshot: None,
            dashboard_tx: None,
        }
    }
}

#[derive(Debug)]
pub struct Orchestrator {
    workflow_store: Arc<WorkflowStore>,
    tracker: TrackerClient,
    tx: UnboundedSender<OrchestratorEvent>,
    rx: UnboundedReceiver<OrchestratorEvent>,
    state: OrchestratorState,
    observability: ObservabilityHandles,
    started_at: Instant,
}

#[derive(Debug)]
struct OrchestratorState {
    poll_interval_ms: u64,
    max_concurrent_agents: usize,
    max_retry_backoff_ms: u64,
    running: HashMap<String, RunningEntry>,
    claimed: HashSet<String>,
    retry_attempts: HashMap<String, RetryEntry>,
    completed: HashSet<String>,
    codex_totals: AggregatedTotals,
    codex_rate_limits: Option<Value>,
    retry_token_counter: u64,
    throughput_tracker: ThroughputTracker,
}

#[derive(Debug, Default, Clone)]
struct AggregatedTotals {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    seconds_running: u64,
}

#[derive(Debug)]
struct RunningEntry {
    handle: JoinHandle<()>,
    cancellation: CancellationToken,
    issue: Issue,
    identifier: String,
    workspace_path: PathBuf,
    session_id: Option<String>,
    codex_app_server_pid: Option<String>,
    last_codex_message: Option<String>,
    last_codex_event: Option<String>,
    last_codex_timestamp: Option<DateTime<Utc>>,
    codex_input_tokens: u64,
    codex_output_tokens: u64,
    codex_total_tokens: u64,
    last_reported_input_tokens: u64,
    last_reported_output_tokens: u64,
    last_reported_total_tokens: u64,
    turn_count: u32,
    retry_attempt: u32,
    started_at: DateTime<Utc>,
    stopping: bool,
    suppress_retry: bool,
    cleanup_workspace_on_exit: bool,
    forced_retry_error: Option<String>,
}

#[derive(Debug)]
struct RetryEntry {
    attempt: u32,
    token: u64,
    due_at: Instant,
    identifier: String,
    error: Option<String>,
    handle: JoinHandle<()>,
}

#[derive(Debug)]
enum WorkerOutcome {
    Succeeded,
    Failed(String),
    Cancelled,
}

#[derive(Debug)]
enum RetryDelayKind {
    Continuation,
    Failure,
}

#[derive(Debug)]
enum OrchestratorEvent {
    WorkerExited {
        issue_id: String,
        outcome: WorkerOutcome,
    },
    CodexUpdate {
        issue_id: String,
        update: AgentUpdate,
    },
    RetryDue {
        issue_id: String,
        token: u64,
    },
}

impl Orchestrator {
    pub fn new(workflow_store: Arc<WorkflowStore>, observability: ObservabilityHandles) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            workflow_store,
            tracker: TrackerClient::new(),
            tx,
            rx,
            state: OrchestratorState {
                poll_interval_ms: 30_000,
                max_concurrent_agents: 10,
                max_retry_backoff_ms: 300_000,
                running: HashMap::new(),
                claimed: HashSet::new(),
                retry_attempts: HashMap::new(),
                completed: HashSet::new(),
                codex_totals: AggregatedTotals::default(),
                codex_rate_limits: None,
                retry_token_counter: 0,
                throughput_tracker: ThroughputTracker::new(),
            },
            observability,
            started_at: Instant::now(),
        }
    }

    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), OrchestratorError> {
        let startup_runtime = self.workflow_store.current().await;
        startup_runtime.config.validate_dispatch()?;
        self.refresh_runtime_config(&startup_runtime.config);
        self.startup_terminal_workspace_cleanup(&startup_runtime.config)
            .await;

        // Publish initial state.
        self.publish_state().await;

        let mut next_tick = Instant::now();

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    self.shutdown().await;
                    return Ok(());
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_tick)) => {
                    self.handle_tick().await;
                    self.publish_state().await;
                    next_tick = Instant::now() + Duration::from_millis(self.state.poll_interval_ms);
                }
                Some(event) = self.rx.recv() => {
                    self.handle_event(event).await;
                    self.publish_state().await;
                }
            }
        }
    }

    async fn handle_tick(&mut self) {
        let runtime = self.workflow_store.current().await;
        self.refresh_runtime_config(&runtime.config);
        self.reconcile_running_issues(&runtime.config).await;

        if let Err(error) = runtime.config.validate_dispatch() {
            error!(error = %error, "dispatch validation failed");
            return;
        }

        let issues = match self.tracker.fetch_candidate_issues(&runtime.config).await {
            Ok(issues) => issues,
            Err(error) => {
                error!(error = %error, "candidate issue fetch failed");
                return;
            }
        };

        let mut sorted = issues;
        sorted.sort_by(|left, right| dispatch_sort_key(left).cmp(&dispatch_sort_key(right)));

        for issue in sorted {
            if self.available_slots() == 0 {
                break;
            }

            if self.should_dispatch_issue(&issue, &runtime.config) {
                self.dispatch_issue(issue, 0, runtime.clone()).await;
            }
        }
    }

    async fn handle_event(&mut self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::WorkerExited { issue_id, outcome } => {
                self.handle_worker_exit(&issue_id, outcome).await;
            }
            OrchestratorEvent::CodexUpdate { issue_id, update } => {
                self.integrate_codex_update(&issue_id, update);
            }
            OrchestratorEvent::RetryDue { issue_id, token } => {
                self.handle_retry_due(&issue_id, token).await;
            }
        }
    }

    fn refresh_runtime_config(&mut self, config: &EffectiveConfig) {
        self.state.poll_interval_ms = config.polling.interval_ms;
        self.state.max_concurrent_agents = config.agent.max_concurrent_agents;
        self.state.max_retry_backoff_ms = config.agent.max_retry_backoff_ms;
    }

    async fn startup_terminal_workspace_cleanup(&self, config: &EffectiveConfig) {
        match self
            .tracker
            .fetch_issues_by_states(config, &config.tracker.terminal_states)
            .await
        {
            Ok(issues) => {
                for issue in issues {
                    if let Err(error) = remove_issue_workspace(config, &issue.identifier).await {
                        warn!(
                            issue_id = %issue.id,
                            issue_identifier = %issue.identifier,
                            error = %error,
                            "terminal workspace cleanup failed"
                        );
                    }
                }
            }
            Err(error) => warn!(error = %error, "startup terminal workspace cleanup skipped"),
        }
    }

    async fn reconcile_running_issues(&mut self, config: &EffectiveConfig) {
        self.reconcile_stalled_running_issues(config);

        let running_ids: Vec<String> = self
            .state
            .running
            .iter()
            .filter_map(|(issue_id, entry)| (!entry.stopping).then_some(issue_id.clone()))
            .collect();
        if running_ids.is_empty() {
            return;
        }

        let refreshed = match self
            .tracker
            .fetch_issue_states_by_ids(config, &running_ids)
            .await
        {
            Ok(issues) => issues,
            Err(error) => {
                debug!(error = %error, "running issue refresh failed; keeping active workers");
                return;
            }
        };

        let refreshed_map: HashMap<String, Issue> = refreshed
            .into_iter()
            .map(|issue| (issue.id.clone(), issue))
            .collect();
        let active_states: HashSet<String> =
            config.normalized_active_states().into_iter().collect();
        let terminal_states: HashSet<String> =
            config.normalized_terminal_states().into_iter().collect();

        for issue_id in running_ids {
            match refreshed_map.get(&issue_id) {
                Some(issue) if terminal_states.contains(&normalize_issue_state(&issue.state)) => {
                    info!(
                        issue_id = %issue.id,
                        issue_identifier = %issue.identifier,
                        state = %issue.state,
                        "issue entered terminal state; stopping worker and cleaning workspace"
                    );
                    self.mark_running_for_stop(&issue.id, true, true, None);
                }
                Some(issue) if !issue.assigned_to_worker => {
                    info!(
                        issue_id = %issue.id,
                        issue_identifier = %issue.identifier,
                        assignee_id = issue.assignee_id.as_deref().unwrap_or("none"),
                        "issue no longer routed to this worker; stopping agent"
                    );
                    self.mark_running_for_stop(&issue.id, true, false, None);
                }
                Some(issue) if active_states.contains(&normalize_issue_state(&issue.state)) => {
                    if let Some(entry) = self.state.running.get_mut(&issue.id) {
                        entry.issue = issue.clone();
                    }
                }
                Some(issue) => {
                    info!(
                        issue_id = %issue.id,
                        issue_identifier = %issue.identifier,
                        state = %issue.state,
                        "issue left active states; stopping worker without cleanup"
                    );
                    self.mark_running_for_stop(&issue.id, true, false, None);
                }
                None => {
                    if let Some(entry) = self.state.running.get(&issue_id) {
                        info!(
                            issue_id = %issue_id,
                            issue_identifier = %entry.identifier,
                            "issue disappeared from refresh; stopping worker without cleanup"
                        );
                    }
                    self.mark_running_for_stop(&issue_id, true, false, None);
                }
            }
        }
    }

    fn reconcile_stalled_running_issues(&mut self, config: &EffectiveConfig) {
        if config.codex.stall_timeout_ms <= 0 {
            return;
        }

        let now = Utc::now();
        let timeout_ms = config.codex.stall_timeout_ms as i64;
        let stalled_ids: Vec<String> = self
            .state
            .running
            .iter()
            .filter_map(|(issue_id, entry)| {
                if entry.stopping {
                    return None;
                }

                let baseline = entry.last_codex_timestamp.unwrap_or(entry.started_at);
                let elapsed = now.signed_duration_since(baseline).num_milliseconds();
                (elapsed > timeout_ms).then_some((issue_id.clone(), elapsed))
            })
            .map(|(issue_id, elapsed)| {
                if let Some(entry) = self.state.running.get(&issue_id) {
                    warn!(
                        issue_id = %issue_id,
                        issue_identifier = %entry.identifier,
                        session_id = entry.session_id.as_deref().unwrap_or("n/a"),
                        elapsed_ms = elapsed,
                        "running issue stalled; restarting with backoff"
                    );
                }
                issue_id
            })
            .collect();

        for issue_id in stalled_ids {
            self.mark_running_for_stop(
                &issue_id,
                false,
                false,
                Some("stalled without codex activity".to_owned()),
            );
        }
    }

    fn should_dispatch_issue(&self, issue: &Issue, config: &EffectiveConfig) -> bool {
        if !issue.has_required_dispatch_fields() {
            return false;
        }

        if !issue.assigned_to_worker {
            return false;
        }

        let active_states: HashSet<String> =
            config.normalized_active_states().into_iter().collect();
        let terminal_states: HashSet<String> =
            config.normalized_terminal_states().into_iter().collect();

        let normalized_state = normalize_issue_state(&issue.state);
        if !active_states.contains(&normalized_state) || terminal_states.contains(&normalized_state)
        {
            return false;
        }

        if self.state.claimed.contains(&issue.id) || self.state.running.contains_key(&issue.id) {
            return false;
        }

        if self.available_slots() == 0 {
            return false;
        }

        if normalize_issue_state(&issue.state) == "todo"
            && issue.blocked_by.iter().any(|blocker| {
                blocker
                    .state
                    .as_deref()
                    .map(|state| !terminal_states.contains(&normalize_issue_state(state)))
                    .unwrap_or(true)
            })
        {
            return false;
        }

        let running_for_state = self
            .state
            .running
            .values()
            .filter(|entry| normalize_issue_state(&entry.issue.state) == normalized_state)
            .count();
        running_for_state < config.max_concurrent_agents_for_state(&issue.state)
    }

    async fn dispatch_issue(&mut self, issue: Issue, attempt: u32, runtime: WorkflowRuntime) {
        let ids = vec![issue.id.clone()];
        let refreshed_issue = match self
            .tracker
            .fetch_issue_states_by_ids(&runtime.config, &ids)
            .await
        {
            Ok(mut issues) => issues.pop(),
            Err(error) => {
                warn!(
                    issue_id = %issue.id,
                    issue_identifier = %issue.identifier,
                    error = %error,
                    "skipping dispatch; issue refresh failed"
                );
                return;
            }
        };

        let Some(issue) = refreshed_issue else {
            return;
        };
        if !self.should_dispatch_issue(&issue, &runtime.config) {
            return;
        }

        let cancellation = CancellationToken::new();
        let issue_id = issue.id.clone();
        let identifier = issue.identifier.clone();
        let workspace_path = runtime
            .config
            .workspace
            .root
            .join(sanitize_identifier(&issue.identifier));
        let tx = self.tx.clone();
        let tracker = self.tracker.clone();
        let runtime_clone = runtime.clone();
        let cancellation_for_worker = cancellation.clone();
        let issue_for_worker = issue.clone();

        let handle = tokio::spawn(async move {
            let outcome = run_worker_attempt(
                issue_for_worker,
                attempt,
                runtime_clone,
                tracker,
                cancellation_for_worker,
                tx.clone(),
            )
            .await;
            let _ = tx.send(OrchestratorEvent::WorkerExited { issue_id, outcome });
        });

        self.state.claimed.insert(issue.id.clone());
        if let Some(retry) = self.state.retry_attempts.remove(&issue.id) {
            retry.handle.abort();
        }
        self.state.running.insert(
            issue.id.clone(),
            RunningEntry {
                handle,
                cancellation,
                issue: issue.clone(),
                identifier,
                workspace_path,
                session_id: None,
                codex_app_server_pid: None,
                last_codex_message: None,
                last_codex_event: None,
                last_codex_timestamp: None,
                codex_input_tokens: 0,
                codex_output_tokens: 0,
                codex_total_tokens: 0,
                last_reported_input_tokens: 0,
                last_reported_output_tokens: 0,
                last_reported_total_tokens: 0,
                turn_count: 0,
                retry_attempt: attempt,
                started_at: Utc::now(),
                stopping: false,
                suppress_retry: false,
                cleanup_workspace_on_exit: false,
                forced_retry_error: None,
            },
        );
    }

    async fn handle_worker_exit(&mut self, issue_id: &str, outcome: WorkerOutcome) {
        let Some(entry) = self.state.running.remove(issue_id) else {
            return;
        };

        self.state.codex_totals.seconds_running += duration_seconds(entry.started_at, Utc::now());

        if entry.cleanup_workspace_on_exit {
            let current = self.workflow_store.current().await;
            if let Err(error) = remove_workspace_path(&current.config, &entry.workspace_path).await
            {
                warn!(
                    issue_id = %issue_id,
                    issue_identifier = %entry.identifier,
                    error = %error,
                    "workspace cleanup failed after worker exit"
                );
            }
        }

        if entry.suppress_retry {
            self.state.claimed.remove(issue_id);
            self.state.retry_attempts.remove(issue_id);
            return;
        }

        match outcome {
            WorkerOutcome::Succeeded => {
                self.state.completed.insert(issue_id.to_owned());
                self.schedule_retry(
                    issue_id.to_owned(),
                    1,
                    entry.identifier,
                    None,
                    RetryDelayKind::Continuation,
                );
            }
            WorkerOutcome::Failed(reason) => {
                let attempt = if entry.retry_attempt > 0 {
                    entry.retry_attempt + 1
                } else {
                    1
                };
                self.schedule_retry(
                    issue_id.to_owned(),
                    attempt,
                    entry.identifier,
                    entry.forced_retry_error.or(Some(reason)),
                    RetryDelayKind::Failure,
                );
            }
            WorkerOutcome::Cancelled => {
                let attempt = if entry.retry_attempt > 0 {
                    entry.retry_attempt + 1
                } else {
                    1
                };
                self.schedule_retry(
                    issue_id.to_owned(),
                    attempt,
                    entry.identifier,
                    entry
                        .forced_retry_error
                        .or(Some("worker cancelled".to_owned())),
                    RetryDelayKind::Failure,
                );
            }
        }
    }

    fn integrate_codex_update(&mut self, issue_id: &str, update: AgentUpdate) {
        let Some(entry) = self.state.running.get_mut(issue_id) else {
            return;
        };

        entry.last_codex_event = Some(update.event.clone());
        entry.last_codex_timestamp = Some(update.timestamp);
        entry.last_codex_message = update
            .raw_line
            .as_deref()
            .map(|line| truncate_line(line))
            .or_else(|| Some(update.event.clone()));
        if let Some(pid) = update.codex_app_server_pid.as_ref() {
            entry.codex_app_server_pid = Some(pid.clone());
        }

        if let Some(session_id) = update.session_id.as_ref() {
            entry.session_id = Some(session_id.clone());
            entry.turn_count += 1;
        }

        if let Some(rate_limits) = update.rate_limits.as_ref() {
            self.state.codex_rate_limits = Some(rate_limits.clone());
        }

        if let Some(usage) = extract_absolute_token_usage(&update) {
            let input_delta = usage
                .input_tokens
                .saturating_sub(entry.last_reported_input_tokens);
            let output_delta = usage
                .output_tokens
                .saturating_sub(entry.last_reported_output_tokens);
            let total_delta = usage
                .total_tokens
                .saturating_sub(entry.last_reported_total_tokens);

            self.state.codex_totals.input_tokens += input_delta;
            self.state.codex_totals.output_tokens += output_delta;
            self.state.codex_totals.total_tokens += total_delta;

            entry.codex_input_tokens = usage.input_tokens;
            entry.codex_output_tokens = usage.output_tokens;
            entry.codex_total_tokens = usage.total_tokens;
            entry.last_reported_input_tokens = usage.input_tokens;
            entry.last_reported_output_tokens = usage.output_tokens;
            entry.last_reported_total_tokens = usage.total_tokens;
        }
    }

    async fn handle_retry_due(&mut self, issue_id: &str, token: u64) {
        let Some(entry) = self.state.retry_attempts.remove(issue_id) else {
            return;
        };
        if entry.token != token {
            self.state.retry_attempts.insert(issue_id.to_owned(), entry);
            return;
        }

        debug!(
            issue_id = %issue_id,
            issue_identifier = %entry.identifier,
            due_in_ms = entry.due_at.saturating_duration_since(Instant::now()).as_millis(),
            previous_error = entry.error.as_deref().unwrap_or(""),
            "retry timer fired"
        );

        let runtime = self.workflow_store.current().await;
        let issues = match self.tracker.fetch_candidate_issues(&runtime.config).await {
            Ok(issues) => issues,
            Err(error) => {
                warn!(issue_id = %issue_id, error = %error, "retry poll failed");
                self.schedule_retry(
                    issue_id.to_owned(),
                    entry.attempt + 1,
                    entry.identifier,
                    Some("retry poll failed".to_owned()),
                    RetryDelayKind::Failure,
                );
                return;
            }
        };

        let Some(issue) = issues.into_iter().find(|issue| issue.id == issue_id) else {
            self.state.claimed.remove(issue_id);
            return;
        };

        if !self.should_dispatch_issue(&issue, &runtime.config) {
            self.state.claimed.remove(issue_id);
            return;
        }

        if self.available_slots() == 0 {
            self.schedule_retry(
                issue_id.to_owned(),
                entry.attempt + 1,
                issue.identifier.clone(),
                Some("no available orchestrator slots".to_owned()),
                RetryDelayKind::Failure,
            );
            return;
        }

        self.dispatch_issue(issue, entry.attempt, runtime).await;
    }

    fn mark_running_for_stop(
        &mut self,
        issue_id: &str,
        suppress_retry: bool,
        cleanup_workspace_on_exit: bool,
        forced_retry_error: Option<String>,
    ) {
        let Some(entry) = self.state.running.get_mut(issue_id) else {
            return;
        };

        if entry.stopping {
            return;
        }

        entry.stopping = true;
        entry.suppress_retry = suppress_retry;
        entry.cleanup_workspace_on_exit = cleanup_workspace_on_exit;
        entry.forced_retry_error = forced_retry_error;
        entry.cancellation.cancel();
    }

    fn schedule_retry(
        &mut self,
        issue_id: String,
        attempt: u32,
        identifier: String,
        error: Option<String>,
        delay_kind: RetryDelayKind,
    ) {
        if let Some(existing) = self.state.retry_attempts.remove(&issue_id) {
            existing.handle.abort();
        }

        self.state.retry_token_counter += 1;
        let token = self.state.retry_token_counter;
        let delay_ms = retry_delay_ms(attempt, &delay_kind, self.state.max_retry_backoff_ms);
        let tx = self.tx.clone();
        let issue_id_for_task = issue_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let _ = tx.send(OrchestratorEvent::RetryDue {
                issue_id: issue_id_for_task,
                token,
            });
        });

        warn!(
            issue_id = %issue_id,
            issue_identifier = %identifier,
            attempt = attempt,
            delay_ms = delay_ms,
            error = error.as_deref().unwrap_or(""),
            "retry scheduled"
        );

        self.state.retry_attempts.insert(
            issue_id,
            RetryEntry {
                attempt,
                token,
                due_at: Instant::now() + Duration::from_millis(delay_ms),
                identifier,
                error,
                handle,
            },
        );
    }

    /// Build a pubsub snapshot from the current orchestrator state and push it
    /// to all observability sinks (bus, shared snapshot, dashboard channel).
    async fn publish_state(&mut self) {
        let now = Utc::now();
        let uptime = self.started_at.elapsed();

        // Record throughput sample before building dashboard state.
        self.state
            .throughput_tracker
            .record(self.state.codex_totals.total_tokens);

        // -- Build pubsub snapshot -----------------------------------------------
        let running: Vec<pubsub::RunningIssueInfo> = self
            .state
            .running
            .values()
            .map(|entry| {
                let elapsed = now
                    .signed_duration_since(entry.started_at)
                    .num_seconds()
                    .max(0) as u64;
                pubsub::RunningIssueInfo {
                    issue_id: entry.issue.id.clone(),
                    identifier: entry.identifier.clone(),
                    title: entry.issue.title.clone(),
                    state: entry.issue.state.clone(),
                    workspace_path: entry.workspace_path.clone(),
                    session_id: entry.session_id.clone(),
                    tokens_used: entry.codex_total_tokens,
                    turn_count: entry.turn_count,
                    started_at: entry.started_at,
                    elapsed_seconds: elapsed,
                }
            })
            .collect();

        let retrying: Vec<pubsub::RetryingIssueInfo> = self
            .state
            .retry_attempts
            .iter()
            .map(|(issue_id, entry)| {
                let remaining = entry.due_at.saturating_duration_since(Instant::now());
                let next_retry_at = now + chrono::Duration::from_std(remaining).unwrap_or_default();
                pubsub::RetryingIssueInfo {
                    issue_id: issue_id.clone(),
                    identifier: entry.identifier.clone(),
                    attempt_count: entry.attempt,
                    next_retry_at,
                    last_error: entry.error.clone(),
                }
            })
            .collect();

        let pubsub_snapshot = PubSubSnapshot {
            running,
            retrying,
            token_totals: pubsub::TokenTotals {
                input_tokens: self.state.codex_totals.input_tokens,
                output_tokens: self.state.codex_totals.output_tokens,
                total_tokens: self.state.codex_totals.total_tokens,
            },
            rate_limits: self
                .state
                .codex_rate_limits
                .as_ref()
                .map(|raw| pubsub::RateLimitInfo { raw: raw.clone() }),
            timestamp: now,
        };

        // -- Publish to bus ------------------------------------------------------
        if let Some(ref bus) = self.observability.bus {
            bus.publish_snapshot(pubsub_snapshot.clone());
        }

        // -- Update shared snapshot for the HTTP server --------------------------
        if let Some(ref shared) = self.observability.shared_snapshot {
            let server_snap = self.build_server_snapshot(now);
            shared.set(server_snap).await;
        }

        // -- Send to dashboard watch channel -------------------------------------
        if self.observability.dashboard_tx.is_some() {
            let dash_state = self.build_dashboard_state(now, uptime);
            // Best-effort send; if no receiver, this is a no-op.
            if let Some(ref tx) = self.observability.dashboard_tx {
                let _ = tx.send(dash_state);
            }
        }
    }

    /// Build a server-module snapshot directly from orchestrator state,
    /// including per-entry details (last_event, tokens breakdown, etc.).
    fn build_server_snapshot(&self, now: DateTime<Utc>) -> ServerSnapshot {
        let running = self
            .state
            .running
            .values()
            .map(|entry| server::RunningIssueSnapshot {
                issue_id: entry.issue.id.clone(),
                issue_identifier: entry.identifier.clone(),
                state: entry.issue.state.clone(),
                worker_host: None,
                workspace_path: Some(entry.workspace_path.clone()),
                session_id: entry.session_id.clone(),
                turn_count: entry.turn_count,
                last_event: entry.last_codex_event.clone(),
                last_message: entry.last_codex_message.clone(),
                started_at: Some(entry.started_at),
                last_event_at: entry.last_codex_timestamp,
                tokens: server::TokenTotals {
                    input_tokens: entry.codex_input_tokens,
                    output_tokens: entry.codex_output_tokens,
                    total_tokens: entry.codex_total_tokens,
                },
            })
            .collect();

        let retrying = self
            .state
            .retry_attempts
            .iter()
            .map(|(issue_id, entry)| {
                let remaining = entry.due_at.saturating_duration_since(Instant::now());
                let due_at = now + chrono::Duration::from_std(remaining).unwrap_or_default();
                server::RetryingIssueSnapshot {
                    issue_id: issue_id.clone(),
                    issue_identifier: entry.identifier.clone(),
                    attempt: entry.attempt,
                    due_at: Some(due_at),
                    error: entry.error.clone(),
                    worker_host: None,
                    workspace_path: None,
                }
            })
            .collect();

        ServerSnapshot {
            running,
            retrying,
            codex_totals: server::TokenTotals {
                input_tokens: self.state.codex_totals.input_tokens,
                output_tokens: self.state.codex_totals.output_tokens,
                total_tokens: self.state.codex_totals.total_tokens,
            },
            rate_limits: self.state.codex_rate_limits.clone(),
        }
    }

    /// Build a dashboard state directly from orchestrator state,
    /// including per-entry details.
    fn build_dashboard_state(&mut self, now: DateTime<Utc>, uptime: Duration) -> DashboardState {
        let running = self
            .state
            .running
            .values()
            .map(|entry| {
                let elapsed = Duration::from_secs(
                    now.signed_duration_since(entry.started_at)
                        .num_seconds()
                        .max(0) as u64,
                );
                dashboard::RunningAgentInfo {
                    identifier: entry.identifier.clone(),
                    title: entry.issue.title.clone(),
                    state: entry.issue.state.clone(),
                    elapsed,
                    input_tokens: entry.codex_input_tokens,
                    output_tokens: entry.codex_output_tokens,
                    total_tokens: entry.codex_total_tokens,
                    turn_count: entry.turn_count,
                    last_event: entry.last_codex_event.clone(),
                    last_message: entry.last_codex_message.clone(),
                    session_id: entry.session_id.clone(),
                }
            })
            .collect();

        let retrying = self
            .state
            .retry_attempts
            .iter()
            .map(|(_issue_id, entry)| {
                let remaining = entry.due_at.saturating_duration_since(Instant::now());
                dashboard::RetryInfo {
                    identifier: entry.identifier.clone(),
                    next_retry_in: remaining,
                    attempt: entry.attempt,
                    error: entry.error.clone(),
                }
            })
            .collect();

        DashboardState {
            running,
            retrying,
            token_totals: dashboard::TokenTotals {
                input_tokens: self.state.codex_totals.input_tokens,
                output_tokens: self.state.codex_totals.output_tokens,
                total_tokens: self.state.codex_totals.total_tokens,
            },
            rate_limits: self.state.codex_rate_limits.as_ref().map(|raw| {
                DashboardRateLimitInfo {
                    raw: Some(raw.clone()),
                }
            }),
            uptime,
            max_agents: self.state.max_concurrent_agents,
            throughput_tps: self.state.throughput_tracker.tokens_per_second(),
            throughput_sparkline: self.state.throughput_tracker.sparkline_graph(),
        }
    }

    fn available_slots(&self) -> usize {
        self.state
            .max_concurrent_agents
            .saturating_sub(self.state.running.len())
    }

    async fn shutdown(&mut self) {
        for retry in self.state.retry_attempts.drain().map(|(_, retry)| retry) {
            retry.handle.abort();
        }

        for entry in self.state.running.values_mut() {
            entry.cancellation.cancel();
            entry.handle.abort();
        }
        self.state.running.clear();
        self.state.claimed.clear();
    }
}

async fn run_worker_attempt(
    issue: Issue,
    attempt: u32,
    runtime: WorkflowRuntime,
    tracker: TrackerClient,
    cancellation: CancellationToken,
    tx: UnboundedSender<OrchestratorEvent>,
) -> WorkerOutcome {
    match run_worker_attempt_inner(&issue, attempt, &runtime, &tracker, &cancellation, &tx).await {
        Ok(()) => WorkerOutcome::Succeeded,
        Err(WorkerRunError::Cancelled) => WorkerOutcome::Cancelled,
        Err(error) => WorkerOutcome::Failed(error.to_string()),
    }
}

async fn run_worker_attempt_inner(
    issue: &Issue,
    attempt: u32,
    runtime: &WorkflowRuntime,
    tracker: &TrackerClient,
    cancellation: &CancellationToken,
    tx: &UnboundedSender<OrchestratorEvent>,
) -> Result<(), WorkerRunError> {
    let workspace = prepare_workspace(&runtime.config, &issue.identifier).await?;
    let run_result = run_worker_turns(
        issue,
        attempt,
        runtime,
        tracker,
        cancellation,
        tx,
        &workspace,
    )
    .await;
    run_after_run_hook(&runtime.config, &workspace, issue).await;
    run_result
}

async fn run_worker_turns(
    issue: &Issue,
    attempt: u32,
    runtime: &WorkflowRuntime,
    tracker: &TrackerClient,
    cancellation: &CancellationToken,
    tx: &UnboundedSender<OrchestratorEvent>,
    workspace: &Workspace,
) -> Result<(), WorkerRunError> {
    run_before_run_hook(&runtime.config, workspace, issue).await?;

    let mut session =
        AppServerSession::start(&runtime.config, &workspace.path, cancellation).await?;
    let mut issue = issue.clone();
    let max_turns = runtime.config.agent.max_turns;
    let result = async {
        for turn_number in 1..=max_turns {
            if cancellation.is_cancelled() {
                return Err(WorkerRunError::Cancelled);
            }

            let prompt = if turn_number == 1 {
                build_prompt(
                    &runtime.definition,
                    &issue,
                    (attempt > 0).then_some(attempt),
                )?
            } else {
                continuation_prompt(turn_number, max_turns)
            };

            let issue_id = issue.id.clone();
            let tx_clone = tx.clone();
            session
                .run_turn(&prompt, &issue, cancellation, move |update| {
                    let _ = tx_clone.send(OrchestratorEvent::CodexUpdate {
                        issue_id: issue_id.clone(),
                        update,
                    });
                })
                .await?;

            let refreshed = tracker
                .fetch_issue_states_by_ids(&runtime.config, &[issue.id.clone()])
                .await?;
            if let Some(refreshed_issue) = refreshed.into_iter().next() {
                issue = refreshed_issue;
            }

            if !runtime
                .config
                .normalized_active_states()
                .contains(&normalize_issue_state(&issue.state))
            {
                return Ok(());
            }

            if turn_number >= max_turns {
                return Ok(());
            }
        }

        Ok(())
    }
    .await;

    session.stop().await;
    result
}

#[derive(Debug, Error)]
enum WorkerRunError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Tracker(#[from] TrackerError),
    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug)]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

fn extract_absolute_token_usage(update: &AgentUpdate) -> Option<TokenUsage> {
    let payload = update.raw_payload.as_ref()?;
    if let Some(total_usage) = recursive_find(payload, &["total_token_usage", "totalTokenUsage"]) {
        return parse_token_usage(&total_usage);
    }

    let method = payload
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == "thread/tokenUsage/updated" || method.contains("tokenUsage") {
        for key in ["usage", "token_usage", "tokenUsage"] {
            if let Some(value) = recursive_find(payload, &[key]) {
                if let Some(parsed) = parse_token_usage(&value) {
                    return Some(parsed);
                }
            }
        }
    }

    None
}

fn parse_token_usage(value: &Value) -> Option<TokenUsage> {
    let object = value.as_object()?;
    let input_tokens = object
        .get("input_tokens")
        .or_else(|| object.get("inputTokens"))
        .or_else(|| object.get("prompt_tokens"))
        .and_then(Value::as_u64)?;
    let output_tokens = object
        .get("output_tokens")
        .or_else(|| object.get("outputTokens"))
        .or_else(|| object.get("completion_tokens"))
        .and_then(Value::as_u64)?;
    let total_tokens = object
        .get("total_tokens")
        .or_else(|| object.get("totalTokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input_tokens + output_tokens);
    Some(TokenUsage {
        input_tokens,
        output_tokens,
        total_tokens,
    })
}

fn recursive_find(payload: &Value, keys: &[&str]) -> Option<Value> {
    match payload {
        Value::Object(map) => {
            for (key, value) in map {
                if keys.iter().any(|candidate| key == candidate) {
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

fn retry_delay_ms(attempt: u32, delay_kind: &RetryDelayKind, max_retry_backoff_ms: u64) -> u64 {
    match delay_kind {
        RetryDelayKind::Continuation if attempt == 1 => CONTINUATION_RETRY_DELAY_MS,
        _ => {
            let power = attempt.saturating_sub(1).min(10);
            let delay = FAILURE_RETRY_BASE_MS.saturating_mul(1_u64 << power);
            delay.min(max_retry_backoff_ms.max(FAILURE_RETRY_BASE_MS))
        }
    }
}

fn dispatch_sort_key(issue: &Issue) -> (u8, i64, String) {
    let priority = match issue.priority {
        Some(priority) if (1..=4).contains(&priority) => priority as u8,
        _ => 5,
    };
    let created_at = issue
        .created_at
        .map(|value| value.timestamp_micros())
        .unwrap_or(i64::MAX);
    let identifier = if issue.identifier.is_empty() {
        issue.id.clone()
    } else {
        issue.identifier.clone()
    };

    (priority, created_at, identifier)
}

fn duration_seconds(started_at: DateTime<Utc>, ended_at: DateTime<Utc>) -> u64 {
    ended_at
        .signed_duration_since(started_at)
        .num_seconds()
        .max(0) as u64
}

fn truncate_line(line: &str) -> String {
    if line.len() <= 200 {
        line.to_owned()
    } else {
        format!("{}...", &line[..200])
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn sort_prefers_priority_then_oldest() {
        let older = Issue {
            id: "1".to_owned(),
            identifier: "ABC-1".to_owned(),
            title: "Older".to_owned(),
            description: None,
            priority: Some(1),
            state: "Todo".to_owned(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: vec![],
            blocked_by: vec![],
            assigned_to_worker: true,
            created_at: Some(Utc.with_ymd_and_hms(2026, 3, 1, 0, 0, 0).unwrap()),
            updated_at: None,
        };
        let newer = Issue {
            created_at: Some(Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0).unwrap()),
            ..older.clone()
        };
        assert!(dispatch_sort_key(&older) < dispatch_sort_key(&newer));
    }

    #[test]
    fn continuation_retry_delay_is_short() {
        assert_eq!(
            retry_delay_ms(1, &RetryDelayKind::Continuation, 300_000),
            1_000
        );
    }

    #[test]
    fn failure_retry_delay_respects_cap() {
        assert_eq!(retry_delay_ms(10, &RetryDelayKind::Failure, 60_000), 60_000);
    }
}
