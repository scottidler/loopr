use clap::Parser;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod agents;
mod cli;
mod config;
mod daemon;
mod domain;
mod error;
mod id;
mod ipc;
mod tui;
mod validator;
mod worktree;

use cli::Cli;
use config::Config;

fn setup_logging() -> Result<()> {
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr")
        .join("logs");

    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    let log_file = log_dir.join("loopr.log");

    let target = Box::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .context("Failed to open log file")?,
    );

    env_logger::Builder::from_default_env()
        .target(env_logger::Target::Pipe(target))
        .init();

    info!("Logging initialized, writing to: {}", log_file.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_logging().context("Failed to setup logging")?;

    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref()).context("Failed to load configuration")?;
    let role = cli_args.r#as.unwrap_or(domain::role::Role::Coordinator);

    match cli_args.command {
        // `loopr daemon` — run as daemon
        Some(cli::Command::Daemon) => {
            info!("Starting daemon");
            let (ctx, _event_tx) = daemon::context::DaemonContext::shared(config)?;
            daemon::daemon_main(ctx).await.context("daemon exited with error")
        }

        // `loopr tui` or no subcommand — start TUI
        Some(cli::Command::Tui) | None => {
            info!(
                "Starting TUI, connecting to daemon at {}",
                config.daemon.socket_path.display()
            );
            tui::run::run_tui(&config.daemon.socket_path)
                .await
                .context("TUI failed")?;
            Ok(())
        }

        // All other subcommands — dispatch as CLI IPC request
        Some(ref cmd) => {
            info!("CLI command: {:?}", cmd);
            cli::dispatch::run(cmd, &config.daemon.socket_path, role).await
        }
    }
}
