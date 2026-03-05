use std::path::PathBuf;

use clap::Parser;
use eyre::{Context, Result};
use log::info;

use loopr::cli::{self, Cli, Command};
use loopr::config::Config;
use loopr::daemon;
use loopr::domain;
use loopr::tui;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref()).context("Failed to load configuration")?;
    loopr::prompts::init();
    let role = cli_args.r#as.unwrap_or(domain::role::Role::Coordinator);

    match cli_args.command {
        Some(Command::Diagnose { ref cmd }) => {
            // Diagnose runs locally — no daemon, no session logging needed
            cli::diagnose::run(cmd)
        }
        Some(Command::Daemon) => {
            let session_id = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
            let log_path = loopr::setup_logging(&config, cli_args.log_level.as_deref(), Some(&session_id))
                .context("Failed to setup logging")?;
            let session_dir = log_path.parent().map(PathBuf::from).unwrap_or_default();
            info!("Starting daemon (session: {})", session_id);
            let (ctx, _event_tx) = daemon::context::DaemonContext::shared(config, session_id, session_dir)?;
            daemon::daemon_main(ctx).await.context("daemon exited with error")
        }
        Some(Command::Tui) | None => {
            loopr::setup_logging(&config, cli_args.log_level.as_deref(), None).context("Failed to setup logging")?;
            daemon::ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!(
                "Starting TUI, connecting to daemon at {}",
                config.daemon.socket_path.display()
            );
            tui::run::run_tui(&config.daemon.socket_path)
                .await
                .context("TUI failed")?;
            Ok(())
        }
        Some(ref cmd) => {
            loopr::setup_logging(&config, cli_args.log_level.as_deref(), None).context("Failed to setup logging")?;
            daemon::ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!("CLI command: {:?}", cmd);
            cli::dispatch::run(cmd, &config.daemon.socket_path, role).await
        }
    }
}
