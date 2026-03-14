# Symphony Rust

This directory contains a Rust implementation of Symphony, based on
[`SPEC.md`](../SPEC.md) at the repository root.

> [!WARNING]
> Symphony Rust is prototype software intended for evaluation only and is presented as-is.
> We recommend implementing your own hardened version based on `SPEC.md`.

## How it works

1. Polls Linear for candidate work
2. Creates a workspace per issue
3. Launches Codex in [App Server mode](https://developers.openai.com/codex/app-server/) inside the
   workspace
4. Sends a workflow prompt to Codex
5. Keeps Codex working on the issue until the work is done

If a claimed issue moves to a terminal state (`Done`, `Closed`, `Cancelled`, or `Duplicate`),
Symphony stops the active agent for that issue and cleans up matching workspaces.

## How to use it

1. Make sure your codebase is set up to work well with agents: see
   [Harness engineering](https://openai.com/index/harness-engineering/).
2. Get a new personal token in Linear via Settings > Security & access > Personal API keys, and
   set it as the `LINEAR_API_KEY` environment variable.
3. Copy a `WORKFLOW.md` to your repo (see [Configuration](#configuration) below).
4. Customize the `WORKFLOW.md` file for your project.
   - To get your project's slug, right-click the project and copy its URL. The slug is part of the
     URL.

## Prerequisites

- Rust nightly toolchain (managed via `rust-toolchain.toml`)

Install the toolchain if needed:

```bash
rustup install nightly
```

## Build

```bash
cd rust
cargo build --release
```

The binary is placed at `target/release/symphony`.

## Run

```
symphony [OPTIONS] [WORKFLOW]
```

| Argument / Flag | Default | Description |
|-----------------|---------|-------------|
| `WORKFLOW` | `WORKFLOW.md` | Path to the workflow file. |
| `--api-key` | `$LINEAR_API_KEY` | Linear API key. Overrides `tracker.api_key` in WORKFLOW.md. Also reads from the `LINEAR_API_KEY` environment variable. |
| `--port` | _(none)_ | Port for the optional server. Overrides `server.port` in WORKFLOW.md. |
| `--project-slug` | `$LINEAR_PROJECT_SLUG` | Linear project slug. Overrides `tracker.project_slug` in WORKFLOW.md. |
| `--workspace-root` | `$SYMPHONY_WORKSPACE_ROOT` | Root directory for per-issue workspaces. Overrides `workspace.root` in WORKFLOW.md. |
| `--codex-command` | `$CODEX_COMMAND` | Command to launch the Codex app-server. Overrides `codex.command` in WORKFLOW.md. |
| `--assignee` | `$LINEAR_ASSIGNEE` | Linear assignee filter. Overrides `tracker.assignee` in WORKFLOW.md. |
| `--max-concurrent-agents` | _(none)_ | Maximum number of concurrent agents. Overrides `agent.max_concurrent_agents` in WORKFLOW.md. |
| `--polling-interval-ms` | _(none)_ | Polling interval in milliseconds. Overrides `polling.interval_ms` in WORKFLOW.md. |
| `--version` | | Print version and exit. |
| `--help` | | Print help and exit. |

```bash
# From the rust/ directory, using WORKFLOW.md in the current directory:
cargo run --release

# Or specify a workflow file path:
cargo run --release -- /path/to/WORKFLOW.md

# Pass the API key as a flag:
cargo run --release -- --api-key lin_api_... /path/to/WORKFLOW.md

# Or run the compiled binary directly:
./target/release/symphony --api-key lin_api_... --port 4000 /path/to/WORKFLOW.md

# Override operational settings via flags:
./target/release/symphony \
  --project-slug my-project \
  --workspace-root /data/workspaces \
  --max-concurrent-agents 20 \
  --polling-interval-ms 10000 \
  /path/to/WORKFLOW.md
```

Symphony runs as a long-lived daemon. Press `Ctrl-C` to trigger a graceful shutdown.

## Configuration

The `WORKFLOW.md` file uses YAML front matter for configuration, plus a Markdown body used as the
Codex session prompt. The prompt body supports [Jinja2](https://jinja.palletsprojects.com/)
template syntax.

Minimal example:

```md
---
tracker:
  kind: linear
  project_slug: "..."
workspace:
  root: ~/code/workspaces
hooks:
  after_create: |
    git clone git@github.com:your-org/your-repo.git .
agent:
  max_concurrent_agents: 10
  max_turns: 20
codex:
  command: codex app-server
---

You are working on a Linear issue {{ issue.identifier }}.

Title: {{ issue.title }} Body: {{ issue.description }}
```

### Configuration reference

#### `tracker`

| Key | Default | Description |
|-----|---------|-------------|
| `kind` | _(required)_ | Tracker type. Must be `linear`. |
| `endpoint` | `https://api.linear.app/graphql` | Linear GraphQL endpoint. |
| `api_key` | `$LINEAR_API_KEY` | API key. Use `$ENV_VAR` syntax for environment variable indirection. Falls back to the `LINEAR_API_KEY` environment variable when unset. |
| `project_slug` | _(required)_ | Linear project slug. |
| `active_states` | `["Todo", "In Progress"]` | Issue states that trigger agent dispatch. |
| `terminal_states` | `["Closed", "Cancelled", "Canceled", "Duplicate", "Done"]` | Issue states that stop agents and clean up workspaces. |

#### `polling`

| Key | Default | Description |
|-----|---------|-------------|
| `interval_ms` | `30000` | How often to poll the tracker for new work (milliseconds). |

#### `workspace`

| Key | Default | Description |
|-----|---------|-------------|
| `root` | System temp dir `/symphony_workspaces` | Root directory for per-issue workspaces. Supports `~` expansion and `$ENV_VAR` indirection. |

#### `hooks`

Shell commands run at workspace lifecycle events. Each hook runs inside the workspace directory.

| Key | Default | Description |
|-----|---------|-------------|
| `after_create` | _(none)_ | Runs after a workspace is first created. Use this to clone a repo. |
| `before_run` | _(none)_ | Runs before the agent starts. |
| `after_run` | _(none)_ | Runs after the agent completes. |
| `before_remove` | _(none)_ | Runs before workspace deletion. |
| `timeout_ms` | `60000` | Maximum time a hook may run (milliseconds). |

#### `agent`

| Key | Default | Description |
|-----|---------|-------------|
| `max_concurrent_agents` | `10` | Global concurrency limit for running agents. |
| `max_turns` | `20` | Maximum back-to-back Codex turns per agent invocation. |
| `max_retry_backoff_ms` | `300000` | Cap on exponential backoff delay between retries (milliseconds). |
| `max_concurrent_agents_by_state` | _(none)_ | Per-state concurrency overrides, e.g. `{ "In Progress": 2 }`. |

#### `codex`

| Key | Default | Description |
|-----|---------|-------------|
| `command` | `codex app-server` | Command to launch the Codex app-server process. |
| `approval_policy` | `{"reject":{"sandbox_approval":true,"rules":true,"mcp_elicitations":true}}` | Approval policy passed to Codex. The default rejects sandbox, rules, and MCP elicitation approvals. Set to `"never"` to auto-approve all requests. |
| `thread_sandbox` | `"workspace-write"` | Sandbox mode for agent threads. |
| `turn_sandbox_policy` | _(auto)_ | Explicit sandbox policy object passed through to Codex. When omitted, a `workspaceWrite` policy scoped to the issue workspace is generated automatically. |
| `turn_timeout_ms` | `3600000` | Maximum time for a single agent turn (milliseconds). |
| `read_timeout_ms` | `5000` | Timeout for reading app-server responses (milliseconds). |
| `stall_timeout_ms` | `300000` | Inactivity timeout before an agent is considered stalled (milliseconds). |

#### `server`

| Key | Default | Description |
|-----|---------|-------------|
| `port` | _(none)_ | Optional server port. |

### Configuration notes

- If a value is missing, defaults are used.
- `tracker.api_key` reads from `LINEAR_API_KEY` when unset or when value is `$LINEAR_API_KEY`.
- For path values, `~` is expanded to the home directory.
- For env-backed path values, use `$VAR`. `workspace.root` resolves `$VAR` before path handling.
- If `WORKFLOW.md` is missing or has invalid YAML at startup, Symphony exits with an error.
- If a later reload fails, Symphony keeps running with the last known good workflow and logs the
  reload error until the file is fixed.
- Prompt templates use Jinja2 syntax. Available variables include `issue.identifier`, `issue.title`,
  `issue.description`, `issue.state`, `issue.labels`, `issue.url`, and other issue fields.

### Environment variable example

```yaml
tracker:
  api_key: $LINEAR_API_KEY
workspace:
  root: $SYMPHONY_WORKSPACE_ROOT
hooks:
  after_create: |
    git clone --depth 1 "$SOURCE_REPO_URL" .
codex:
  command: "$CODEX_BIN app-server --model gpt-5.3-codex"
```

## Logging

Symphony uses `tracing` for structured logging. Control the log level with the `RUST_LOG`
environment variable:

```bash
RUST_LOG=debug cargo run --release
```

The default log level is `info`.

## Orchestration behavior

- **Dispatch order**: Issues are sorted by priority (lower number = higher priority), then by
  creation date (oldest first).
- **Concurrency**: Bounded by `max_concurrent_agents` globally and optionally per-state via
  `max_concurrent_agents_by_state`.
- **Retries**: Failed agent runs are retried with exponential backoff, capped at
  `max_retry_backoff_ms`. Continuation retries (agent completed normally but issue is still active)
  use a short 1-second delay.
- **Blocking dependencies**: Issues in a "todo" state with incomplete blockers are skipped until
  their dependencies resolve.
- **Hot reload**: Changes to `WORKFLOW.md` are detected automatically (polled every second) and
  applied without restarting the service.
- **Graceful shutdown**: `Ctrl-C` cancels running agents and waits for cleanup before exiting.

## Project layout

```
src/
  main.rs             CLI entry point, signal handling
  lib.rs              Public module declarations
  orchestrator.rs     Core polling, dispatch, retry, and state management
  agent/mod.rs        Codex app-server JSON-RPC protocol
  config.rs           YAML config parsing, validation, defaults
  workflow.rs         WORKFLOW.md parsing and hot-reload
  workspace.rs        Per-issue workspace lifecycle
  tracker/linear.rs   Linear GraphQL API client
  prompt.rs           Jinja2 template rendering
  path_safety.rs      Symlink escape and directory traversal prevention
  issue.rs            Issue data model
  error.rs            Error types
tests/                Integration tests
```

## Testing

```bash
cargo test
```

## License

This project is licensed under the [Apache License 2.0](../LICENSE).
