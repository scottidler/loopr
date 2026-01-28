// Library modules contain components designed to be used together but not all
// are wired through main.rs yet. This is expected for a phase-based build.
#![allow(dead_code)]

use clap::Parser;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

mod cli;
mod config;
mod llm;
mod loops;
mod recovery;
mod rule_of_five;
mod scheduler;
mod store;
mod tui;
mod validation;

use cli::Cli;
use config::Config;
use store::TaskStore;

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

/// Get the project directory for data storage.
fn get_project_dir() -> Result<PathBuf> {
    // Use current directory as project root
    let project_root = std::env::current_dir().context("Failed to get current directory")?;

    // Create .loopr directory for data
    let data_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr")
        .join("projects");

    // Use a hash of the project path for the project ID
    let project_id = store::compute_project_hash(&project_root)?;
    let project_dir = data_dir.join(&project_id);

    fs::create_dir_all(&project_dir).context("Failed to create project data directory")?;

    Ok(project_dir)
}

async fn run_tui(_config: &Config) -> Result<()> {
    info!("Starting TUI mode");

    // Initialize TaskStore
    let project_dir = get_project_dir()?;
    let store = TaskStore::open_at(&project_dir).context("Failed to open TaskStore")?;
    let store = Arc::new(Mutex::new(store));

    // Initialize terminal
    let terminal = tui::init_terminal().context("Failed to initialize terminal")?;

    // Create and run TUI
    let mut runner = tui::TuiRunner::with_store(terminal, store);

    // Run the TUI loop
    let result = runner.run().await;

    // Always restore terminal, even on error
    tui::restore_terminal().context("Failed to restore terminal")?;

    result
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup logging first
    setup_logging().context("Failed to setup logging")?;

    // Parse CLI arguments
    let cli = Cli::parse();

    // Load configuration
    let config = Config::load(cli.config.as_ref()).context("Failed to load configuration")?;

    info!("Starting with config from: {:?}", cli.config);

    // Run the TUI
    run_tui(&config).await.context("TUI failed")?;

    Ok(())
}
