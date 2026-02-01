use clap::Parser;
use colored::*;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod cli;
mod config;

use cli::Cli;
use cli::commands::{Commands, DaemonCommands};
use config::Config;

fn setup_logging() -> Result<()> {
    // Create log directory
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr")
        .join("logs");

    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    let log_file = log_dir.join("loopr.log");

    // Setup env_logger with file output
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

fn run_application(cli: &Cli, config: &Config) -> Result<()> {
    info!("Starting application");

    if cli.is_verbose() {
        println!("{}", "Verbose mode enabled".yellow());
    }

    match &cli.command {
        None => {
            // Default: launch TUI mode
            run_tui(config)
        }
        Some(Commands::Daemon { command }) => handle_daemon_command(command, config),
        Some(Commands::Plan { task }) => handle_plan_command(task, config),
        Some(Commands::List { status, loop_type }) => {
            handle_list_command(status.as_deref(), loop_type.as_deref(), config)
        }
        Some(Commands::Status { id, detailed }) => handle_status_command(id, *detailed, config),
        Some(Commands::Approve { id }) => handle_approve_command(id, config),
        Some(Commands::Reject { id, reason }) => {
            handle_reject_command(id, reason.as_deref(), config)
        }
        Some(Commands::Pause { id }) => handle_pause_command(id, config),
        Some(Commands::Resume { id }) => handle_resume_command(id, config),
        Some(Commands::Cancel { id }) => handle_cancel_command(id, config),
    }
}

// Command handlers - stubs for now, will be wired to real implementations

fn run_tui(config: &Config) -> Result<()> {
    info!("Launching TUI mode");
    println!("{}", "Launching TUI...".cyan());
    if config.debug {
        println!("{}", "Debug mode enabled".yellow());
    }
    // TODO: Wire up actual TUI
    Ok(())
}

fn handle_daemon_command(command: &DaemonCommands, config: &Config) -> Result<()> {
    info!("Handling daemon command: {:?}", command);
    match command {
        DaemonCommands::Start { foreground } => {
            if *foreground {
                println!("{}", "Starting daemon in foreground...".cyan());
            } else {
                println!("{}", "Starting daemon...".cyan());
            }
            // TODO: Wire up daemon start
        }
        DaemonCommands::Stop => {
            println!("{}", "Stopping daemon...".cyan());
            // TODO: Wire up daemon stop
        }
        DaemonCommands::Status => {
            println!("{}", "Checking daemon status...".cyan());
            // TODO: Wire up daemon status check
        }
        DaemonCommands::Restart => {
            println!("{}", "Restarting daemon...".cyan());
            // TODO: Wire up daemon restart
        }
    }
    let _ = config; // Suppress unused warning until wired up
    Ok(())
}

fn handle_plan_command(task: &str, config: &Config) -> Result<()> {
    info!("Creating plan for task: {}", task);
    println!("{} Creating plan: {}", "Planning:".green(), task);
    let _ = config;
    // TODO: Wire up plan creation via LoopManager
    Ok(())
}

fn handle_list_command(
    status: Option<&str>,
    loop_type: Option<&str>,
    config: &Config,
) -> Result<()> {
    info!("Listing loops - status: {:?}, type: {:?}", status, loop_type);
    println!("{}", "Listing loops...".cyan());
    if let Some(s) = status {
        println!("  Filtering by status: {}", s);
    }
    if let Some(t) = loop_type {
        println!("  Filtering by type: {}", t);
    }
    let _ = config;
    // TODO: Wire up loop listing via Storage
    Ok(())
}

fn handle_status_command(id: &str, detailed: bool, config: &Config) -> Result<()> {
    info!("Getting status for loop: {} (detailed: {})", id, detailed);
    println!("{} {}", "Status for:".green(), id);
    if detailed {
        println!("  (detailed view)");
    }
    let _ = config;
    // TODO: Wire up status retrieval via Storage
    Ok(())
}

fn handle_approve_command(id: &str, config: &Config) -> Result<()> {
    info!("Approving plan: {}", id);
    println!("{} {}", "Approving:".green(), id);
    let _ = config;
    // TODO: Wire up plan approval via LoopManager
    Ok(())
}

fn handle_reject_command(id: &str, reason: Option<&str>, config: &Config) -> Result<()> {
    info!("Rejecting plan: {} (reason: {:?})", id, reason);
    println!("{} {}", "Rejecting:".red(), id);
    if let Some(r) = reason {
        println!("  Reason: {}", r);
    }
    let _ = config;
    // TODO: Wire up plan rejection via LoopManager
    Ok(())
}

fn handle_pause_command(id: &str, config: &Config) -> Result<()> {
    info!("Pausing loop: {}", id);
    println!("{} {}", "Pausing:".yellow(), id);
    let _ = config;
    // TODO: Wire up loop pause via signal
    Ok(())
}

fn handle_resume_command(id: &str, config: &Config) -> Result<()> {
    info!("Resuming loop: {}", id);
    println!("{} {}", "Resuming:".green(), id);
    let _ = config;
    // TODO: Wire up loop resume via signal
    Ok(())
}

fn handle_cancel_command(id: &str, config: &Config) -> Result<()> {
    info!("Canceling loop: {}", id);
    println!("{} {}", "Canceling:".red(), id);
    let _ = config;
    // TODO: Wire up loop cancellation via signal
    Ok(())
}

fn main() -> Result<()> {
    // Setup logging first
    setup_logging().context("Failed to setup logging")?;

    // Parse CLI arguments
    let cli = Cli::parse();

    // Load configuration
    let config = Config::load(cli.config.as_ref()).context("Failed to load configuration")?;

    info!("Starting with config from: {:?}", cli.config);

    // Run the main application logic
    run_application(&cli, &config).context("Application failed")?;

    Ok(())
}
