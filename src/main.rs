use clap::Parser;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::{Path, PathBuf};

mod agents;
mod cli;
mod config;
mod daemon;
mod domain;
mod error;
mod id;
mod ipc;
mod tools;
mod tui;
mod validator;
mod worktree;

#[cfg(test)]
mod integration_tests;

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

/// Check if the daemon is running; if not, spawn it in the background and wait
/// for the socket to appear. This lets `loopr` (TUI) and CLI commands work
/// without requiring a separate `loopr daemon` step.
fn ensure_daemon(pid_path: &Path, socket_path: &Path) -> Result<()> {
    // Check PID file — is a daemon already alive?
    if let Ok(contents) = fs::read_to_string(pid_path)
        && let Ok(pid) = contents.trim().parse::<u32>()
        && Path::new(&format!("/proc/{pid}")).exists()
    {
        // Daemon is running — check socket exists too
        if socket_path.exists() {
            return Ok(());
        }
        // PID alive but socket missing — daemon may still be starting up
        info!("Daemon process alive (pid={pid}) but socket not ready, waiting...");
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if socket_path.exists() {
                return Ok(());
            }
        }
        return Err(eyre::eyre!(
            "daemon process alive (pid={pid}) but socket never appeared at {}",
            socket_path.display()
        ));
    }

    // No live daemon — spawn one
    eprintln!("Starting daemon...");
    let exe = std::env::current_exe().context("failed to determine loopr executable path")?;
    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn daemon process")?;

    // Wait for socket to appear (up to 3 seconds)
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if socket_path.exists() {
            return Ok(());
        }
    }

    Err(eyre::eyre!(
        "daemon was spawned but socket never appeared at {}",
        socket_path.display()
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
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
            ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
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
            ensure_daemon(&config.daemon.pid_path, &config.daemon.socket_path)?;
            info!("CLI command: {:?}", cmd);
            cli::dispatch::run(cmd, &config.daemon.socket_path, role).await
        }
    }
}
