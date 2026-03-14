use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use symphony::config::CliOverrides;
use symphony::orchestrator::Orchestrator;
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
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    init_tracing();

    let cli = Cli::parse();
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
    let shutdown_signal = shutdown.clone();

    let orchestrator = Orchestrator::new(workflow_store);
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
            match orchestrator_task.await {
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
