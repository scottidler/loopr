use clap::Parser;
use colored::*;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod cli;
mod config;
mod domain;
mod error;
mod id;

use cli::Cli;
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

fn run_application(_cli: &Cli, config: &Config) -> error::Result<()> {
    info!("Starting application with session_id={}", id::generate_id());

    // Load and display configuration
    println!("{}", "Configuration loaded successfully".green());
    if config.debug {
        println!("{}", "Debug mode enabled".yellow());
    }

    // Display current role
    let role = domain::role::Role::Coordinator;
    println!("Hello from {}!", "loopr".cyan());
    println!("Current role: {}", role);
    println!("Author: {}", config.name);

    // Validate that the transition engine is wired up
    let rules: Vec<domain::transition::TransitionRule<&str>> = vec![
        domain::transition::TransitionRule {
            from: "init",
            to: "running",
            role: Some(role),
        },
    ];
    domain::transition::validate_transition("init", "running", role, &rules)?;
    info!("Transition engine validated");

    // Log some information
    info!("Application started at ts={}", id::now_millis());

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
