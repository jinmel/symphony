//! Embedded web dashboard for Symphony observability.
//!
//! Serves a single-page HTML dashboard at `GET /` that polls the
//! `/api/v1/state` endpoint every 2 seconds and renders orchestrator
//! state in the browser -- the Rust equivalent of the Elixir Phoenix
//! LiveView dashboard.

use axum::http::header;
use axum::response::{Html, IntoResponse};

/// Handler for `GET /` -- serves the embedded dashboard HTML.
pub async fn handle_dashboard() -> impl IntoResponse {
    Html(DASHBOARD_HTML)
}

/// Handler for `GET /dashboard.css` -- serves the embedded stylesheet.
pub async fn handle_dashboard_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], DASHBOARD_CSS)
}

// ---------------------------------------------------------------------------
// Embedded CSS
// ---------------------------------------------------------------------------

const DASHBOARD_CSS: &str = r##"
:root {
  color-scheme: dark;
  --page: #0d1117;
  --page-soft: #161b22;
  --page-deep: #010409;
  --card: rgba(22, 27, 34, 0.94);
  --card-muted: #1c2128;
  --ink: #e6edf3;
  --muted: #8b949e;
  --line: #30363d;
  --line-strong: #484f58;
  --accent: #10a37f;
  --accent-ink: #7ee2b8;
  --accent-soft: rgba(16, 163, 127, 0.12);
  --danger: #f85149;
  --danger-soft: rgba(248, 81, 73, 0.1);
  --warning: #d29922;
  --warning-soft: rgba(210, 153, 34, 0.1);
  --shadow-sm: 0 1px 2px rgba(0, 0, 0, 0.3);
  --shadow-lg: 0 20px 50px rgba(0, 0, 0, 0.4);
}

* {
  box-sizing: border-box;
}

html {
  background: var(--page);
}

body {
  margin: 0;
  min-height: 100vh;
  background:
    radial-gradient(circle at top, rgba(16, 163, 127, 0.08) 0%, rgba(16, 163, 127, 0) 30%),
    linear-gradient(180deg, var(--page-soft) 0%, var(--page) 24%, var(--page-deep) 100%);
  color: var(--ink);
  font-family: "SF Mono", "SFMono-Regular", "Cascadia Code", "Fira Code", Consolas, "Liberation Mono", monospace;
  line-height: 1.5;
  -webkit-font-smoothing: antialiased;
}

a {
  color: var(--accent-ink);
  text-decoration: none;
  transition: color 140ms ease;
}

a:hover {
  color: var(--accent);
}

pre {
  margin: 0;
  white-space: pre-wrap;
  word-break: break-word;
}

.numeric {
  font-variant-numeric: tabular-nums slashed-zero;
  font-feature-settings: "tnum" 1, "zero" 1;
}

.app-shell {
  max-width: 1280px;
  margin: 0 auto;
  padding: 2rem 1rem 3.5rem;
}

.dashboard-shell {
  display: grid;
  gap: 1rem;
}

.hero-card,
.section-card,
.metric-card,
.error-card {
  background: var(--card);
  border: 1px solid var(--line);
  box-shadow: var(--shadow-sm);
  backdrop-filter: blur(18px);
}

.hero-card {
  border-radius: 16px;
  padding: clamp(1.25rem, 3vw, 2rem);
  box-shadow: var(--shadow-lg);
}

.hero-grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto;
  gap: 1.25rem;
  align-items: start;
}

.eyebrow {
  margin: 0;
  color: var(--accent-ink);
  text-transform: uppercase;
  letter-spacing: 0.08em;
  font-size: 0.72rem;
  font-weight: 600;
}

.hero-title {
  margin: 0.35rem 0 0;
  font-size: clamp(1.6rem, 4vw, 2.4rem);
  line-height: 0.98;
  letter-spacing: -0.04em;
  color: var(--ink);
}

.hero-copy {
  margin: 0.75rem 0 0;
  max-width: 46rem;
  color: var(--muted);
  font-size: 0.85rem;
}

.status-stack {
  display: grid;
  justify-items: end;
  align-content: start;
  min-width: min(100%, 9rem);
}

.status-badge {
  display: inline-flex;
  align-items: center;
  gap: 0.45rem;
  min-height: 2rem;
  padding: 0.35rem 0.78rem;
  border-radius: 999px;
  border: 1px solid var(--line);
  background: var(--card-muted);
  color: var(--muted);
  font-size: 0.78rem;
  font-weight: 700;
  letter-spacing: 0.01em;
}

.status-badge-dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 999px;
  background: currentColor;
  opacity: 0.9;
}

.status-badge-live {
  background: var(--accent-soft);
  border-color: rgba(16, 163, 127, 0.3);
  color: var(--accent-ink);
}

.status-badge-live .status-badge-dot {
  animation: pulse 2s ease-in-out infinite;
}

@keyframes pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: 0.4; }
}

.status-badge-offline {
  background: var(--card-muted);
  border-color: var(--line-strong);
  color: var(--muted);
}

.metric-grid {
  display: grid;
  gap: 0.85rem;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
}

.metric-card {
  border-radius: 12px;
  padding: 1rem 1.05rem 1.1rem;
}

.metric-label {
  margin: 0;
  color: var(--muted);
  font-size: 0.72rem;
  font-weight: 600;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}

.metric-value {
  margin: 0.35rem 0 0;
  font-size: clamp(1.4rem, 2vw, 1.8rem);
  line-height: 1.05;
  letter-spacing: -0.03em;
  color: var(--accent-ink);
}

.metric-detail {
  margin: 0.45rem 0 0;
  color: var(--muted);
  font-size: 0.78rem;
}

.section-card {
  border-radius: 12px;
  padding: 1.15rem;
}

.section-header {
  display: flex;
  justify-content: space-between;
  align-items: flex-start;
  gap: 1rem;
  flex-wrap: wrap;
}

.section-title {
  margin: 0;
  font-size: 1rem;
  line-height: 1.2;
  letter-spacing: -0.02em;
  color: var(--ink);
}

.section-copy {
  margin: 0.35rem 0 0;
  color: var(--muted);
  font-size: 0.82rem;
}

.table-wrap {
  overflow-x: auto;
  margin-top: 1rem;
}

.data-table {
  width: 100%;
  min-width: 720px;
  border-collapse: collapse;
}

.data-table-running {
  table-layout: fixed;
  min-width: 900px;
}

.data-table th {
  padding: 0 0.5rem 0.75rem 0;
  text-align: left;
  color: var(--muted);
  font-size: 0.68rem;
  font-weight: 600;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}

.data-table td {
  padding: 0.75rem 0.5rem 0.75rem 0;
  border-top: 1px solid var(--line);
  vertical-align: top;
  font-size: 0.82rem;
}

.issue-stack,
.detail-stack,
.token-stack {
  display: grid;
  gap: 0.2rem;
  min-width: 0;
}

.event-text {
  font-weight: 500;
  line-height: 1.45;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--ink);
}

.event-meta {
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--muted);
  font-size: 0.76rem;
}

.state-badge {
  display: inline-flex;
  align-items: center;
  min-height: 1.7rem;
  padding: 0.25rem 0.6rem;
  border-radius: 999px;
  border: 1px solid var(--line);
  background: var(--card-muted);
  color: var(--ink);
  font-size: 0.72rem;
  font-weight: 600;
  line-height: 1;
}

.state-badge-active {
  background: var(--accent-soft);
  border-color: rgba(16, 163, 127, 0.3);
  color: var(--accent-ink);
}

.state-badge-warning {
  background: var(--warning-soft);
  border-color: rgba(210, 153, 34, 0.3);
  color: var(--warning);
}

.state-badge-danger {
  background: var(--danger-soft);
  border-color: rgba(248, 81, 73, 0.3);
  color: var(--danger);
}

.issue-id {
  font-weight: 600;
  letter-spacing: -0.01em;
  color: var(--ink);
}

.issue-link {
  color: var(--muted);
  font-size: 0.76rem;
}

.issue-link:hover {
  color: var(--accent-ink);
}

.muted {
  color: var(--muted);
}

.code-panel {
  margin-top: 1rem;
  padding: 1rem;
  border-radius: 8px;
  background: var(--page-deep);
  border: 1px solid var(--line);
  color: var(--muted);
  font-size: 0.8rem;
  max-height: 300px;
  overflow-y: auto;
}

.empty-state {
  margin: 1rem 0 0;
  color: var(--muted);
  font-style: italic;
}

.error-card {
  border-radius: 12px;
  padding: 1.25rem;
  background: var(--danger-soft);
  border-color: rgba(248, 81, 73, 0.3);
}

.error-title {
  margin: 0;
  color: var(--danger);
  font-size: 1rem;
  letter-spacing: -0.02em;
}

.error-copy {
  margin: 0.45rem 0 0;
  color: var(--danger);
  font-size: 0.85rem;
}

.subtle-button {
  appearance: none;
  border: 1px solid var(--line-strong);
  background: var(--card-muted);
  color: var(--muted);
  border-radius: 999px;
  padding: 0.25rem 0.6rem;
  cursor: pointer;
  font: inherit;
  font-size: 0.72rem;
  font-weight: 600;
  letter-spacing: 0.01em;
  transition: background 140ms ease, border-color 140ms ease, color 140ms ease;
}

.subtle-button:hover {
  background: var(--line);
  border-color: var(--muted);
  color: var(--ink);
}

.refresh-indicator {
  display: inline-block;
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--accent);
  margin-left: 0.5rem;
  opacity: 0;
  transition: opacity 200ms ease;
}

.refresh-indicator.active {
  opacity: 1;
}

@media (max-width: 860px) {
  .app-shell {
    padding: 1rem 0.85rem 2rem;
  }

  .hero-grid {
    grid-template-columns: 1fr;
  }

  .status-stack {
    justify-items: start;
  }

  .metric-grid {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
}

@media (max-width: 560px) {
  .metric-grid {
    grid-template-columns: 1fr;
  }

  .section-card,
  .hero-card,
  .error-card {
    border-radius: 8px;
    padding: 1rem;
  }
}
"##;

// ---------------------------------------------------------------------------
// Embedded HTML
// ---------------------------------------------------------------------------

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Symphony Dashboard</title>
  <link rel="stylesheet" href="/dashboard.css" />
</head>
<body>
  <div class="app-shell">
    <section class="dashboard-shell" id="dashboard">
      <header class="hero-card">
        <div class="hero-grid">
          <div>
            <p class="eyebrow">Symphony Observability</p>
            <h1 class="hero-title">Operations Dashboard</h1>
            <p class="hero-copy">
              Current state, retry pressure, token usage, and orchestration health for the active Symphony runtime.
            </p>
          </div>
          <div class="status-stack">
            <span class="status-badge status-badge-live" id="status-live" style="display:none;">
              <span class="status-badge-dot"></span>
              Live
              <span class="refresh-indicator" id="refresh-dot"></span>
            </span>
            <span class="status-badge status-badge-offline" id="status-offline">
              <span class="status-badge-dot"></span>
              Connecting&hellip;
            </span>
          </div>
        </div>
      </header>

      <div id="error-container"></div>

      <section class="metric-grid" id="metrics">
        <article class="metric-card">
          <p class="metric-label">Running</p>
          <p class="metric-value numeric" id="m-running">--</p>
          <p class="metric-detail">Active issue sessions in the current runtime.</p>
        </article>
        <article class="metric-card">
          <p class="metric-label">Retrying</p>
          <p class="metric-value numeric" id="m-retrying">--</p>
          <p class="metric-detail">Issues waiting for the next retry window.</p>
        </article>
        <article class="metric-card">
          <p class="metric-label">Total Tokens</p>
          <p class="metric-value numeric" id="m-total-tokens">--</p>
          <p class="metric-detail numeric" id="m-token-breakdown">In -- / Out --</p>
        </article>
        <article class="metric-card">
          <p class="metric-label">Runtime</p>
          <p class="metric-value numeric" id="m-runtime">--</p>
          <p class="metric-detail">Total elapsed runtime across active sessions.</p>
        </article>
      </section>

      <section class="section-card">
        <div class="section-header">
          <div>
            <h2 class="section-title">Rate Limits</h2>
            <p class="section-copy">Latest upstream rate-limit snapshot, when available.</p>
          </div>
        </div>
        <pre class="code-panel" id="rate-limits">Waiting for data&hellip;</pre>
      </section>

      <section class="section-card">
        <div class="section-header">
          <div>
            <h2 class="section-title">Running Sessions</h2>
            <p class="section-copy">Active issues, last known agent activity, and token usage.</p>
          </div>
        </div>
        <div id="running-container">
          <p class="empty-state">Waiting for data&hellip;</p>
        </div>
      </section>

      <section class="section-card">
        <div class="section-header">
          <div>
            <h2 class="section-title">Retry Queue</h2>
            <p class="section-copy">Issues waiting for the next retry window.</p>
          </div>
        </div>
        <div id="retry-container">
          <p class="empty-state">Waiting for data&hellip;</p>
        </div>
      </section>
    </section>
  </div>

<script>
(function() {
  "use strict";

  // --- Helpers ---

  function formatInt(n) {
    if (n == null || isNaN(n)) return "n/a";
    return Number(n).toLocaleString("en-US");
  }

  function formatRuntimeSeconds(seconds) {
    if (seconds == null || isNaN(seconds)) return "--";
    var s = Math.max(Math.floor(seconds), 0);
    var m = Math.floor(s / 60);
    s = s % 60;
    return m + "m " + s + "s";
  }

  function elapsedSeconds(isoString) {
    if (!isoString) return 0;
    var then = new Date(isoString).getTime();
    if (isNaN(then)) return 0;
    return Math.max(0, (Date.now() - then) / 1000);
  }

  function escapeHtml(str) {
    if (!str) return "";
    var d = document.createElement("div");
    d.appendChild(document.createTextNode(str));
    return d.innerHTML;
  }

  function stateBadgeClass(state) {
    if (!state) return "state-badge";
    var s = state.toLowerCase();
    if (/progress|running|active/.test(s)) return "state-badge state-badge-active";
    if (/blocked|error|failed/.test(s)) return "state-badge state-badge-danger";
    if (/todo|queued|pending|retry/.test(s)) return "state-badge state-badge-warning";
    return "state-badge";
  }

  function prettyJson(obj) {
    if (obj == null) return "n/a";
    try { return JSON.stringify(obj, null, 2); }
    catch(e) { return String(obj); }
  }

  // --- DOM references ---

  var mRunning = document.getElementById("m-running");
  var mRetrying = document.getElementById("m-retrying");
  var mTotalTokens = document.getElementById("m-total-tokens");
  var mTokenBreakdown = document.getElementById("m-token-breakdown");
  var mRuntime = document.getElementById("m-runtime");
  var rateLimitsEl = document.getElementById("rate-limits");
  var runningContainer = document.getElementById("running-container");
  var retryContainer = document.getElementById("retry-container");
  var statusLive = document.getElementById("status-live");
  var statusOffline = document.getElementById("status-offline");
  var refreshDot = document.getElementById("refresh-dot");
  var errorContainer = document.getElementById("error-container");

  var lastData = null;
  var connected = false;

  // --- Rendering ---

  function setConnected(ok) {
    connected = ok;
    statusLive.style.display = ok ? "inline-flex" : "none";
    statusOffline.style.display = ok ? "none" : "inline-flex";
    statusOffline.querySelector(".status-badge-dot + span") || null;
    if (!ok) {
      var textNode = statusOffline.childNodes;
      // Update text if disconnected
      for (var i = 0; i < textNode.length; i++) {
        if (textNode[i].nodeType === 3 && textNode[i].textContent.trim()) {
          // skip
        }
      }
    }
  }

  function flashRefresh() {
    refreshDot.classList.add("active");
    setTimeout(function() { refreshDot.classList.remove("active"); }, 400);
  }

  function totalRuntimeSeconds(data) {
    var completed = 0; // The API doesn't expose completed runtime yet
    var active = 0;
    if (data.running) {
      for (var i = 0; i < data.running.length; i++) {
        active += elapsedSeconds(data.running[i].started_at);
      }
    }
    return completed + active;
  }

  function renderMetrics(data) {
    var counts = data.counts || {};
    var totals = data.codex_totals || {};

    mRunning.textContent = formatInt(counts.running);
    mRetrying.textContent = formatInt(counts.retrying);
    mTotalTokens.textContent = formatInt(totals.total_tokens);
    mTokenBreakdown.textContent = "In " + formatInt(totals.input_tokens) + " / Out " + formatInt(totals.output_tokens);
    mRuntime.textContent = formatRuntimeSeconds(totalRuntimeSeconds(data));
  }

  function renderRateLimits(data) {
    rateLimitsEl.textContent = prettyJson(data.rate_limits);
  }

  function renderRunning(data) {
    var entries = data.running || [];
    if (entries.length === 0) {
      runningContainer.innerHTML = '<p class="empty-state">No active sessions.</p>';
      return;
    }

    var html = '<div class="table-wrap"><table class="data-table data-table-running">';
    html += "<colgroup>";
    html += '<col style="width:11rem" />';
    html += '<col style="width:8rem" />';
    html += '<col style="width:7rem" />';
    html += '<col />';
    html += '<col style="width:10rem" />';
    html += "</colgroup>";
    html += "<thead><tr>";
    html += "<th>Issue</th><th>State</th><th>Runtime / Turns</th><th>Last Event</th><th>Tokens</th>";
    html += "</tr></thead><tbody>";

    for (var i = 0; i < entries.length; i++) {
      var e = entries[i];
      var elapsed = elapsedSeconds(e.started_at);
      var runtimeStr = formatRuntimeSeconds(elapsed);
      if (e.turn_count > 0) runtimeStr += " / " + e.turn_count;

      var tokens = e.tokens || {};
      var eventText = escapeHtml(e.last_message || e.last_event || "n/a");
      var eventMeta = escapeHtml(e.last_event || "n/a");
      if (e.last_event_at) eventMeta += ' &middot; <span class="numeric">' + escapeHtml(e.last_event_at) + "</span>";

      html += "<tr>";
      html += '<td><div class="issue-stack">';
      html += '<span class="issue-id">' + escapeHtml(e.issue_identifier) + "</span>";
      html += '<a class="issue-link" href="/api/v1/' + encodeURIComponent(e.issue_identifier) + '">JSON</a>';
      html += "</div></td>";
      html += '<td><span class="' + stateBadgeClass(e.state) + '">' + escapeHtml(e.state) + "</span></td>";
      html += '<td class="numeric">' + runtimeStr + "</td>";
      html += '<td><div class="detail-stack">';
      html += '<span class="event-text">' + eventText + "</span>";
      html += '<span class="muted event-meta">' + eventMeta + "</span>";
      html += "</div></td>";
      html += '<td><div class="token-stack numeric">';
      html += "<span>Total: " + formatInt(tokens.total_tokens) + "</span>";
      html += '<span class="muted">In ' + formatInt(tokens.input_tokens) + " / Out " + formatInt(tokens.output_tokens) + "</span>";
      html += "</div></td>";
      html += "</tr>";
    }

    html += "</tbody></table></div>";
    runningContainer.innerHTML = html;
  }

  function renderRetrying(data) {
    var entries = data.retrying || [];
    if (entries.length === 0) {
      retryContainer.innerHTML = '<p class="empty-state">No issues are currently backing off.</p>';
      return;
    }

    var html = '<div class="table-wrap"><table class="data-table" style="min-width:680px">';
    html += "<thead><tr>";
    html += "<th>Issue</th><th>Attempt</th><th>Due At</th><th>Error</th>";
    html += "</tr></thead><tbody>";

    for (var i = 0; i < entries.length; i++) {
      var e = entries[i];
      html += "<tr>";
      html += '<td><div class="issue-stack">';
      html += '<span class="issue-id">' + escapeHtml(e.issue_identifier) + "</span>";
      html += '<a class="issue-link" href="/api/v1/' + encodeURIComponent(e.issue_identifier) + '">JSON</a>';
      html += "</div></td>";
      html += "<td>" + (e.attempt != null ? e.attempt : "n/a") + "</td>";
      html += '<td class="numeric">' + escapeHtml(e.due_at || "n/a") + "</td>";
      html += "<td>" + escapeHtml(e.error || "n/a") + "</td>";
      html += "</tr>";
    }

    html += "</tbody></table></div>";
    retryContainer.innerHTML = html;
  }

  function renderError(msg) {
    errorContainer.innerHTML =
      '<section class="error-card">' +
      '<h2 class="error-title">Snapshot unavailable</h2>' +
      '<p class="error-copy">' + escapeHtml(msg) + "</p>" +
      "</section>";
  }

  function clearError() {
    errorContainer.innerHTML = "";
  }

  function render(data) {
    lastData = data;
    clearError();
    renderMetrics(data);
    renderRateLimits(data);
    renderRunning(data);
    renderRetrying(data);
  }

  // --- Polling ---

  function poll() {
    fetch("/api/v1/state")
      .then(function(res) {
        if (!res.ok) throw new Error("HTTP " + res.status);
        return res.json();
      })
      .then(function(data) {
        if (!connected) setConnected(true);
        flashRefresh();
        render(data);
      })
      .catch(function(err) {
        setConnected(false);
        renderError("Could not reach /api/v1/state: " + err.message);
      });
  }

  // --- Runtime timer (updates runtime metric every second without a fetch) ---

  function tickRuntime() {
    if (lastData) {
      mRuntime.textContent = formatRuntimeSeconds(totalRuntimeSeconds(lastData));
    }
  }

  // Initial fetch
  poll();

  // Poll every 2 seconds
  setInterval(poll, 2000);

  // Update runtime display every second (between polls)
  setInterval(tickRuntime, 1000);
})();
</script>
</body>
</html>
"##;
