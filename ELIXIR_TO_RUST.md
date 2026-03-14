# Symphony: Elixir to Rust Feature Mapping

Symphony is an orchestration system that polls Linear for candidate issues, creates isolated per-issue workspaces, launches Codex agents in app-server mode, and manages multi-turn agent sessions with retry, concurrency control, and observability.

Both implementations are feature-complete and functionally equivalent. The Elixir version is built on OTP (GenServer supervision, Phoenix LiveView). The Rust version uses Tokio async tasks, Axum for HTTP, and an embedded HTML dashboard.

## Feature Parity Table

| Feature | Description | Elixir | Rust |
|---------|-------------|--------|------|
| **Orchestrator** | Main polling loop: dispatches issues to agents, manages concurrency limits, retries with exponential backoff, terminal-state cleanup, token accounting | `lib/symphony_elixir/orchestrator.ex` | `src/orchestrator.rs` |
| **Agent / Codex Session** | JSON-RPC communication with Codex app-server over stdio; executes turns, handles approval requests, tracks token usage | `lib/symphony_elixir/codex/app_server.ex`, `lib/symphony_elixir/agent_runner.ex` | `src/agent/mod.rs` |
| **Dynamic Tool (linear_graphql)** | Allows agents to execute raw GraphQL queries/mutations against Linear during turns | `lib/symphony_elixir/codex/dynamic_tool.ex` | `src/agent/mod.rs` |
| **Linear Tracker Client** | GraphQL queries for polling issues (paginated), fetching by ID/state; mutations for comments and state updates | `lib/symphony_elixir/linear/client.ex` | `src/tracker/linear.rs` |
| **Linear Adapter** | Implements Tracker behaviour/trait, delegates to the GraphQL client | `lib/symphony_elixir/linear/adapter.ex` | `src/tracker/linear.rs` |
| **Tracker Abstraction** | Pluggable interface (`behaviour` / `trait`) for tracker backends with async methods | `lib/symphony_elixir/tracker.ex` | `src/tracker/mod.rs` |
| **In-Memory Tracker** | Test adapter that stores issues in memory and logs mutation events | `lib/symphony_elixir/tracker/memory.ex` | `src/tracker/memory.rs` |
| **Issue Model** | Normalized issue struct: id, identifier, title, description, priority, state, labels, blockers, assignee, timestamps | `lib/symphony_elixir/linear/issue.ex` | `src/issue.rs` |
| **Config Parsing & Schema** | Loads YAML front-matter from WORKFLOW.md into typed config; validates all sections with defaults | `lib/symphony_elixir/config.ex`, `lib/symphony_elixir/config/schema.ex` | `src/config.rs` |
| **Workflow Store** | Hot-reloading store that polls WORKFLOW.md every 1s for changes using fingerprinting; keeps last-known-good config on parse errors | `lib/symphony_elixir/workflow_store.ex` | `src/workflow.rs` |
| **Workflow Parsing** | Splits WORKFLOW.md into YAML front-matter (config) and Markdown body (prompt template) | `lib/symphony_elixir/workflow.ex` | `src/workflow.rs` |
| **Prompt Builder** | Renders prompt templates with issue context and attempt number; produces continuation prompts for multi-turn | `lib/symphony_elixir/prompt_builder.ex` | `src/prompt.rs` |
| **Workspace Management** | Creates/cleans up isolated per-issue directories; runs lifecycle hooks (after_create, before_run, after_run, before_remove) | `lib/symphony_elixir/workspace.ex` | `src/workspace.rs` |
| **Remote Workspace (SSH)** | Creates and removes workspaces on remote worker hosts via SSH; runs hooks remotely | `lib/symphony_elixir/workspace.ex` | `src/workspace.rs` |
| **Batch Workspace Removal** | Removes workspaces for an issue across all configured worker hosts (local + remote) | `lib/symphony_elixir/workspace.ex` | `src/workspace.rs` |
| **SSH Module** | Host:port parsing (including IPv6 bracket notation), shell escaping, remote command execution with timeout | `lib/symphony_elixir/ssh.ex` | `src/ssh.rs` |
| **Worker Distribution** | Configurable SSH worker hosts with per-host concurrency limits and round-robin selection | `lib/symphony_elixir/agent_runner.ex` | `src/worker.rs` |
| **HTTP Server** | Observability HTTP server with configurable host and port | `lib/symphony_elixir/http_server.ex`, `lib/symphony_elixir_web/endpoint.ex` | `src/server.rs` |
| **Observability API** | JSON endpoints: `GET /api/v1/state`, `POST /api/v1/refresh`, `GET /api/v1/:issue_identifier` | `lib/symphony_elixir_web/controllers/observability_api_controller.ex` | `src/server.rs` |
| **API Response Presenter** | Formats orchestrator state into JSON response payloads | `lib/symphony_elixir_web/presenter.ex` | `src/server.rs` |
| **Web Dashboard (HTML)** | Browser-based dashboard with real-time updates showing running agents, retry queue, and token metrics | `lib/symphony_elixir_web/live/dashboard_live.ex` | `src/web_dashboard.rs` |
| **Web Dashboard Styling** | CSS for the web dashboard UI | `priv/static/dashboard.css` | `src/web_dashboard.rs` (embedded) |
| **Web Router** | Mounts API, dashboard, and static asset routes | `lib/symphony_elixir_web/router.ex` | `src/server.rs` |
| **Terminal Dashboard** | Real-time ANSI terminal UI showing running agents, retry queue, token accounting, rate limits | `lib/symphony_elixir/status_dashboard.ex` | `src/dashboard.rs` |
| **Sparklines & Throughput** | Unicode block sparkline graphs over a 10-minute window; sliding-window tokens-per-second calculation | `lib/symphony_elixir/status_dashboard.ex` | `src/dashboard.rs` |
| **PubSub Broadcast** | Broadcast system for pushing orchestrator state updates to dashboard and server subscribers | `lib/symphony_elixir_web/observability_pubsub.ex` | `src/pubsub.rs` |
| **Rotating File Logs** | Rotating disk log files with configurable max size and file count | `lib/symphony_elixir/log_file.ex` | `src/log_file.rs` |
| **Path Safety** | Symlink resolution, canonical path validation, ensures workspaces stay under configured root | `lib/symphony_elixir/path_safety.ex` | `src/path_safety.rs` |
| **CLI Entry Point** | Argument parsing, safety acknowledgement flag with warning banner, workflow file validation | `lib/symphony_elixir/cli.ex` | `src/main.rs` |
| **Error Types** | Structured error types for each subsystem (tracker, config, agent, workspace, workflow) | Distributed across modules | `src/error.rs` + per-module error enums |

## Architecture Comparison

### Elixir

```
lib/
├── symphony_elixir/
│   ├── orchestrator.ex          # GenServer polling loop
│   ├── agent_runner.ex          # Per-issue agent execution
│   ├── codex/
│   │   ├── app_server.ex        # JSON-RPC client for Codex
│   │   └── dynamic_tool.ex      # Tool call handler
│   ├── linear/
│   │   ├── client.ex            # GraphQL HTTP client
│   │   ├── adapter.ex           # Tracker behaviour impl
│   │   └── issue.ex             # Issue struct
│   ├── tracker.ex               # Tracker behaviour definition
│   ├── tracker/memory.ex        # In-memory test adapter
│   ├── config.ex                # Config loader
│   ├── config/schema.ex         # Ecto schema validation
│   ├── workflow.ex              # WORKFLOW.md parser
│   ├── workflow_store.ex        # GenServer hot-reload cache
│   ├── prompt_builder.ex        # Solid template renderer
│   ├── workspace.ex             # Workspace lifecycle + SSH
│   ├── ssh.ex                   # SSH command execution
│   ├── path_safety.ex           # Symlink validation
│   ├── status_dashboard.ex      # ANSI terminal dashboard
│   ├── http_server.ex           # Phoenix server wrapper
│   ├── log_file.ex              # Rotating disk logs
│   └── cli.ex                   # escript entrypoint
└── symphony_elixir_web/
    ├── endpoint.ex              # Phoenix endpoint config
    ├── router.ex                # Route definitions
    ├── observability_pubsub.ex  # PubSub broadcast
    ├── presenter.ex             # JSON response formatting
    ├── live/dashboard_live.ex   # LiveView web dashboard
    └── controllers/
        ├── observability_api_controller.ex
        └── static_asset_controller.ex
```

### Rust

```
src/
├── main.rs              # Tokio entrypoint, CLI args, subsystem wiring
├── lib.rs               # Module declarations
├── orchestrator.rs      # Async polling loop, dispatch, retry, state publishing
├── agent/
│   └── mod.rs           # Codex session, JSON-RPC, tools, approvals
├── tracker/
│   ├── mod.rs           # Tracker trait + TrackerClient dispatcher
│   ├── linear.rs        # GraphQL client + Tracker impl
│   └── memory.rs        # In-memory test adapter
├── issue.rs             # Issue + BlockerRef structs
├── config.rs            # YAML config parsing, all sections, validation
├── workflow.rs          # WORKFLOW.md parser + hot-reload store
├── prompt.rs            # minijinja template renderer
├── workspace.rs         # Workspace lifecycle, hooks, remote ops
├── ssh.rs               # SSH host parsing, shell escaping, execution
├── worker.rs            # Worker config, round-robin host selection
├── server.rs            # Axum HTTP server + API endpoints
├── web_dashboard.rs     # Embedded HTML/CSS/JS dashboard
├── dashboard.rs         # ANSI terminal dashboard + sparklines
├── pubsub.rs            # tokio::broadcast observability bus
├── log_file.rs          # tracing-appender rotating file logs
├── path_safety.rs       # Symlink validation, canonicalization
└── error.rs             # Shared error utilities
```

### Key Implementation Differences

| Aspect | Elixir | Rust |
|--------|--------|------|
| Concurrency model | OTP GenServer supervision tree | Tokio async tasks with CancellationToken |
| Config validation | Ecto changesets | Manual validation with serde defaults |
| Template engine | Solid (Liquid-like) | minijinja (Jinja2) |
| HTTP framework | Phoenix + Bandit | Axum + Tokio |
| Web dashboard | Phoenix LiveView (WebSocket push) | Embedded HTML + JS polling `/api/v1/state` |
| State sharing | GenServer calls / PubSub | `Arc<RwLock<T>>` + `tokio::sync::watch` + `broadcast` |
| Logging | OTP Logger + disk_log handler | tracing + tracing-appender |
| Process lifecycle | Supervisor tree with child specs | `tokio::spawn` + `CancellationToken` |
