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

fn setup_logging(log_level: Option<&str>) -> Result<()> {
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

    // Check for env var override: LOOPR_LOG_LEVEL
    let level_filter = std::env::var("LOOPR_LOG_LEVEL")
        .ok()
        .and_then(|s| match s.to_lowercase().as_str() {
            "trace" => Some(log::LevelFilter::Trace),
            "debug" => Some(log::LevelFilter::Debug),
            "info" => Some(log::LevelFilter::Info),
            "warn" => Some(log::LevelFilter::Warn),
            "error" => Some(log::LevelFilter::Error),
            _ => None,
        })
        .unwrap_or_else(|| match log_level {
            Some("trace") => log::LevelFilter::Trace,
            Some("debug") => log::LevelFilter::Debug,
            Some("info") => log::LevelFilter::Info,
            Some("warn") => log::LevelFilter::Warn,
            Some("error") => log::LevelFilter::Error,
            _ => log::LevelFilter::Info,
        });

    env_logger::Builder::new()
        .filter_level(level_filter)
        .target(env_logger::Target::Pipe(target))
        .init();

    info!(
        "Logging initialized at level {:?}, writing to: {}",
        level_filter,
        log_file.display()
    );
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
    // Parse CLI arguments
    let cli = Cli::parse();

    // Load log level early (before full config) to enable logging during config parsing
    let log_level = Config::load_log_level(cli.config.as_ref());

    // Setup logging first with early log level
    setup_logging(log_level.as_deref()).context("Failed to setup logging")?;

    info!("Logging initialized, log_level={:?}", log_level);

    // Load full configuration
    let config = Config::load(cli.config.as_ref()).context("Failed to load configuration")?;

    info!("Configuration loaded: llm.default={}", config.llm.default);

    // Run the TUI
    run_tui(&config).await.context("TUI failed")?;

    Ok(())
}
