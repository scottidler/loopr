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
    loopr::setup_logging().context("Failed to setup logging")?;

    let cli_args = Cli::parse();
    let config = Config::load(cli_args.config.as_ref()).context("Failed to load configuration")?;
    let role = cli_args.r#as.unwrap_or(domain::role::Role::Coordinator);

    match cli_args.command {
        Some(Command::Daemon) => {
            info!("Starting daemon");
            let (ctx, _event_tx) = daemon::context::DaemonContext::shared(config)?;
            daemon::daemon_main(ctx).await.context("daemon exited with error")
        }
        Some(Command::Tui) | None => {
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
            daemon::ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!("CLI command: {:?}", cmd);
            cli::dispatch::run(cmd, &config.daemon.socket_path, role).await
        }
    }
}
