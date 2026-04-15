use std::path::PathBuf;

use clap::Parser;
use eyre::{Context, Result};
use tracing::info;

use loopr::cli::{self, Cli, Command};
use loopr::config::Config;
use loopr::daemon;
use loopr::domain;
use loopr::tui;

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref()).context("Failed to load configuration")?;
    loopr::prompts::init(&config, Some(&config.project.repo_path)).context("Failed to initialize prompt store")?;
    let role = cli_args.r#as.unwrap_or(domain::role::Role::Coordinator);

    match cli_args.command {
        Some(Command::Score { ref dir, duration_secs }) => {
            // Score runs locally — reads JSONL, writes score.json, no daemon needed
            let score = loopr::scorer::compute(dir, duration_secs).context("Failed to compute score")?;
            let score_path = dir.join("score.json");
            let json = serde_json::to_string_pretty(&score).context("Failed to serialize score")?;
            std::fs::write(&score_path, &json).context("Failed to write score.json")?;
            println!("{json}");
            println!("\nScore written to: {}", score_path.display());
            Ok(())
        }
        Some(Command::Diagnose { ref cmd }) => {
            // Diagnose runs locally — no daemon, no session logging needed
            cli::diagnose::run(cmd)
        }
        Some(Command::Daemon) => {
            // Explicit foreground daemon — create runtime directly
            let session_id = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
            let log_handle = loopr::setup_logging(&config, cli_args.log_level.as_deref(), Some(&session_id))
                .context("Failed to setup logging")?;
            let session_dir = log_handle.log_path.parent().map(PathBuf::from).unwrap_or_default();
            info!("Starting daemon (session: {})", session_id);
            tokio::runtime::Runtime::new()
                .context("Failed to create Tokio runtime")?
                .block_on(async {
                    let (ctx, _event_tx) = daemon::context::DaemonContext::shared(config, session_id, session_dir)?;
                    daemon::daemon_main(ctx).await.context("daemon exited with error")
                })
        }
        Some(Command::Tui) | None => {
            // ensure_daemon may double-fork — MUST happen before Tokio runtime
            daemon::ensure_daemon(&config, cli_args.log_level.as_deref())?;
            // Only the parent reaches here (grandchild diverges in ensure_daemon)
            let _log_handle = loopr::setup_logging(&config, cli_args.log_level.as_deref(), None)
                .context("Failed to setup logging")?;
            info!(
                "Starting TUI, connecting to daemon at {}",
                config.daemon.socket_path.display()
            );
            tokio::runtime::Runtime::new()
                .context("Failed to create Tokio runtime")?
                .block_on(async {
                    tui::run::run_tui(&config.daemon.socket_path)
                        .await
                        .context("TUI failed")?;
                    Ok(())
                })
        }
        Some(ref cmd) => {
            // ensure_daemon may double-fork — MUST happen before Tokio runtime
            daemon::ensure_daemon(&config, cli_args.log_level.as_deref())?;
            // Guard must stay alive until program exit to keep the log writer flushed.
            let _log_handle = loopr::setup_logging(&config, cli_args.log_level.as_deref(), None)
                .context("Failed to setup logging")?;
            info!("CLI command: {:?}", cmd);
            tokio::runtime::Runtime::new()
                .context("Failed to create Tokio runtime")?
                .block_on(async {
                    cli::dispatch::run(cmd, &config.daemon.socket_path, role, &config.clarity_gate).await
                })
        }
    }
}
