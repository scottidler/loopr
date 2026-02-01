use clap::Parser;
use colored::*;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::execute;
use eyre::{Context, Result};
use log::info;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Tabs};
use ratatui::Terminal;
use std::fs;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

mod cli;
mod config;

use cli::Cli;
use cli::commands::{Commands, DaemonCommands};
use config::Config;

// Re-export from lib for local usage
use loopr::coordination::SignalManager;
use loopr::domain::{LoopStatus, LoopType};
use loopr::llm::MockLlmClient;
use loopr::manager::{LoopManager, LoopManagerConfig};
use loopr::storage::JsonlStorage;
use loopr::tools::{LocalToolRouter, ToolCatalog};
use loopr::tui::{App, InputHandler, View};
use loopr::tui::views::{ApprovalView, ChatView, LoopsView};
use loopr::tui::app::{ActiveView, LoopSummary, MessageSender};
use loopr::validation::{ValidationResult, Validator};
use loopr::worktree::WorktreeManager;

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
        Some(Commands::Reject { id, reason }) => handle_reject_command(id, reason.as_deref(), config),
        Some(Commands::Pause { id }) => handle_pause_command(id, config),
        Some(Commands::Resume { id }) => handle_resume_command(id, config),
        Some(Commands::Cancel { id }) => handle_cancel_command(id, config),
    }
}

// Command handlers - wired to real implementations

fn run_tui(config: &Config) -> Result<()> {
    info!("Launching TUI mode");

    // 1. Enable raw mode
    enable_raw_mode().context("Failed to enable raw mode")?;

    // 2. Setup terminal with alternate screen
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;

    // 3. Create app state
    let mut app = App::with_defaults();
    if config.debug {
        app.set_status("Debug mode enabled");
    }

    // Add welcome message
    app.add_chat_message(
        MessageSender::System,
        "Welcome to Loopr! Press Tab to switch views, q to quit.".to_string(),
    );

    // 4. Run event loop
    let result = run_event_loop(&mut terminal, &mut app);

    // 5. Restore terminal (always, even on error)
    disable_raw_mode().context("Failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("Failed to leave alternate screen")?;

    result
}

/// Run the TUI event loop
fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let input_handler = InputHandler::new();
    let chat_view = ChatView::new();
    let loops_view = LoopsView::new();
    let approval_view = ApprovalView::new();

    while !app.state.should_quit {
        // Render the UI
        terminal.draw(|frame| {
            let size = frame.area();

            // Create main layout
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),  // Tabs
                    Constraint::Min(0),     // Main content
                    Constraint::Length(1),  // Status bar
                ])
                .split(size);

            // Render tabs
            let tab_titles: Vec<Line> = vec![
                Line::from(" Chat "),
                Line::from(" Loops "),
                Line::from(" Approval "),
            ];
            let selected_tab = match app.state.active_view {
                ActiveView::Chat => 0,
                ActiveView::Loops => 1,
                ActiveView::Approval => 2,
            };
            let tabs = Tabs::new(tab_titles)
                .block(Block::default().borders(Borders::ALL).title(" Loopr "))
                .select(selected_tab)
                .style(Style::default().fg(Color::White))
                .highlight_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
            frame.render_widget(tabs, chunks[0]);

            // Render active view
            match app.state.active_view {
                ActiveView::Chat => chat_view.render(frame, chunks[1], &app.state),
                ActiveView::Loops => loops_view.render(frame, chunks[1], &app.state),
                ActiveView::Approval => approval_view.render(frame, chunks[1], &app.state),
            }

            // Render status bar
            let status_text = app.state.status_message.as_deref().unwrap_or("Press Tab to switch views, q to quit");
            let status = ratatui::widgets::Paragraph::new(status_text)
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(status, chunks[2]);
        })?;

        // Handle input
        if let Some(key) = input_handler.poll()? {
            if key.is_quit() {
                app.quit();
            } else if key.is_tab() {
                app.next_view();
            } else if key.is_escape() {
                app.prev_view();
            } else {
                // View-specific input handling
                match app.state.active_view {
                    ActiveView::Chat => {
                        if key.is_enter() && !app.state.chat_input.is_empty() {
                            let msg = std::mem::take(&mut app.state.chat_input);
                            app.add_chat_message(MessageSender::User, msg);
                            app.add_chat_message(
                                MessageSender::Daemon,
                                "Command received. (Daemon not connected)".to_string(),
                            );
                        } else if let Some(c) = key.char() {
                            app.state.chat_input.push(c);
                        } else if key.is_backspace() && !app.state.chat_input.is_empty() {
                            app.state.chat_input.pop();
                        }
                    }
                    ActiveView::Loops => {
                        if key.is_up() {
                            app.select_prev_loop();
                        } else if key.is_down() {
                            app.select_next_loop();
                        }
                    }
                    ActiveView::Approval => {
                        // Approval view input handling
                        if let Some(approval) = &mut app.state.pending_approval {
                            if let Some(c) = key.char() {
                                approval.feedback.push(c);
                            } else if key.is_backspace() && !approval.feedback.is_empty() {
                                approval.feedback.pop();
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn handle_daemon_command(command: &DaemonCommands, config: &Config) -> Result<()> {
    info!("Handling daemon command: {:?}", command);
    if config.debug {
        println!("{}", "[debug] Daemon command handler".yellow());
    }
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
    Ok(())
}

fn handle_plan_command(task: &str, config: &Config) -> Result<()> {
    info!("Creating plan for task: {}", task);
    if config.debug {
        println!("{}", "[debug] Plan command handler".yellow());
    }
    println!("{} Creating plan: {}", "Planning:".green(), task);
    // TODO: Wire up plan creation via LoopManager
    Ok(())
}

fn handle_list_command(status: Option<&str>, loop_type: Option<&str>, config: &Config) -> Result<()> {
    info!("Listing loops - status: {:?}, type: {:?}", status, loop_type);
    if config.debug {
        println!("{}", "[debug] List command handler".yellow());
    }
    println!("{}", "Listing loops...".cyan());
    if let Some(s) = status {
        println!("  Filtering by status: {}", s);
    }
    if let Some(t) = loop_type {
        println!("  Filtering by type: {}", t);
    }
    // TODO: Wire up loop listing via Storage
    Ok(())
}

fn handle_status_command(id: &str, detailed: bool, config: &Config) -> Result<()> {
    info!("Getting status for loop: {} (detailed: {})", id, detailed);
    if config.debug {
        println!("{}", "[debug] Status command handler".yellow());
    }
    println!("{} {}", "Status for:".green(), id);
    if detailed {
        println!("  (detailed view)");
    }
    // TODO: Wire up status retrieval via Storage
    Ok(())
}

fn handle_approve_command(id: &str, config: &Config) -> Result<()> {
    info!("Approving plan: {}", id);
    if config.debug {
        println!("{}", "[debug] Approve command handler".yellow());
    }
    println!("{} {}", "Approving:".green(), id);
    // TODO: Wire up plan approval via LoopManager
    Ok(())
}

fn handle_reject_command(id: &str, reason: Option<&str>, config: &Config) -> Result<()> {
    info!("Rejecting plan: {} (reason: {:?})", id, reason);
    if config.debug {
        println!("{}", "[debug] Reject command handler".yellow());
    }
    println!("{} {}", "Rejecting:".red(), id);
    if let Some(r) = reason {
        println!("  Reason: {}", r);
    }
    // TODO: Wire up plan rejection via LoopManager
    Ok(())
}

fn handle_pause_command(id: &str, config: &Config) -> Result<()> {
    info!("Pausing loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Pause command handler".yellow());
    }
    println!("{} {}", "Pausing:".yellow(), id);
    // TODO: Wire up loop pause via signal
    Ok(())
}

fn handle_resume_command(id: &str, config: &Config) -> Result<()> {
    info!("Resuming loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Resume command handler".yellow());
    }
    println!("{} {}", "Resuming:".green(), id);
    // TODO: Wire up loop resume via signal
    Ok(())
}

fn handle_cancel_command(id: &str, config: &Config) -> Result<()> {
    info!("Canceling loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Cancel command handler".yellow());
    }
    println!("{} {}", "Canceling:".red(), id);
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
