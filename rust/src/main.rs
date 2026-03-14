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
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

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

    let overrides = CliOverrides {
        api_key: cli.api_key,
        port: cli.port,
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
