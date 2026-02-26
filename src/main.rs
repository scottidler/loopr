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

    // Validate hierarchy status transitions are wired up
    let plan = domain::plan::Plan::new(
        "bootstrap".to_string(),
        "Bootstrap plan".to_string(),
        "Compiles and passes tests".to_string(),
    );
    info!("Created plan: {} (status={})", plan.id, plan.status);

    let spec = domain::spec::Spec::new(
        plan.id.clone(),
        "Bootstrap spec".to_string(),
        "Detailed specification".to_string(),
    );
    info!(
        "Created spec: {} (plan={}, status={})",
        spec.id, spec.plan_id, spec.status
    );

    let phase = domain::phase::Phase::new(
        spec.id.clone(),
        "Bootstrap phase".to_string(),
        "First implementation phase".to_string(),
        1,
    );
    info!(
        "Created phase: {} (spec={}, order={}, status={})",
        phase.id, phase.spec_id, phase.order, phase.status
    );

    let work_item = domain::work_item::WorkItem::new(
        phase.id.clone(),
        "Bootstrap work item".to_string(),
        "First concrete task".to_string(),
    );
    info!(
        "Created work_item: {} (phase={}, status={})",
        work_item.id, work_item.phase_id, work_item.status
    );

    // Validate work item FSM is wired up
    let wi_rules = domain::work_item::work_item_transitions();
    domain::transition::validate_transition(
        domain::work_item::WorkItemStatus::Draft,
        domain::work_item::WorkItemStatus::Ready,
        role,
        &wi_rules,
    )?;
    info!("WorkItem FSM validated ({} rules)", wi_rules.len());

    // Validate bundle FSM is wired up
    let bundle = domain::bundle::Bundle::new(
        work_item.id.clone(),
        None,
        "feature/bootstrap".to_string(),
        "Bootstrap bundle".to_string(),
    );
    info!(
        "Created bundle: {} (work_item={}, status={})",
        bundle.id, bundle.work_item_id, bundle.status
    );
    let bundle_rules = domain::bundle::bundle_transitions();
    domain::transition::validate_transition(
        domain::bundle::BundleStatus::Proposed,
        domain::bundle::BundleStatus::Triaged,
        role,
        &bundle_rules,
    )?;
    info!("Bundle FSM validated ({} rules)", bundle_rules.len());

    // Validate tick FSM is wired up
    let tick = domain::tick::Tick::new(1);
    info!(
        "Created tick: {} (number={}, status={})",
        tick.id, tick.number, tick.status
    );
    let tick_rules = domain::tick::tick_transitions();
    domain::transition::validate_transition(
        domain::tick::TickStatus::Open,
        domain::tick::TickStatus::Sealing,
        domain::role::Role::Integrator,
        &tick_rules,
    )?;
    info!("Tick FSM validated ({} rules)", tick_rules.len());

    // Validate hierarchy FSM is wired up
    let hierarchy_rules = domain::plan::hierarchy_transitions();
    domain::transition::validate_transition(
        domain::plan::HierarchyStatus::Draft,
        domain::plan::HierarchyStatus::Active,
        role,
        &hierarchy_rules,
    )?;
    info!("Hierarchy FSM validated ({} rules)", hierarchy_rules.len());

    // Validate learning record is wired up
    let mut learning = domain::learning::Learning::new(
        work_item.id.clone(),
        domain::learning::LearningScope::WorkItem,
        "Bootstrap learning".to_string(),
    );
    learning.reinforce();
    learning.contradict();
    learning.promote();
    learning.demote();
    info!(
        "Created learning: {} (source={}, scope={}, promoted={})",
        learning.id, learning.source_id, learning.scope, learning.promoted
    );

    // Validate lock record is wired up
    let mut lock = domain::lock::Lock::new(
        "src/main.rs".to_string(),
        work_item.id.clone(),
        "coordinator".to_string(),
    );
    info!(
        "Created lock: {} (resource={}, holder={}, status={})",
        lock.id, lock.resource, lock.holder_id, lock.status
    );
    assert!(lock.is_active());
    lock.release();
    info!("Lock released: status={}", lock.status);
    lock.expire();
    info!("Lock expired: status={}", lock.status);

    // Validate that the transition engine is wired up
    let rules: Vec<domain::transition::TransitionRule<&str>> = vec![domain::transition::TransitionRule {
        from: "init",
        to: "running",
        role: Some(role),
    }];
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
