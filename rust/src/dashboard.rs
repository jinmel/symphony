//! Terminal status dashboard for Symphony orchestrator.
//!
//! Renders a live-updating terminal UI showing orchestrator status, running agents,
//! retry queue, token accounting, and rate limit information using ANSI escape codes.
//!
//! # Integration
//!
//! To enable the dashboard, add `pub mod dashboard;` to `lib.rs` and wire a
//! `tokio::sync::watch::Sender<DashboardState>` from the orchestrator into the
//! dashboard task. See [`StatusDashboard::run`] for the main loop.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// ANSI escape codes
// ---------------------------------------------------------------------------

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_YELLOW: &str = "\x1b[33m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GRAY: &str = "\x1b[90m";

/// Clear the entire screen and move cursor to the top-left.
const ANSI_CLEAR_SCREEN: &str = "\x1b[2J";
const ANSI_HOME: &str = "\x1b[H";

// ---------------------------------------------------------------------------
// Column widths (mirrors Elixir constants)
// ---------------------------------------------------------------------------

const COL_ID: usize = 8;
const COL_STATE: usize = 14;
const COL_AGE: usize = 12;
const COL_TOKENS: usize = 10;
const COL_TURNS: usize = 6;
const COL_EVENT: usize = 30;

// ---------------------------------------------------------------------------
// Sparkline blocks (Unicode block elements for bar charts)
// ---------------------------------------------------------------------------

const SPARKLINE_BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// ---------------------------------------------------------------------------
// Throughput tracking
// ---------------------------------------------------------------------------

/// Default sliding window for computing tokens/second (5 seconds).
const THROUGHPUT_WINDOW_MS: u64 = 5_000;

/// How long to keep throughput samples for the sparkline graph (10 minutes).
const THROUGHPUT_GRAPH_WINDOW_MS: u64 = 10 * 60 * 1_000;

/// Number of columns in the sparkline graph.
const THROUGHPUT_GRAPH_COLUMNS: usize = 24;

/// Tracks token throughput over a sliding window and maintains a history of
/// throughput samples for sparkline rendering.
///
/// The tracker records `(timestamp_ms, total_tokens)` samples. It computes
/// the instantaneous throughput as the delta between the most recent and oldest
/// samples within the sliding window.
#[derive(Debug, Clone)]
pub struct ThroughputTracker {
    /// Sliding window of `(timestamp_ms, cumulative_total_tokens)`.
    samples: VecDeque<(u64, u64)>,
    /// Origin instant used to derive monotonic millisecond timestamps.
    epoch: Instant,
    /// Width of the sliding window in milliseconds.
    window_ms: u64,
    /// Cached tps value (throttled to 1-second granularity).
    last_tps_second: Option<u64>,
    last_tps_value: f64,
}

impl ThroughputTracker {
    /// Create a new tracker with default settings.
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            epoch: Instant::now(),
            window_ms: THROUGHPUT_WINDOW_MS,
            last_tps_second: None,
            last_tps_value: 0.0,
        }
    }

    /// Record a cumulative token count at the current time.
    pub fn record(&mut self, total_tokens: u64) {
        let now_ms = self.now_ms();
        self.samples.push_back((now_ms, total_tokens));
        self.prune(now_ms);
    }

    /// Current monotonic timestamp in milliseconds relative to the epoch.
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Remove samples older than the graph window (the larger of the two
    /// windows so we keep enough data for sparkline rendering).
    fn prune(&mut self, now_ms: u64) {
        let keep_window = std::cmp::max(self.window_ms, THROUGHPUT_GRAPH_WINDOW_MS);
        let min_ts = now_ms.saturating_sub(keep_window);
        while let Some(&(ts, _)) = self.samples.front() {
            if ts < min_ts {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// Calculate the current tokens-per-second throughput within the sliding
    /// window. The value is throttled: it is recomputed at most once per
    /// calendar second (by integer division of `now_ms / 1000`).
    pub fn tokens_per_second(&mut self) -> f64 {
        let now_ms = self.now_ms();
        let second = now_ms / 1_000;

        if let Some(last_sec) = self.last_tps_second {
            if last_sec == second {
                return self.last_tps_value;
            }
        }

        let tps = self.rolling_tps(now_ms);
        self.last_tps_second = Some(second);
        self.last_tps_value = tps;
        tps
    }

    /// Compute rolling tokens-per-second over the sliding window.
    fn rolling_tps(&self, now_ms: u64) -> f64 {
        let min_ts = now_ms.saturating_sub(self.window_ms);
        // Find oldest sample within the window.
        let window_samples: Vec<&(u64, u64)> = self
            .samples
            .iter()
            .filter(|(ts, _)| *ts >= min_ts)
            .collect();

        if window_samples.len() < 2 {
            return 0.0;
        }

        let (start_ms, start_tokens) = *window_samples[0];
        let (_, end_tokens) = *window_samples[window_samples.len() - 1];
        let elapsed_ms = now_ms.saturating_sub(start_ms);

        if elapsed_ms == 0 {
            return 0.0;
        }

        let delta = end_tokens.saturating_sub(start_tokens);
        delta as f64 / (elapsed_ms as f64 / 1_000.0)
    }

    /// Build a sparkline string for the throughput history.
    ///
    /// Divides the graph window into `THROUGHPUT_GRAPH_COLUMNS` buckets and
    /// computes the average throughput in each bucket, then maps each bucket
    /// to a Unicode block character.
    pub fn sparkline_graph(&self) -> String {
        let now_ms = self.now_ms();
        let bucket_ms = THROUGHPUT_GRAPH_WINDOW_MS / THROUGHPUT_GRAPH_COLUMNS as u64;
        let active_bucket_start = (now_ms / bucket_ms) * bucket_ms;
        let graph_window_start =
            active_bucket_start.saturating_sub((THROUGHPUT_GRAPH_COLUMNS as u64 - 1) * bucket_ms);

        // Build per-pair rates from sorted samples within the graph window.
        let graph_min = now_ms.saturating_sub(THROUGHPUT_GRAPH_WINDOW_MS);
        let mut sorted: Vec<(u64, u64)> = self
            .samples
            .iter()
            .filter(|(ts, _)| *ts >= graph_min)
            .copied()
            .collect();
        sorted.sort_by_key(|(ts, _)| *ts);

        let rates: Vec<(u64, f64)> = sorted
            .windows(2)
            .map(|pair| {
                let (s_ms, s_tok) = pair[0];
                let (e_ms, e_tok) = pair[1];
                let elapsed = e_ms.saturating_sub(s_ms);
                let delta = e_tok.saturating_sub(s_tok);
                let tps = if elapsed == 0 {
                    0.0
                } else {
                    delta as f64 / (elapsed as f64 / 1_000.0)
                };
                (e_ms, tps)
            })
            .collect();

        // Bucket the rates.
        let bucketed: Vec<f64> = (0..THROUGHPUT_GRAPH_COLUMNS)
            .map(|idx| {
                let bucket_start = graph_window_start + idx as u64 * bucket_ms;
                let bucket_end = bucket_start + bucket_ms;
                let last_bucket = idx == THROUGHPUT_GRAPH_COLUMNS - 1;

                let values: Vec<f64> = rates
                    .iter()
                    .filter(|(ts, _)| {
                        if last_bucket {
                            *ts >= bucket_start && *ts <= bucket_end
                        } else {
                            *ts >= bucket_start && *ts < bucket_end
                        }
                    })
                    .map(|(_, tps)| *tps)
                    .collect();

                if values.is_empty() {
                    0.0
                } else {
                    values.iter().sum::<f64>() / values.len() as f64
                }
            })
            .collect();

        sparkline(&bucketed, THROUGHPUT_GRAPH_COLUMNS)
    }
}

impl Default for ThroughputTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a slice of non-negative values as a Unicode sparkline string.
///
/// Each value is mapped to one of the 8 block characters `▁▂▃▄▅▆▇█` relative
/// to the maximum value in the data. The output is exactly `width` characters
/// wide; if `data` has fewer elements than `width` the remaining columns use
/// the lowest block (`▁`).
pub fn sparkline(data: &[f64], width: usize) -> String {
    let max_val = data
        .iter()
        .cloned()
        .fold(0.0_f64, f64::max);

    let mut result = String::with_capacity(width * 3); // UTF-8 block chars are 3 bytes
    for i in 0..width {
        let value = data.get(i).copied().unwrap_or(0.0);
        let index = if max_val <= 0.0 {
            0
        } else {
            ((value / max_val) * (SPARKLINE_BLOCKS.len() - 1) as f64).round() as usize
        };
        let index = index.min(SPARKLINE_BLOCKS.len() - 1);
        result.push(SPARKLINE_BLOCKS[index]);
    }
    result
}

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// Snapshot of a single running agent, suitable for display.
#[derive(Debug, Clone, Default)]
pub struct RunningAgentInfo {
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub elapsed: Duration,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub turn_count: u32,
    pub last_event: Option<String>,
    pub last_message: Option<String>,
    pub session_id: Option<String>,
}

/// Snapshot of a single retry queue entry.
#[derive(Debug, Clone, Default)]
pub struct RetryInfo {
    pub identifier: String,
    pub next_retry_in: Duration,
    pub attempt: u32,
    pub error: Option<String>,
}

/// Token accounting totals.
#[derive(Debug, Clone, Default)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Rate limit information extracted from the agent.
#[derive(Debug, Clone, Default)]
pub struct RateLimitInfo {
    pub raw: Option<Value>,
}

/// Complete state snapshot the dashboard renders each tick.
///
/// Sent from the orchestrator to the dashboard via a `watch` channel.
#[derive(Debug, Clone)]
pub struct DashboardState {
    pub running: Vec<RunningAgentInfo>,
    pub retrying: Vec<RetryInfo>,
    pub token_totals: TokenTotals,
    pub rate_limits: Option<RateLimitInfo>,
    pub uptime: Duration,
    pub max_agents: usize,
    /// Current tokens-per-second throughput.
    pub throughput_tps: f64,
    /// Pre-rendered sparkline graph for the throughput history.
    pub throughput_sparkline: String,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            running: Vec::new(),
            retrying: Vec::new(),
            token_totals: TokenTotals::default(),
            rate_limits: None,
            uptime: Duration::ZERO,
            max_agents: 0,
            throughput_tps: 0.0,
            throughput_sparkline: String::new(),
        }
    }
}

/// Configuration for the status dashboard.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    /// How often to re-render the dashboard.
    pub refresh_interval: Duration,
    /// Whether the dashboard is enabled. When disabled, [`StatusDashboard::run`]
    /// returns immediately.
    pub enabled: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            refresh_interval: Duration::from_secs(1),
            enabled: true,
        }
    }
}

/// The terminal status dashboard.
///
/// Receives [`DashboardState`] snapshots via a `tokio::sync::watch` channel and
/// renders them to stdout at a configurable interval.
pub struct StatusDashboard {
    config: DashboardConfig,
    rx: watch::Receiver<DashboardState>,
}

impl StatusDashboard {
    /// Create a new dashboard. The `rx` end of a watch channel provides state
    /// snapshots produced by the orchestrator.
    pub fn new(config: DashboardConfig, rx: watch::Receiver<DashboardState>) -> Self {
        Self { config, rx }
    }

    /// Run the dashboard render loop until the cancellation token fires.
    ///
    /// If the dashboard is disabled via config, this returns immediately.
    pub async fn run(self, shutdown: CancellationToken) {
        if !self.config.enabled {
            // Wait for cancellation so the task doesn't just vanish.
            shutdown.cancelled().await;
            return;
        }

        let mut interval = tokio::time::interval(self.config.refresh_interval);
        // The first tick completes immediately, which is fine — we want an
        // initial render right away.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    return;
                }
                _ = interval.tick() => {
                    let state = self.rx.borrow().clone();
                    let frame = render_frame(&state);
                    // Best-effort write; ignore errors (e.g. broken pipe).
                    let _ = write_frame(&frame);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Build the full dashboard frame as a `String`.
fn render_frame(state: &DashboardState) -> String {
    let mut out = String::with_capacity(2048);

    // Header
    write_header(&mut out, state);

    // Running agents section
    write_running_section(&mut out, &state.running);

    // Retry queue section
    write_retry_section(&mut out, &state.retrying);

    // Footer
    out.push_str(&colorize("╰─", ANSI_BOLD));
    out.push('\n');

    out
}

/// Write the frame string to stdout, clearing the screen first.
fn write_frame(frame: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(ANSI_HOME.as_bytes())?;
    handle.write_all(ANSI_CLEAR_SCREEN.as_bytes())?;
    handle.write_all(frame.as_bytes())?;
    handle.flush()
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn write_header(out: &mut String, state: &DashboardState) {
    out.push_str(&colorize("╭─ SYMPHONY STATUS", ANSI_BOLD));
    out.push('\n');

    // Agents line
    out.push_str(&colorize("│ Agents: ", ANSI_BOLD));
    out.push_str(&colorize(&state.running.len().to_string(), ANSI_GREEN));
    out.push_str(&colorize("/", ANSI_GRAY));
    out.push_str(&colorize(&state.max_agents.to_string(), ANSI_GRAY));
    out.push('\n');

    // Uptime
    out.push_str(&colorize("│ Uptime: ", ANSI_BOLD));
    out.push_str(&colorize(&format_duration(state.uptime), ANSI_MAGENTA));
    out.push('\n');

    // Throughput
    out.push_str(&colorize("│ Throughput: ", ANSI_BOLD));
    out.push_str(&colorize(&format!("{} tps", format_tps(state.throughput_tps)), ANSI_CYAN));
    if !state.throughput_sparkline.is_empty() {
        out.push_str(&colorize(" ", ANSI_GRAY));
        out.push_str(&colorize(&state.throughput_sparkline, ANSI_GREEN));
    }
    out.push('\n');

    // Tokens
    let tt = &state.token_totals;
    out.push_str(&colorize("│ Tokens: ", ANSI_BOLD));
    out.push_str(&colorize(&format!("in {}", format_count(tt.input_tokens)), ANSI_YELLOW));
    out.push_str(&colorize(" | ", ANSI_GRAY));
    out.push_str(&colorize(&format!("out {}", format_count(tt.output_tokens)), ANSI_YELLOW));
    out.push_str(&colorize(" | ", ANSI_GRAY));
    out.push_str(&colorize(&format!("total {}", format_count(tt.total_tokens)), ANSI_YELLOW));
    out.push('\n');

    // Rate limits
    out.push_str(&colorize("│ Rate Limits: ", ANSI_BOLD));
    out.push_str(&format_rate_limits(state.rate_limits.as_ref()));
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Running agents table
// ---------------------------------------------------------------------------

fn write_running_section(out: &mut String, running: &[RunningAgentInfo]) {
    out.push_str(&colorize("├─ Running", ANSI_BOLD));
    out.push('\n');
    out.push_str("│\n");

    // Table header
    write_running_header(out);

    if running.is_empty() {
        out.push_str("│  ");
        out.push_str(&colorize("No active agents", ANSI_GRAY));
        out.push('\n');
    } else {
        let mut sorted: Vec<&RunningAgentInfo> = running.iter().collect();
        sorted.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        for agent in sorted {
            write_running_row(out, agent);
        }
    }

    out.push_str("│\n");
}

fn write_running_header(out: &mut String) {
    let header = format!(
        "{} {} {} {} {} {}",
        format_cell("ID", COL_ID, Align::Left),
        format_cell("STATE", COL_STATE, Align::Left),
        format_cell("AGE / TURN", COL_AGE, Align::Left),
        format_cell("TOKENS", COL_TOKENS, Align::Right),
        format_cell("TURNS", COL_TURNS, Align::Right),
        format_cell("EVENT", COL_EVENT, Align::Left),
    );
    out.push_str("│   ");
    out.push_str(&colorize(header.trim_end(), ANSI_GRAY));
    out.push('\n');

    let sep_width = COL_ID + COL_STATE + COL_AGE + COL_TOKENS + COL_TURNS + COL_EVENT + 5;
    out.push_str("│   ");
    out.push_str(&colorize(&"─".repeat(sep_width), ANSI_GRAY));
    out.push('\n');
}

fn write_running_row(out: &mut String, agent: &RunningAgentInfo) {
    let event_str = agent.last_event.as_deref().unwrap_or("none");

    let status_color = match event_str {
        "none" => ANSI_RED,
        e if e.contains("token_count") => ANSI_YELLOW,
        e if e.contains("task_started") => ANSI_GREEN,
        e if e.contains("turn_completed") => ANSI_MAGENTA,
        _ => ANSI_BLUE,
    };

    let age_turns = format_age_turns(agent.elapsed, agent.turn_count);
    let event_label = summarize_event(agent.last_message.as_deref(), event_str);

    out.push_str("│ ");
    out.push_str(&colorize("●", status_color));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&agent.identifier, COL_ID, Align::Left),
        ANSI_CYAN,
    ));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&agent.state, COL_STATE, Align::Left),
        status_color,
    ));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&age_turns, COL_AGE, Align::Left),
        ANSI_MAGENTA,
    ));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&format_count(agent.total_tokens), COL_TOKENS, Align::Right),
        ANSI_YELLOW,
    ));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&agent.turn_count.to_string(), COL_TURNS, Align::Right),
        ANSI_CYAN,
    ));
    out.push(' ');
    out.push_str(&colorize(
        &format_cell(&event_label, COL_EVENT, Align::Left),
        status_color,
    ));
    out.push('\n');
}

// ---------------------------------------------------------------------------
// Retry queue section
// ---------------------------------------------------------------------------

fn write_retry_section(out: &mut String, retrying: &[RetryInfo]) {
    out.push_str(&colorize("├─ Backoff queue", ANSI_BOLD));
    out.push('\n');
    out.push_str("│\n");

    if retrying.is_empty() {
        out.push_str("│  ");
        out.push_str(&colorize("No queued retries", ANSI_GRAY));
        out.push('\n');
    } else {
        let mut sorted = retrying.to_vec();
        sorted.sort_by_key(|r| r.next_retry_in);
        for entry in &sorted {
            write_retry_row(out, entry);
        }
    }

    out.push_str("│\n");
}

fn write_retry_row(out: &mut String, entry: &RetryInfo) {
    out.push_str("│  ");
    out.push_str(&colorize("↻", ANSI_YELLOW));
    out.push(' ');
    out.push_str(&colorize(&entry.identifier, ANSI_RED));
    out.push(' ');
    out.push_str(&colorize(&format!("attempt={}", entry.attempt), ANSI_YELLOW));
    out.push_str(&colorize(" in ", ANSI_DIM));
    out.push_str(&colorize(&format_retry_duration(entry.next_retry_in), ANSI_CYAN));

    if let Some(error) = &entry.error {
        let sanitized = sanitize_error(error);
        if !sanitized.is_empty() {
            out.push(' ');
            out.push_str(&colorize(&format!("error={}", truncate(&sanitized, 96)), ANSI_DIM));
        }
    }

    out.push('\n');
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// Colorize a string with the given ANSI code.
fn colorize(text: &str, color: &str) -> String {
    let mut s = String::with_capacity(color.len() + text.len() + ANSI_RESET.len());
    s.push_str(color);
    s.push_str(text);
    s.push_str(ANSI_RESET);
    s
}

/// Format a cell value, truncating or padding to `width`.
fn format_cell(value: &str, width: usize, align: Align) -> String {
    // Sanitize: collapse whitespace, trim
    let cleaned: String = value
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    let truncated = truncate(cleaned, width);

    match align {
        Align::Left => format!("{:<width$}", truncated, width = width),
        Align::Right => format!("{:>width$}", truncated, width = width),
    }
}

/// Truncate a string to `max_len`, appending "..." if it was longer.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_owned()
    } else if max_len <= 3 {
        s.chars().take(max_len).collect()
    } else {
        let mut result: String = s.chars().take(max_len - 3).collect();
        result.push_str("...");
        result
    }
}

/// Format a token count with thousand separators (e.g. "1,234,567").
fn format_count(value: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }

    let s = value.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + (len - 1) / 3);

    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }

    result
}

/// Format a [`Duration`] as a human-readable "Xm Ys" string.
fn format_duration(d: Duration) -> String {
    let total_secs = d.as_secs();
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {mins}m {secs}s")
    } else {
        format!("{mins}m {secs}s")
    }
}

/// Format age and turn count together.
fn format_age_turns(elapsed: Duration, turn_count: u32) -> String {
    let base = format_duration(elapsed);
    if turn_count > 0 {
        format!("{base} / {turn_count}")
    } else {
        base
    }
}

/// Format retry duration as "X.XXXs".
fn format_retry_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    format!("{secs}.{millis:03}s")
}

/// Sanitize an error message for single-line display.
fn sanitize_error(error: &str) -> String {
    error
        .replace("\\r\\n", " ")
        .replace("\\r", " ")
        .replace("\\n", " ")
        .replace("\r\n", " ")
        .replace('\r', " ")
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Format a throughput value as a human-readable string with thousand separators.
fn format_tps(value: f64) -> String {
    format_count(value.trunc() as u64)
}

/// Produce a short human-readable event label for the running table.
fn summarize_event(last_message: Option<&str>, event: &str) -> String {
    if let Some(msg) = last_message {
        let cleaned = msg
            .replace('\n', " ")
            .replace('\r', " ");
        let trimmed = cleaned.trim();
        if !trimmed.is_empty() {
            return truncate(trimmed, COL_EVENT);
        }
    }

    match event {
        "none" => "waiting...".to_owned(),
        other => truncate(other, COL_EVENT),
    }
}

/// Format rate limit information from the raw JSON value.
fn format_rate_limits(info: Option<&RateLimitInfo>) -> String {
    let Some(info) = info else {
        return colorize("unavailable", ANSI_GRAY);
    };

    let Some(raw) = &info.raw else {
        return colorize("unavailable", ANSI_GRAY);
    };

    let map = match raw.as_object() {
        Some(m) => m,
        None => return colorize("unavailable", ANSI_GRAY),
    };

    let limit_id = map_str_value(map, &["limit_id", "limitId", "limit_name", "limitName"])
        .unwrap_or("unknown");

    let primary = format_rate_bucket(map_object_value(map, &["primary"]));
    let secondary = format_rate_bucket(map_object_value(map, &["secondary"]));
    let credits = format_credits(map_object_value(map, &["credits"]));

    let mut s = String::new();
    s.push_str(&colorize(limit_id, ANSI_YELLOW));
    s.push_str(&colorize(" | ", ANSI_GRAY));
    s.push_str(&colorize(&format!("primary {primary}"), ANSI_CYAN));
    s.push_str(&colorize(" | ", ANSI_GRAY));
    s.push_str(&colorize(&format!("secondary {secondary}"), ANSI_CYAN));
    s.push_str(&colorize(" | ", ANSI_GRAY));
    s.push_str(&colorize(&credits, ANSI_GREEN));
    s
}

fn format_rate_bucket(bucket: Option<&serde_json::Map<String, Value>>) -> String {
    let Some(bucket) = bucket else {
        return "n/a".to_owned();
    };

    let remaining = map_u64_value(bucket, &["remaining"]);
    let limit = map_u64_value(bucket, &["limit"]);

    match (remaining, limit) {
        (Some(r), Some(l)) => format!("{}/{}", format_count(r), format_count(l)),
        (Some(r), None) => format!("remaining {}", format_count(r)),
        (None, Some(l)) => format!("limit {}", format_count(l)),
        (None, None) => "n/a".to_owned(),
    }
}

fn format_credits(credits: Option<&serde_json::Map<String, Value>>) -> String {
    let Some(credits) = credits else {
        return "credits n/a".to_owned();
    };

    let unlimited = credits
        .get("unlimited")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_credits = credits
        .get("has_credits")
        .or_else(|| credits.get("hasCredits"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let balance = credits
        .get("balance")
        .and_then(Value::as_f64);

    if unlimited {
        "credits unlimited".to_owned()
    } else if has_credits {
        match balance {
            Some(b) => format!("credits {:.2}", b),
            None => "credits available".to_owned(),
        }
    } else {
        "credits none".to_owned()
    }
}

// ---------------------------------------------------------------------------
// JSON map helpers
// ---------------------------------------------------------------------------

fn map_str_value<'a>(
    map: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter().find_map(|k| map.get(*k).and_then(Value::as_str))
}

fn map_u64_value(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|k| map.get(*k).and_then(Value::as_u64))
}

fn map_object_value<'a>(
    map: &'a serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    keys.iter().find_map(|k| map.get(*k).and_then(Value::as_object))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_count_adds_commas() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(1_000_000_000), "1,000,000,000");
    }

    #[test]
    fn format_duration_produces_readable_output() {
        assert_eq!(format_duration(Duration::from_secs(0)), "0m 0s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1m 5s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
    }

    #[test]
    fn truncate_handles_short_strings() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_adds_ellipsis_for_long_strings() {
        assert_eq!(truncate("hello world", 8), "hello...");
        assert_eq!(truncate("abcdefghij", 6), "abc...");
    }

    #[test]
    fn format_cell_pads_left() {
        let cell = format_cell("hi", 6, Align::Left);
        assert_eq!(cell, "hi    ");
    }

    #[test]
    fn format_cell_pads_right() {
        let cell = format_cell("42", 6, Align::Right);
        assert_eq!(cell, "    42");
    }

    #[test]
    fn format_cell_truncates() {
        let cell = format_cell("abcdefghij", 6, Align::Left);
        assert_eq!(cell, "abc...");
    }

    #[test]
    fn format_retry_duration_formats_correctly() {
        assert_eq!(
            format_retry_duration(Duration::from_millis(3_500)),
            "3.500s"
        );
        assert_eq!(
            format_retry_duration(Duration::from_millis(100)),
            "0.100s"
        );
    }

    #[test]
    fn sanitize_error_collapses_whitespace() {
        assert_eq!(
            sanitize_error("line1\nline2\r\nline3"),
            "line1 line2 line3"
        );
    }

    #[test]
    fn render_frame_empty_state() {
        let state = DashboardState::default();
        let frame = render_frame(&state);
        // Should contain key sections
        assert!(frame.contains("SYMPHONY STATUS"));
        assert!(frame.contains("Throughput"));
        assert!(frame.contains("tps"));
        assert!(frame.contains("Running"));
        assert!(frame.contains("No active agents"));
        assert!(frame.contains("Backoff queue"));
        assert!(frame.contains("No queued retries"));
    }

    #[test]
    fn render_frame_with_running_agents() {
        let state = DashboardState {
            running: vec![
                RunningAgentInfo {
                    identifier: "ABC-123".to_owned(),
                    title: "Fix bug".to_owned(),
                    state: "In Progress".to_owned(),
                    elapsed: Duration::from_secs(125),
                    total_tokens: 15_000,
                    turn_count: 3,
                    last_event: Some("codex/event/task_started".to_owned()),
                    ..Default::default()
                },
                RunningAgentInfo {
                    identifier: "ABC-456".to_owned(),
                    title: "Add feature".to_owned(),
                    state: "Todo".to_owned(),
                    elapsed: Duration::from_secs(30),
                    total_tokens: 2_500,
                    turn_count: 1,
                    last_event: Some("codex/event/token_count".to_owned()),
                    ..Default::default()
                },
            ],
            retrying: vec![RetryInfo {
                identifier: "DEF-789".to_owned(),
                next_retry_in: Duration::from_millis(5_500),
                attempt: 2,
                error: Some("rate limited".to_owned()),
            }],
            token_totals: TokenTotals {
                input_tokens: 10_000,
                output_tokens: 5_000,
                total_tokens: 15_000,
            },
            rate_limits: None,
            uptime: Duration::from_secs(300),
            max_agents: 10,
            throughput_tps: 1_234.5,
            throughput_sparkline: "▁▂▃▄▅▆▇█".to_owned(),
        };

        let frame = render_frame(&state);

        // Verify structure
        assert!(frame.contains("SYMPHONY STATUS"));
        assert!(frame.contains("Throughput"));
        assert!(frame.contains("1,234 tps"));
        assert!(frame.contains("▁▂▃▄▅▆▇█"));
        assert!(frame.contains("2"));  // agent count
        assert!(frame.contains("5m 0s")); // uptime
        assert!(frame.contains("15,000")); // total tokens
        assert!(frame.contains("ABC-123"));
        assert!(frame.contains("ABC-456"));
        assert!(frame.contains("DEF-789"));
        assert!(frame.contains("attempt=2"));
        assert!(frame.contains("5.500s"));
        assert!(frame.contains("rate limited"));
    }

    #[test]
    fn colorize_wraps_correctly() {
        let result = colorize("hello", ANSI_RED);
        assert_eq!(result, format!("{ANSI_RED}hello{ANSI_RESET}"));
    }

    #[test]
    fn format_age_turns_without_turns() {
        assert_eq!(format_age_turns(Duration::from_secs(65), 0), "1m 5s");
    }

    #[test]
    fn format_age_turns_with_turns() {
        assert_eq!(
            format_age_turns(Duration::from_secs(65), 3),
            "1m 5s / 3"
        );
    }

    #[test]
    fn format_rate_limits_none() {
        let result = format_rate_limits(None);
        assert!(result.contains("unavailable"));
    }

    #[test]
    fn format_rate_limits_with_data() {
        let raw = serde_json::json!({
            "limit_id": "tier-5",
            "primary": {
                "remaining": 450,
                "limit": 500
            },
            "secondary": {
                "remaining": 90000,
                "limit": 100000
            },
            "credits": {
                "unlimited": true
            }
        });

        let info = RateLimitInfo {
            raw: Some(raw),
        };

        let result = format_rate_limits(Some(&info));
        assert!(result.contains("tier-5"));
        assert!(result.contains("450/500"));
        assert!(result.contains("90,000/100,000"));
        assert!(result.contains("credits unlimited"));
    }

    #[test]
    fn summarize_event_uses_message_when_available() {
        let result = summarize_event(Some("running tests"), "some_event");
        assert_eq!(result, "running tests");
    }

    #[test]
    fn summarize_event_falls_back_to_event_name() {
        let result = summarize_event(None, "codex/event/task_started");
        assert_eq!(result, "codex/event/task_started");
    }

    #[test]
    fn summarize_event_shows_waiting_for_none() {
        let result = summarize_event(None, "none");
        assert_eq!(result, "waiting...");
    }

    #[test]
    fn sparkline_all_zeros() {
        let data = vec![0.0; 8];
        let result = sparkline(&data, 8);
        assert_eq!(result, "▁▁▁▁▁▁▁▁");
    }

    #[test]
    fn sparkline_ascending() {
        let data: Vec<f64> = (0..8).map(|i| i as f64).collect();
        let result = sparkline(&data, 8);
        // 0/7=0 -> ▁, 1/7≈0.14 -> ▁, 2/7≈0.29 -> ▂, 3/7≈0.43 -> ▃,
        // 4/7≈0.57 -> ▅, 5/7≈0.71 -> ▅, 6/7≈0.86 -> ▇, 7/7=1.0 -> █
        assert_eq!(result.chars().count(), 8);
        // First char should be lowest block, last should be highest
        assert_eq!(result.chars().next().unwrap(), '▁');
        assert_eq!(result.chars().last().unwrap(), '█');
    }

    #[test]
    fn sparkline_single_peak() {
        let data = vec![0.0, 0.0, 0.0, 10.0, 0.0, 0.0];
        let result = sparkline(&data, 6);
        assert_eq!(result.chars().count(), 6);
        // The peak should be the full block
        let chars: Vec<char> = result.chars().collect();
        assert_eq!(chars[3], '█');
        assert_eq!(chars[0], '▁');
    }

    #[test]
    fn sparkline_width_larger_than_data() {
        let data = vec![1.0, 2.0];
        let result = sparkline(&data, 5);
        assert_eq!(result.chars().count(), 5);
        // Extra positions should be ▁
        let chars: Vec<char> = result.chars().collect();
        assert_eq!(chars[2], '▁');
        assert_eq!(chars[4], '▁');
    }

    #[test]
    fn sparkline_empty_data() {
        let data: Vec<f64> = vec![];
        let result = sparkline(&data, 4);
        assert_eq!(result, "▁▁▁▁");
    }

    #[test]
    fn throughput_tracker_empty() {
        let mut tracker = ThroughputTracker::new();
        assert_eq!(tracker.tokens_per_second(), 0.0);
    }

    #[test]
    fn throughput_tracker_single_sample() {
        let mut tracker = ThroughputTracker::new();
        tracker.record(100);
        // Single sample -> 0 tps (need at least 2 to compute a rate)
        assert_eq!(tracker.tokens_per_second(), 0.0);
    }

    #[test]
    fn throughput_tracker_rolling_tps_computes_correctly() {
        let mut tracker = ThroughputTracker::new();
        // Use a fixed "now" well ahead of epoch so samples are within the window.
        let now_ms = 10_000;
        tracker.samples.push_back((now_ms - 1_000, 0));
        tracker.samples.push_back((now_ms, 1_000));
        let tps = tracker.rolling_tps(now_ms);
        // 1000 tokens in 1 second = 1000 tps
        assert!(
            (tps - 1_000.0).abs() < 1.0,
            "expected ~1000, got {}",
            tps
        );
    }

    #[test]
    fn throughput_tracker_rolling_tps_multiple_samples() {
        let mut tracker = ThroughputTracker::new();
        let now_ms = 10_000;
        // 3 samples over 2 seconds: 0 -> 500 -> 2000
        tracker.samples.push_back((now_ms - 2_000, 0));
        tracker.samples.push_back((now_ms - 1_000, 500));
        tracker.samples.push_back((now_ms, 2_000));
        let tps = tracker.rolling_tps(now_ms);
        // oldest sample is (now-2000, 0), delta = 2000 tokens in 2 seconds = 1000 tps
        assert!(
            (tps - 1_000.0).abs() < 1.0,
            "expected ~1000, got {}",
            tps
        );
    }

    #[test]
    fn throughput_tracker_sparkline_empty() {
        let tracker = ThroughputTracker::new();
        let graph = tracker.sparkline_graph();
        assert_eq!(graph.chars().count(), THROUGHPUT_GRAPH_COLUMNS);
        // All blocks should be the minimum
        assert!(graph.chars().all(|c| c == '▁'));
    }

    #[test]
    fn format_tps_formats_with_commas() {
        assert_eq!(format_tps(0.0), "0");
        assert_eq!(format_tps(999.9), "999");
        assert_eq!(format_tps(1_234.5), "1,234");
        assert_eq!(format_tps(1_000_000.0), "1,000,000");
    }
}
