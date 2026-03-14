use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use symphony::config::CliOverrides;
use symphony::dashboard::{DashboardConfig, DashboardState, StatusDashboard};
use symphony::log_file::{self, LogFileConfig};
use symphony::orchestrator::{ObservabilityHandles, Orchestrator};
use symphony::pubsub::ObservabilityBus;
use symphony::server::{ObservabilityServer, SharedSnapshot};
use symphony::workflow::WorkflowStore;

#[derive(Debug, Parser)]
#[command(name = "symphony", version, about = "Orchestrate coding agents to work through Linear issues")]
struct Cli {
    /// Path to the WORKFLOW.md file
    #[arg(value_name = "WORKFLOW", default_value = "WORKFLOW.md")]
    workflow_path: PathBuf,

    /// Linear API key (overrides tracker.api_key in WORKFLOW.md and $LINEAR_API_KEY)
    #[arg(long, env = "LINEAR_API_KEY")]
    api_key: Option<String>,

    /// Port for the optional server
    #[arg(long)]
    port: Option<u16>,

    /// Directory for rotating log files (enables file logging in addition to stdout)
    #[arg(long, value_name = "PATH")]
    logs_root: Option<PathBuf>,

    /// Acknowledge that this is an engineering preview running without usual guardrails
    #[arg(long = "i-understand-that-this-will-be-running-without-the-usual-guardrails")]
    guardrails_acknowledged: bool,

    /// Linear project slug (overrides tracker.project_slug in WORKFLOW.md)
    #[arg(long, env = "LINEAR_PROJECT_SLUG")]
    project_slug: Option<String>,

    /// Root directory for per-issue workspaces (overrides workspace.root in WORKFLOW.md)
    #[arg(long, env = "SYMPHONY_WORKSPACE_ROOT")]
    workspace_root: Option<PathBuf>,

    /// Command to launch the Codex app-server (overrides codex.command in WORKFLOW.md)
    #[arg(long, env = "CODEX_COMMAND")]
    codex_command: Option<String>,

    /// Linear assignee filter (overrides tracker.assignee in WORKFLOW.md)
    #[arg(long, env = "LINEAR_ASSIGNEE")]
    assignee: Option<String>,

    /// Maximum number of concurrent agents (overrides agent.max_concurrent_agents in WORKFLOW.md)
    #[arg(long)]
    max_concurrent_agents: Option<usize>,

    /// Polling interval in milliseconds (overrides polling.interval_ms in WORKFLOW.md)
    #[arg(long)]
    polling_interval_ms: Option<u64>,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    if !cli.guardrails_acknowledged {
        eprintln!("{}", acknowledgement_banner());
        std::process::exit(1);
    }

    // Hold the guard for the lifetime of the application so file logs are flushed.
    let _log_guard = if let Some(ref logs_root) = cli.logs_root {
        let config = LogFileConfig::new(logs_root);
        Some(log_file::init_file_logging(&config))
    } else {
        init_tracing();
        None
    };
    let workflow_path = if cli.workflow_path.is_absolute() {
        cli.workflow_path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(cli.workflow_path)
    };

    if !workflow_path.is_file() {
        eprintln!("Workflow file not found: {}", workflow_path.display());
        std::process::exit(1);
    }

    let workspace_root = cli.workspace_root.map(|p| {
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        }
    });

    let overrides = CliOverrides {
        api_key: cli.api_key,
        port: cli.port,
        project_slug: cli.project_slug,
        workspace_root,
        codex_command: cli.codex_command,
        assignee: cli.assignee,
        max_concurrent_agents: cli.max_concurrent_agents,
        polling_interval_ms: cli.polling_interval_ms,
    };

    let workflow_store = match WorkflowStore::open(workflow_path, overrides).await {
        Ok(store) => Arc::new(store),
        Err(error) => {
            eprintln!("Failed to start Symphony: {error}");
            std::process::exit(1);
        }
    };

    let _watch_task = workflow_store.start_polling();
    let shutdown = CancellationToken::new();

    // -----------------------------------------------------------------------
    // Observability subsystems
    // -----------------------------------------------------------------------

    // Shared snapshot for the HTTP server.
    let shared_snapshot = SharedSnapshot::default();

    // PubSub bus for broadcast observability events.
    let bus = ObservabilityBus::default();

    // Dashboard watch channel.
    let (dashboard_tx, dashboard_rx) = tokio::sync::watch::channel(DashboardState::default());

    // -- HTTP observability server -------------------------------------------
    // Read the server port from the workflow config; the CLI --port flag will
    // have been folded in via CliOverrides already.
    let server_port = {
        let runtime = workflow_store.current().await;
        runtime.config.server.port
    };

    if let Some(port) = server_port.filter(|p| *p > 0) {
        let server = ObservabilityServer::new("0.0.0.0", port, shared_snapshot.clone(), shutdown.clone());
        tokio::spawn(async move {
            if let Err(error) = server.run().await {
                eprintln!("Observability server error: {error}");
            }
        });
    }

    // -- Terminal dashboard --------------------------------------------------
    let dashboard = StatusDashboard::new(DashboardConfig::default(), dashboard_rx);
    let dashboard_shutdown = shutdown.clone();
    tokio::spawn(async move {
        dashboard.run(dashboard_shutdown).await;
    });

    // -----------------------------------------------------------------------
    // Orchestrator
    // -----------------------------------------------------------------------

    let observability = ObservabilityHandles {
        bus: Some(bus),
        shared_snapshot: Some(shared_snapshot),
        dashboard_tx: Some(dashboard_tx),
    };

    let shutdown_signal = shutdown.clone();
    let orchestrator = Orchestrator::new(workflow_store, observability);
    let mut orchestrator_task =
        tokio::spawn(async move { orchestrator.run(shutdown_signal).await });

    tokio::select! {
        result = &mut orchestrator_task => {
            match result {
                Ok(Ok(())) => std::process::exit(0),
                Ok(Err(error)) => {
                    eprintln!("Symphony exited with error: {error}");
                    std::process::exit(1);
                }
                Err(error) => {
                    eprintln!("Symphony task failed: {error}");
                    std::process::exit(1);
                }
            }
        }
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                eprintln!("Failed to wait for shutdown signal: {error}");
                std::process::exit(1);
            }
            shutdown.cancel();

            // Render an offline status message before exiting.
            eprintln!("\nSymphony shutting down...");

            match orchestrator_task.await {
                Ok(Ok(())) => {
                    eprintln!("Symphony offline.");
                    std::process::exit(0);
                }
                Ok(Err(error)) => {
                    eprintln!("Symphony exited with error: {error}");
                    std::process::exit(1);
                }
                Err(error) => {
                    eprintln!("Symphony task failed: {error}");
                    std::process::exit(1);
                }
            }
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn acknowledgement_banner() -> String {
    let lines = [
        "This Symphony implementation is a low key engineering preview.",
        "Codex will run without any guardrails.",
        "Symphony is not a supported product and is presented as-is.",
        "To proceed, start with `--i-understand-that-this-will-be-running-without-the-usual-guardrails` CLI argument",
    ];
    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0);
    let border: String = std::iter::repeat('─').take(width + 2).collect();
    let top = format!("╭{border}╮");
    let bottom = format!("╰{border}╯");
    let spacer = format!("│ {:width$} │", "", width = width);

    let mut content = vec![top, spacer.clone()];
    for line in &lines {
        content.push(format!("│ {:width$} │", line, width = width));
    }
    content.push(spacer);
    content.push(bottom);

    format!("\x1b[91;1m{}\x1b[0m", content.join("\n"))
}
