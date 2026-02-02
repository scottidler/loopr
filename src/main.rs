use clap::Parser;
use colored::*;
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use eyre::{Context, Result, eyre};
use log::info;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Tabs};
use std::fs;
use std::io::stdout;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

mod cli;
mod config;

use cli::Cli;
use cli::commands::{Commands, DaemonCommands};
use config::Config;
use loopr::daemon::{Daemon, DaemonConfig, default_pid_path, default_socket_path};
use loopr::tui::app::{ActiveView, MessageSender};
use loopr::tui::views::{ApprovalView, ChatView, LoopsView};
use loopr::tui::{App, InputHandler, View};

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

async fn run_application(cli: &Cli, config: &Config) -> Result<()> {
    info!("Starting application");

    if cli.is_verbose() {
        println!("{}", "Verbose mode enabled".yellow());
    }

    match &cli.command {
        None => {
            // Default: launch TUI mode
            run_tui(config).await
        }
        Some(Commands::Daemon { command }) => handle_daemon_command(command, config).await,
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

/// Try to connect to daemon, auto-starting if needed
async fn try_connect_with_autostart(app: &mut App, socket_path: &str, pid_path: &std::path::Path) -> Result<bool> {
    // First attempt - try direct connection with short timeout
    match tokio::time::timeout(Duration::from_secs(2), app.connect()).await {
        Ok(Ok(())) => return Ok(true),
        Ok(Err(_)) | Err(_) => {
            // Connection failed, check if daemon is running
        }
    }

    // Check if daemon is running but just slow
    if Daemon::is_running(pid_path) {
        eprintln!("{} Daemon is running but not responding, waiting...", "⏳".yellow());
        // Give it more time - maybe it's still starting
        for attempt in 1..=5 {
            tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
            if app.connect().await.is_ok() {
                return Ok(true);
            }
        }
        eprintln!();
        eprintln!("{}", "═══════════════════════════════════════════════════════".red());
        eprintln!("{}", "  CONNECTION TIMEOUT".red().bold());
        eprintln!("{}", "═══════════════════════════════════════════════════════".red());
        eprintln!();
        eprintln!("  Daemon at {} is not responding.", socket_path);
        eprintln!();
        eprintln!("  {} The daemon may be hung. Try restarting:", "→".cyan());
        eprintln!();
        eprintln!("    {}", "$ loopr daemon restart".green().bold());
        eprintln!();
        eprintln!("{}", "═══════════════════════════════════════════════════════".red());
        return Err(eyre!("Connection timeout. Daemon may be hung."));
    }

    // Daemon not running - auto-start it
    eprintln!("{} Daemon not running, starting automatically...", "⏳".yellow());

    // Get current executable path
    let exe = std::env::current_exe().context("Failed to get current executable")?;

    // Spawn daemon in background
    let child = Command::new(&exe)
        .args(["daemon", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to spawn daemon process")?;

    info!("Spawned daemon process (PID: {})", child.id());

    // Wait for daemon to be ready with exponential backoff
    for attempt in 1..=5 {
        let delay = Duration::from_millis(100 * (1 << attempt)); // 200ms, 400ms, 800ms, 1600ms, 3200ms
        tokio::time::sleep(delay).await;

        if Daemon::is_running(pid_path) {
            // Daemon is running, try to connect
            if app.connect().await.is_ok() {
                return Ok(true);
            }
        }
    }

    // Failed to connect after auto-start
    eprintln!();
    eprintln!("{}", "═══════════════════════════════════════════════════════".red());
    eprintln!("{}", "  FAILED TO AUTO-START DAEMON".red().bold());
    eprintln!("{}", "═══════════════════════════════════════════════════════".red());
    eprintln!();
    eprintln!("  Could not connect to daemon socket:");
    eprintln!("    {}", socket_path.yellow());
    eprintln!();
    eprintln!("  {} Try starting the daemon manually:", "→".cyan());
    eprintln!();
    eprintln!("    {}", "$ loopr daemon start".green().bold());
    eprintln!();
    eprintln!("{}", "═══════════════════════════════════════════════════════".red());

    Ok(false)
}

async fn run_tui(config: &Config) -> Result<()> {
    info!("Launching TUI mode");

    // 1. Create app and attempt daemon connection FIRST (before entering raw mode)
    let mut app = App::with_defaults();
    let socket_path = app.config.socket_path.display().to_string();
    let pid_path = default_pid_path();

    eprintln!("{} Connecting to daemon at {}...", "Loopr:".cyan(), &socket_path);

    // Try to connect, auto-starting daemon if needed
    let connected = try_connect_with_autostart(&mut app, &socket_path, &pid_path).await?;

    if !connected {
        return Err(eyre!("Could not connect to daemon after auto-start attempts"));
    }

    eprintln!("{} Connected to daemon", "Loopr:".green());

    // 2. Enable raw mode
    enable_raw_mode().context("Failed to enable raw mode")?;

    // 3. Setup terminal with alternate screen
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen).context("Failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;

    if config.debug {
        app.set_status("Debug mode enabled");
    }

    // Add welcome message
    app.add_chat_message(
        MessageSender::System,
        "Welcome to Loopr! Press Tab to switch views, Ctrl+C to quit.".to_string(),
    );

    // 4. Run event loop
    let result = run_event_loop(&mut terminal, &mut app).await;

    // 5. Restore terminal (always, even on error)
    disable_raw_mode().context("Failed to disable raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).context("Failed to leave alternate screen")?;

    result
}

/// Run the TUI event loop
async fn run_event_loop(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, app: &mut App) -> Result<()> {
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
                    Constraint::Length(3), // Tabs
                    Constraint::Min(0),    // Main content
                    Constraint::Length(1), // Status bar
                ])
                .split(size);

            // Render tabs
            let tab_titles: Vec<Line> = vec![Line::from(" Chat "), Line::from(" Loops "), Line::from(" Approval ")];
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
            let status_text = app
                .state
                .status_message
                .as_deref()
                .unwrap_or("Press Tab to switch views, Ctrl+C to quit");
            let status = ratatui::widgets::Paragraph::new(status_text).style(Style::default().fg(Color::DarkGray));
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
                            app.add_chat_message(MessageSender::User, msg.clone());

                            // Send message to daemon
                            if let Some(client) = app.client_mut() {
                                match client.chat_send(&msg).await {
                                    Ok(response) => {
                                        if let Some(result) = response.result {
                                            let reply = result
                                                .get("message")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or_else(|| {
                                                    result
                                                        .get("response")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("Message received")
                                                });
                                            app.add_chat_message(MessageSender::Daemon, reply.to_string());
                                        } else if let Some(error) = response.error {
                                            app.add_chat_message(
                                                MessageSender::Daemon,
                                                format!("Error: {}", error.message),
                                            );
                                        } else {
                                            app.add_chat_message(
                                                MessageSender::Daemon,
                                                "Message sent to daemon".to_string(),
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        app.add_chat_message(
                                            MessageSender::Daemon,
                                            format!("Failed to send: {}", e),
                                        );
                                    }
                                }
                            } else {
                                app.add_chat_message(
                                    MessageSender::Daemon,
                                    "Not connected to daemon".to_string(),
                                );
                            }
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

async fn handle_daemon_command(command: &DaemonCommands, config: &Config) -> Result<()> {
    info!("Handling daemon command: {:?}", command);
    if config.debug {
        println!("{}", "[debug] Daemon command handler".yellow());
    }
    match command {
        DaemonCommands::Start { foreground } => handle_daemon_start(*foreground).await,
        DaemonCommands::Stop => handle_daemon_stop(),
        DaemonCommands::Status => handle_daemon_status(),
        DaemonCommands::Restart => handle_daemon_restart().await,
    }
}

async fn handle_daemon_start(foreground: bool) -> Result<()> {
    let pid_path = default_pid_path();

    // Check if already running
    if Daemon::is_running(&pid_path) {
        let pid = Daemon::get_pid(&pid_path).unwrap_or(0);
        println!("{} Daemon is already running (PID: {})", "✓".green(), pid);
        return Ok(());
    }

    if foreground {
        println!("{}", "Starting daemon in foreground...".cyan());
        println!("Socket: {}", default_socket_path().display());
        println!("PID file: {}", pid_path.display());
        println!();

        // Run daemon directly in this process (we're already in an async context)
        let config = DaemonConfig::default();
        let mut daemon = Daemon::new(config).map_err(|e| eyre!("{}", e))?;
        daemon.run().await.map_err(|e| eyre!("{}", e))
    } else {
        println!("{}", "Starting daemon...".cyan());

        // Get current executable path
        let exe = std::env::current_exe().context("Failed to get current executable")?;

        // Spawn daemon in background with --foreground flag
        let child = Command::new(&exe)
            .args(["daemon", "start", "--foreground"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn daemon process")?;

        println!("Spawned daemon process (PID: {})", child.id());

        // Wait briefly and verify it's running
        std::thread::sleep(std::time::Duration::from_millis(500));

        if Daemon::is_running(&pid_path) {
            let pid = Daemon::get_pid(&pid_path).unwrap_or(0);
            println!("{} Daemon started successfully (PID: {})", "✓".green(), pid);
            println!("Socket: {}", default_socket_path().display());
        } else {
            return Err(eyre!("Daemon failed to start. Check logs for details."));
        }

        Ok(())
    }
}

fn handle_daemon_stop() -> Result<()> {
    let pid_path = default_pid_path();

    println!("{}", "Stopping daemon...".cyan());

    match Daemon::stop(&pid_path) {
        Ok(true) => {
            println!("{} Daemon stopped", "✓".green());
            Ok(())
        }
        Ok(false) => {
            println!("{} Daemon was not running", "⚠".yellow());
            Ok(())
        }
        Err(e) => Err(eyre!("Failed to stop daemon: {}", e)),
    }
}

fn handle_daemon_status() -> Result<()> {
    let pid_path = default_pid_path();
    let socket_path = default_socket_path();

    if Daemon::is_running(&pid_path) {
        let pid = Daemon::get_pid(&pid_path).unwrap_or(0);
        println!("{} Daemon is running", "●".green());
        println!("  PID: {}", pid);
        println!("  Socket: {}", socket_path.display());
        println!("  PID file: {}", pid_path.display());
    } else {
        println!("{} Daemon is not running", "○".red());
        println!("  Socket: {}", socket_path.display());
        println!("  PID file: {}", pid_path.display());

        // Check for stale PID file
        if pid_path.exists() {
            println!();
            println!("{} Stale PID file detected", "⚠".yellow());
            if let Some(pid) = Daemon::get_pid(&pid_path) {
                println!("  (references PID {} which is no longer running)", pid);
            }
        }
    }

    Ok(())
}

async fn handle_daemon_restart() -> Result<()> {
    println!("{}", "Restarting daemon...".cyan());

    // Stop if running
    let pid_path = default_pid_path();
    if Daemon::is_running(&pid_path) {
        handle_daemon_stop()?;
        // Give it a moment to fully shut down
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Start fresh
    handle_daemon_start(false).await
}

fn handle_plan_command(task: &str, config: &Config) -> Result<()> {
    info!("Creating plan for task: {}", task);
    if config.debug {
        println!("{}", "[debug] Plan command handler".yellow());
    }
    println!("{} Creating plan: {}", "Planning:".green(), task);
    println!("{}", "Plan creation not yet implemented".yellow());
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
    println!("{}", "Loop listing not yet implemented".yellow());
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
    println!("{}", "Status retrieval not yet implemented".yellow());
    Ok(())
}

fn handle_approve_command(id: &str, config: &Config) -> Result<()> {
    info!("Approving plan: {}", id);
    if config.debug {
        println!("{}", "[debug] Approve command handler".yellow());
    }
    println!("{} {}", "Approving:".green(), id);
    println!("{}", "Plan approval not yet implemented".yellow());
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
    println!("{}", "Plan rejection not yet implemented".yellow());
    Ok(())
}

fn handle_pause_command(id: &str, config: &Config) -> Result<()> {
    info!("Pausing loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Pause command handler".yellow());
    }
    println!("{} {}", "Pausing:".yellow(), id);
    println!("{}", "Loop pause not yet implemented".yellow());
    Ok(())
}

fn handle_resume_command(id: &str, config: &Config) -> Result<()> {
    info!("Resuming loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Resume command handler".yellow());
    }
    println!("{} {}", "Resuming:".green(), id);
    println!("{}", "Loop resume not yet implemented".yellow());
    Ok(())
}

fn handle_cancel_command(id: &str, config: &Config) -> Result<()> {
    info!("Canceling loop: {}", id);
    if config.debug {
        println!("{}", "[debug] Cancel command handler".yellow());
    }
    println!("{} {}", "Canceling:".red(), id);
    println!("{}", "Loop cancellation not yet implemented".yellow());
    Ok(())
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

    // Run the main application logic
    run_application(&cli, &config).await.context("Application failed")?;

    Ok(())
}
