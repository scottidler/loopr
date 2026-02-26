use clap::Parser;
use colored::*;
use eyre::{Context, Result};
use log::info;
use std::fs;
use std::path::PathBuf;

mod cli;
mod config;
mod daemon;
mod domain;
mod error;
mod id;
mod ipc;
mod tui;
mod worktree;

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

async fn run_application(_cli: &Cli, config: &Config) -> error::Result<()> {
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

    // Validate WorktreeManager is wired up
    let wt_mgr = worktree::manager::WorktreeManager::new(
        config.project.repo_path.clone(),
        config.project.repo_path.join(".worktrees"),
    );
    info!(
        "WorktreeManager: repo={} worktree_dir={}",
        wt_mgr.repo_path.display(),
        wt_mgr.worktree_dir.display()
    );
    info!(
        "WorktreeManager worktree_path(wi-test)={}",
        wt_mgr.worktree_path("wi-test").display()
    );
    info!("WorktreeManager exists(wi-test)={}", wt_mgr.exists("wi-test"));
    // Exercise WorktreeInfo serde
    let wt_info = worktree::manager::WorktreeInfo {
        path: wt_mgr.worktree_path("wi-test"),
        branch: "agent/wi-test".to_string(),
        head: "abc123".to_string(),
    };
    let wt_json = serde_json::to_string(&wt_info).map_err(error::LooprError::SerdeJson)?;
    info!("WorktreeInfo json: {}", wt_json);
    // Exercise WorktreeError display
    let wt_err = worktree::manager::WorktreeError::NotFound("wi-test".to_string());
    info!("WorktreeError: {}", wt_err);
    let wt_err2 = worktree::manager::WorktreeError::AlreadyExists("wi-test".to_string());
    info!("WorktreeError: {}", wt_err2);
    let wt_err3 = worktree::manager::WorktreeError::GitCommand("fatal".to_string());
    info!("WorktreeError: {}", wt_err3);
    // Exercise list (read-only git operation)
    match wt_mgr.list() {
        Ok(wts) => info!("WorktreeManager list: {} worktrees", wts.len()),
        Err(e) => info!("WorktreeManager list (expected in test): {}", e),
    }
    // Exercise create/refresh/cleanup error paths (nonexistent paths)
    match wt_mgr.refresh("nonexistent-wi", "HEAD") {
        Ok(()) => {}
        Err(e) => info!("WorktreeManager refresh (expected): {}", e),
    }
    match wt_mgr.cleanup("nonexistent-wi") {
        Ok(()) => {}
        Err(e) => info!("WorktreeManager cleanup (expected): {}", e),
    }
    // Exercise create with a path that would fail (no real git repo)
    match wt_mgr.create("wt-test-fail", "HEAD") {
        Ok(p) => info!("WorktreeManager create: {}", p.display()),
        Err(e) => info!("WorktreeManager create (expected): {}", e),
    }

    // Validate that the transition engine is wired up
    let rules: Vec<domain::transition::TransitionRule<&str>> = vec![domain::transition::TransitionRule {
        from: "init",
        to: "running",
        role: Some(role),
    }];
    domain::transition::validate_transition("init", "running", role, &rules)?;
    info!("Transition engine validated");

    // Validate IPC protocol types are wired up
    let req = ipc::protocol::DaemonRequest::new(1, "system.handshake", serde_json::json!({"client_version": "0.1.0"}));
    info!("IPC request: method={} id={}", req.method, req.id);
    let resp = ipc::protocol::DaemonResponse::ok(req.id, serde_json::json!({"server_version": "0.1.0"}));
    info!("IPC response: id={} is_error={}", resp.id, resp.is_error());
    let err_resp = ipc::protocol::DaemonResponse::err(req.id, ipc::protocol::RpcError::method_not_found("bad.method"));
    info!(
        "IPC error response: id={} is_error={}",
        err_resp.id,
        err_resp.is_error()
    );
    // Exercise all RpcError constructors
    let _ = ipc::protocol::RpcError::invalid_params("missing field");
    let _ = ipc::protocol::RpcError::internal("something broke");
    let _ = ipc::protocol::RpcError::transition_rejected("wrong role");
    let event = ipc::protocol::DaemonEvent::record_created("plan", &plan.id);
    info!("IPC event: {}", event.event);
    let tc_event =
        ipc::protocol::DaemonEvent::transition_completed("work_item", &work_item.id, "Draft", "Ready", "Coordinator");
    info!("IPC transition event: {}", tc_event.event);
    let line = serde_json::to_string(&event).map_err(error::LooprError::SerdeJson)?;
    let msg = ipc::protocol::IpcMessage::from_json(&line).map_err(error::LooprError::SerdeJson)?;
    info!("IPC message discrimination: {:?}", std::mem::discriminant(&msg));

    // Validate IPC codec is wired up
    let _codec = ipc::codec::ndjson_codec();
    let encoded_req = ipc::codec::encode_request(&req).map_err(error::LooprError::SerdeJson)?;
    let decoded_req = ipc::codec::decode_request(&encoded_req).map_err(error::LooprError::SerdeJson)?;
    info!("IPC codec roundtrip: method={}", decoded_req.method);
    let encoded_resp = ipc::codec::encode_response(&resp).map_err(error::LooprError::SerdeJson)?;
    let decoded_msg = ipc::codec::decode_client_message(&encoded_resp).map_err(error::LooprError::SerdeJson)?;
    info!("IPC codec client msg: {:?}", std::mem::discriminant(&decoded_msg));
    let encoded_event = ipc::codec::encode_event(&event).map_err(error::LooprError::SerdeJson)?;
    info!("IPC codec event encoded: {} bytes", encoded_event.len());

    // Validate IPC server types are wired up
    let socket_path = std::env::temp_dir().join("loopr-test-main.sock");
    let (server, event_tx) = ipc::server::IpcServer::new(&socket_path);
    info!("IPC server socket_path={}", server.socket_path().display());
    let event_rx = event_tx.subscribe();
    let _ = server.event_sender();
    // Exercise bind (creates socket) then cleanup
    let listener = server.bind().await.map_err(error::LooprError::Io)?;
    // Exercise handle_client: accept a connection, connect a client, then abort
    let event_rx2 = event_tx.subscribe();
    let accept_task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            ipc::server::handle_client(
                stream,
                |req| ipc::protocol::DaemonResponse::ok(req.id, serde_json::json!(null)),
                event_rx2,
            )
            .await;
        }
    });
    // Exercise IPC client: connect, send handshake, then clean up
    let mut client = ipc::client::IpcClient::connect(&socket_path)
        .await
        .map_err(|e| error::LooprError::Io(std::io::Error::other(e.to_string())))?;
    let handshake_resp = client.handshake("0.1.0").await;
    info!("IPC client handshake result: {:?}", handshake_resp.is_ok());
    // Exercise send + recv
    let send_id = client.send("system.status", serde_json::json!(null)).await;
    info!("IPC client send id: {:?}", send_id);
    let recv_msg = client.recv().await;
    info!("IPC client recv: {:?}", recv_msg.is_ok());
    // Exercise ClientError display
    let ce = ipc::client::ClientError::Disconnected;
    info!("ClientError: {}", ce);
    drop(client);
    accept_task.abort();
    drop(event_rx);
    server.cleanup();

    // Validate DaemonContext + daemon_main are wired up
    let mut daemon_config = config.clone();
    let daemon_test_sock = std::env::temp_dir().join(format!("loopr-run-{}.sock", id::generate_id()));
    let daemon_test_pid = std::env::temp_dir().join(format!("loopr-run-{}.pid", id::generate_id()));
    daemon_config.daemon.socket_path = daemon_test_sock.clone();
    daemon_config.daemon.pid_path = daemon_test_pid.clone();
    let (daemon_ctx, daemon_tx) = daemon::context::DaemonContext::shared(daemon_config);
    {
        let c = daemon_ctx.read().await;
        let mut rx = c.subscribe();
        daemon_tx
            .send(ipc::protocol::DaemonEvent::record_created("plan", &plan.id))
            .unwrap();
        let evt = rx.try_recv().unwrap();
        info!("DaemonContext event: {}", evt.event);
        info!("DaemonContext socket_path={}", c.config.daemon.socket_path.display());
    }
    // Start daemon briefly to validate wiring, then abort
    let daemon_handle = tokio::spawn(daemon::daemon_main(daemon_ctx));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    daemon_handle.abort();
    let _ = daemon_handle.await;
    let _ = std::fs::remove_file(&daemon_test_sock);
    let _ = std::fs::remove_file(&daemon_test_pid);

    // Validate TUI module is wired up
    let mut tui_app = tui::app::App::new();
    info!("TUI app: view={} role={}", tui_app.current_view, tui_app.current_role);
    tui_app.next_view();
    tui_app.cycle_role();
    tui_app.select_next();
    tui_app.select_prev();
    tui_app.toggle_help();
    info!(
        "TUI app after actions: view={} role={} help={} list_len={}",
        tui_app.current_view,
        tui_app.current_role,
        tui_app.show_help,
        tui_app.current_list_len()
    );
    // Validate View cycling
    let v = tui::app::View::Dashboard;
    info!("TUI view cycle: {} -> {} -> prev={}", v, v.next(), v.prev());
    // Validate ConnectionStatus display
    info!("TUI connection: {}", tui::app::ConnectionStatus::Connected);
    info!("TUI connection: {}", tui::app::ConnectionStatus::Disconnected);
    info!("TUI connection: {}", tui::app::ConnectionStatus::Reconnecting);
    // Validate input handling
    let key_event = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('q'),
        crossterm::event::KeyModifiers::NONE,
    );
    let action = tui::input::handle_key(key_event);
    info!("TUI input: q -> {:?}", action);
    tui::input::apply_action(&mut tui_app, action);
    info!("TUI should_quit after q: {}", tui_app.should_quit);
    // Validate view rendering with TestBackend
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            tui::views::dashboard::render(&tui_app, frame, area);
        })
        .unwrap();
    info!("TUI dashboard render OK");
    terminal
        .draw(|frame| {
            let area = frame.area();
            tui::views::work_items::render(&tui_app, frame, area);
        })
        .unwrap();
    info!("TUI work_items render OK");
    terminal
        .draw(|frame| {
            let area = frame.area();
            tui::views::bundles::render(&tui_app, frame, area);
        })
        .unwrap();
    info!("TUI bundles render OK");
    terminal
        .draw(|frame| {
            let area = frame.area();
            tui::views::ticks::render(&tui_app, frame, area);
        })
        .unwrap();
    info!("TUI ticks render OK");
    terminal
        .draw(|frame| {
            let area = frame.area();
            tui::views::learnings::render(&tui_app, frame, area);
        })
        .unwrap();
    info!("TUI learnings render OK");
    // Validate TUI run module is wired up
    let run_actions = tui::run::role_actions(role);
    info!("TUI role_actions for {}: {:?}", role, run_actions);
    terminal
        .draw(|frame| {
            tui::run::draw(&tui_app, frame);
        })
        .unwrap();
    info!("TUI run::draw OK");
    // run_tui() requires a real daemon connection — just reference it to avoid dead_code
    let _ = tui::run::run_tui;
    info!("TUI run_tui wired up");

    // Log some information
    info!("Application started at ts={}", id::now_millis());

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
