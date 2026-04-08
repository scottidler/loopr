use std::path::Path;

use eyre::{Context, Result, bail};

const GOAL_POLL_INTERVAL_SECS: u64 = 5;
use serde_json::json;
use tracing::warn;

use crate::clarity::{self, ClarityGate};
use crate::config::ClarityGateConfig;
use crate::domain::role::Role;
use crate::ipc::client::IpcClient;

use super::{AgentCmd, BundleCmd, Command, CoordinatorCmd, CrudCmd, LearningCmd, LockCmd, TickCmd, WorktreeCmd};

/// Connect to the daemon, send the IPC request for the given CLI command,
/// print the result, and exit.
pub async fn run(command: &Command, socket_path: &Path, role: Role, clarity_gate: &ClarityGateConfig) -> Result<()> {
    // Gap #6: `loopr role` writes to local config, doesn't need daemon
    if let Command::SetRole { role: role_str } = command {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("loopr");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::write(config_dir.join("role"), role_str)?;
        println!("Role set to: {role_str}");
        return Ok(());
    }

    // `loopr run` has its own flow - set goal, optionally accept plan, poll until done
    if let Command::Run {
        goal,
        timeout,
        plan,
        no_monitor,
        skip_clarity_gate,
    } = command
    {
        return run_headless(
            socket_path,
            goal,
            *timeout,
            plan.as_deref(),
            *no_monitor,
            *skip_clarity_gate,
            clarity_gate,
        )
        .await;
    }

    let mut client = IpcClient::connect(socket_path)
        .await
        .context("failed to connect to daemon — is it running?")?;

    // Perform handshake first
    let handshake = client.handshake(crate::version()).await;
    if let Err(e) = handshake {
        bail!("handshake failed: {e}");
    }

    let (method, params) = command_to_ipc(command, role);

    // Shutdown is fire-and-forget — the daemon may not respond
    if method == "system.shutdown" {
        client.send(&method, params).await.context("failed to send shutdown")?;
        println!("Shutdown signal sent.");
        return Ok(());
    }

    let (resp, _events) = client.request(&method, params).await.context("IPC request failed")?;

    if let Some(err) = resp.error {
        bail!("error ({}): {}", err.code, err.message);
    }

    // Pretty-print the result
    if let Some(result) = resp.result {
        let pretty = serde_json::to_string_pretty(&result)?;
        println!("{pretty}");
    }

    Ok(())
}

/// Map a CLI Command to an IPC (method, params) pair.
fn command_to_ipc(command: &Command, role: Role) -> (String, serde_json::Value) {
    match command {
        Command::Status => ("system.status".to_string(), json!(null)),

        Command::Shutdown => ("system.shutdown".to_string(), json!(null)),

        Command::Plan { cmd } => crud_to_ipc("plan", cmd, role),
        Command::Spec { cmd } => crud_to_ipc("spec", cmd, role),
        Command::Phase { cmd } => crud_to_ipc("phase", cmd, role),
        Command::Work { cmd } => crud_to_ipc("work", cmd, role),

        Command::Bundle { cmd } => bundle_to_ipc(cmd, role),
        Command::Tick { cmd } => tick_to_ipc(cmd, role),
        Command::Worktree { cmd } => worktree_to_ipc(cmd),
        Command::Learning { cmd } => learning_to_ipc(cmd),
        Command::Lock { cmd } => lock_to_ipc(cmd),

        Command::Agent { cmd } => agent_to_ipc(cmd),
        Command::Coordinator { cmd } => coordinator_to_ipc(cmd),

        Command::Init => ("system.init".to_string(), json!({})),
        Command::Validate { collection, id } => (
            "validator.validate".to_string(),
            json!({ "collection": collection, "id": id }),
        ),
        Command::Report { id } => ("validator.report".to_string(), json!({ "id": id })),
        Command::Reports { collection, target_id } => (
            "validator.reports".to_string(),
            json!({ "collection": collection, "target_id": target_id }),
        ),

        // SetRole writes to local config, not IPC — handled in dispatch_command
        Command::SetRole { .. } => unreachable!("role handled before IPC dispatch"),

        // Tui, Daemon, Diagnose, and Run are handled by main/run() before dispatch
        Command::Tui | Command::Daemon | Command::Diagnose { .. } | Command::Run { .. } => {
            unreachable!("tui/daemon/diagnose/run handled before dispatch")
        }
    }
}

/// Run a goal headlessly: set goal, optionally accept plan, poll until completion.
async fn run_headless(
    socket_path: &Path,
    goal: &str,
    timeout_secs: u64,
    plan_text: Option<&str>,
    no_monitor: bool,
    skip_clarity_gate: bool,
    clarity_gate_config: &ClarityGateConfig,
) -> Result<()> {
    // --- Goal Clarity Gate (fail-open) ---
    // Skip when: --plan provided, --skip-clarity-gate, or gate disabled in config
    let should_skip = plan_text.is_some() || skip_clarity_gate || !clarity_gate_config.enabled;
    if !should_skip {
        match ClarityGate::new(clarity_gate_config.clone()) {
            Ok(gate) => match gate.evaluate(goal).await {
                Ok(verdict) => {
                    if !verdict.passes(clarity_gate_config.min_score) {
                        eprint!("{}", clarity::format_failure(goal, &verdict));
                        std::process::exit(3);
                    }
                }
                Err(e) => {
                    warn!("Clarity gate evaluation failed, skipping: {e}");
                    eprintln!("Warning: clarity gate unavailable ({e}), proceeding anyway.");
                }
            },
            Err(e) => {
                warn!("Clarity gate init failed (API key missing?), skipping: {e}");
                eprintln!("Warning: clarity gate unavailable ({e}), proceeding anyway.");
            }
        }
    }

    let mut client = IpcClient::connect(socket_path)
        .await
        .context("failed to connect to daemon - is it running?")?;
    client.handshake(crate::version()).await.context("handshake failed")?;

    // Inject plan via doc.inject path. A .md plan file is required.
    let plan_path = match plan_text {
        Some(p) if p.ends_with(".md") => p,
        Some(p) => bail!("--plan must point to a .md plan file, got: {}", p),
        None => bail!("--plan <path.md> is required for headless mode"),
    };

    tracing::debug!(
        "run_headless: goal hint='{}' (ignored, title comes from plan .md)",
        goal
    );

    let (resp, _) = client
        .request("doc.inject", json!({ "path": plan_path }))
        .await
        .context("failed to inject plan")?;
    if let Some(err) = resp.error {
        bail!("doc.inject error: {}", err.message);
    }
    let goal_id = resp
        .result
        .as_ref()
        .and_then(|r| r.get("goal_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    eprintln!("Plan injected: {} (goal_id: {})", plan_path, goal_id);

    // Step 3: If no-monitor, exit now
    if no_monitor {
        eprintln!("Goal ID: {goal_id}");
        eprintln!("Use `loopr coordinator goal` to check status.");
        return Ok(());
    }

    // Step 4: Poll until terminal state or timeout
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let mut last_status = String::new();

    loop {
        if start.elapsed() > timeout {
            eprintln!("Timeout after {}s. Goal ID: {}", timeout_secs, goal_id);
            std::process::exit(1);
        }

        tokio::time::sleep(std::time::Duration::from_secs(GOAL_POLL_INTERVAL_SECS)).await;

        let poll_result = client.request("coordinator.get_state", json!({})).await;

        match poll_result {
            Ok((resp, _)) => {
                if let Some(err) = resp.error {
                    eprintln!("Poll error: {}", err.message);
                    continue;
                }
                if let Some(result) = &resp.result {
                    let status = result
                        .get("fsm_state")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    // No active coordinator state - check if goal is still active
                    if status.is_empty() {
                        if let Ok((gr, _)) = client.request("coordinator.get_goal", json!({})).await {
                            let goal_active = gr
                                .result
                                .as_ref()
                                .and_then(|r| r.get("active"))
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            if !goal_active {
                                eprintln!("Goal complete.");
                                std::process::exit(0);
                            }
                        }
                        if last_status != "waiting" {
                            let now = chrono::Local::now().format("%H:%M:%S");
                            eprintln!("[{now}] Waiting for coordinator...");
                            last_status = "waiting".to_string();
                        }
                        continue;
                    }

                    if status != last_status {
                        let now = chrono::Local::now().format("%H:%M:%S");
                        eprintln!("[{now}] Coordinator: {status}");
                        last_status = status.clone();
                    }

                    // Terminal states
                    if status == "GoalComplete" {
                        eprintln!("Goal complete.");
                        std::process::exit(0);
                    }
                    if status == "NeedHelp" {
                        eprintln!("Coordinator needs help. Goal ID: {}", goal_id);
                        std::process::exit(2);
                    }
                }
            }
            Err(e) => {
                eprintln!("Connection lost: {e}. Retrying...");
                // Try to reconnect
                if let Ok(new_client) = IpcClient::connect(socket_path).await {
                    client = new_client;
                    let _ = client.handshake(crate::version()).await;
                }
            }
        }
    }
}

/// Map CrudCmd to IPC for Plan/Spec/Phase/Work.
fn crud_to_ipc(collection: &str, cmd: &CrudCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        CrudCmd::Create {
            title,
            description,
            parent,
            order,
            files,
            acceptance_criteria,
            dependencies,
        } => {
            let mut params = json!({
                "title": title,
                "description": description,
            });
            if let Some(parent_id) = parent {
                params["parent_id"] = json!(parent_id);
            }
            if let Some(order) = order {
                params["order"] = json!(order);
            }
            // Work-specific fields
            if collection == "work" {
                if !files.is_empty() {
                    params["files"] = json!(files);
                }
                if !acceptance_criteria.is_empty() {
                    params["acceptance_criteria"] = json!(acceptance_criteria);
                }
                if !dependencies.is_empty() {
                    params["dependencies"] = json!(dependencies);
                }
            }
            (format!("{collection}.create"), params)
        }
        CrudCmd::Get { id } => (format!("{collection}.get"), json!({ "id": id })),
        CrudCmd::List { parent } => {
            let mut params = json!({});
            if let Some(parent_id) = parent {
                params["parent_id"] = json!(parent_id);
            }
            (format!("{collection}.list"), params)
        }
        CrudCmd::Transition {
            id,
            status,
            skip_validation,
        } => {
            // Plan/Spec/Phase use serde(rename_all = "lowercase"), so normalize
            let normalized_status = match collection {
                "plan" | "spec" | "phase" => status.to_lowercase(),
                _ => status.clone(),
            };
            let mut params = json!({
                "id": id,
                "target_status": normalized_status,
                "role": role.to_string(),
            });
            if *skip_validation {
                params["skip_validation"] = json!(true);
            }
            (format!("{collection}.transition"), params)
        }
    }
}

fn bundle_to_ipc(cmd: &BundleCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        BundleCmd::Create {
            work_id,
            branch,
            description,
            base_tick_id,
            claims,
            touched_paths,
        } => {
            let mut params = json!({
                "work_id": work_id,
                "branch_name": branch,
                "description": description,
            });
            if let Some(tick_id) = base_tick_id {
                params["base_tick_id"] = json!(tick_id);
            }
            if !claims.is_empty() {
                params["claims"] = json!(claims);
            }
            if !touched_paths.is_empty() {
                params["touched_paths"] = json!(touched_paths);
            }
            ("bundle.create".to_string(), params)
        }
        BundleCmd::Get { id } => ("bundle.get".to_string(), json!({ "id": id })),
        BundleCmd::List { work_id } => {
            let mut params = json!({});
            if let Some(wi_id) = work_id {
                params["work_id"] = json!(wi_id);
            }
            ("bundle.list".to_string(), params)
        }
        BundleCmd::Transition { id, status } => (
            "bundle.transition".to_string(),
            json!({
                "id": id,
                "target_status": status,
                "role": role.to_string(),
            }),
        ),
    }
}

fn tick_to_ipc(cmd: &TickCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        TickCmd::Create { number } => ("tick.create".to_string(), json!({ "number": number })),
        TickCmd::Get { id } => ("tick.get".to_string(), json!({ "id": id })),
        TickCmd::List { status } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            ("tick.list".to_string(), params)
        }
        TickCmd::Transition { id, status } => (
            "tick.transition".to_string(),
            json!({
                "id": id,
                "target_status": status,
                "role": role.to_string(),
            }),
        ),
        TickCmd::Validate { id } => ("integrator.validate".to_string(), json!({ "tick_id": id })),
        TickCmd::Publish { id } => ("integrator.publish".to_string(), json!({ "tick_id": id })),
    }
}

fn worktree_to_ipc(cmd: &WorktreeCmd) -> (String, serde_json::Value) {
    match cmd {
        WorktreeCmd::Create { work_id, git_ref } => (
            "worktree.create".to_string(),
            json!({ "work_id": work_id, "ref": git_ref }),
        ),
        WorktreeCmd::List => ("worktree.list".to_string(), json!(null)),
        WorktreeCmd::Cleanup { work_id } => ("worktree.cleanup".to_string(), json!({ "work_id": work_id })),
        WorktreeCmd::Refresh { work_id, git_ref } => (
            "worktree.refresh".to_string(),
            json!({ "work_id": work_id, "ref": git_ref }),
        ),
    }
}

fn learning_to_ipc(cmd: &LearningCmd) -> (String, serde_json::Value) {
    match cmd {
        LearningCmd::Create {
            source_id,
            scope,
            content,
        } => (
            "learning.create".to_string(),
            json!({ "source_id": source_id, "scope": scope, "content": content }),
        ),
        LearningCmd::Get { id } => ("learning.get".to_string(), json!({ "id": id })),
        LearningCmd::List => ("learning.list".to_string(), json!(null)),
        LearningCmd::Reinforce { id } => ("learning.reinforce".to_string(), json!({ "id": id })),
        LearningCmd::Contradict { id } => ("learning.contradict".to_string(), json!({ "id": id })),
        LearningCmd::Promote { id } => ("learning.promote".to_string(), json!({ "id": id })),
        LearningCmd::Demote { id } => ("learning.demote".to_string(), json!({ "id": id })),
    }
}

fn agent_to_ipc(cmd: &AgentCmd) -> (String, serde_json::Value) {
    match cmd {
        AgentCmd::StartImplementer { work_id } => (
            "agent.start".to_string(),
            json!({ "agent_type": "implementer", "work_id": work_id }),
        ),
        AgentCmd::StartReviewer { bundle_id } => (
            "agent.start".to_string(),
            json!({ "agent_type": "reviewer", "bundle_id": bundle_id }),
        ),
        AgentCmd::StartCoordinator => ("agent.start".to_string(), json!({ "agent_type": "coordinator" })),
        AgentCmd::StartIntegrator => ("agent.start".to_string(), json!({ "agent_type": "integrator" })),
        AgentCmd::StartResearcher { query, target_id } => {
            let mut params = json!({ "agent_type": "researcher", "query": query });
            if let Some(tid) = target_id {
                params["target_id"] = json!(tid);
            }
            ("agent.start".to_string(), params)
        }
        AgentCmd::Stop { session_id } => ("agent.stop".to_string(), json!({ "session_id": session_id })),
        AgentCmd::Pause { session_id } => ("agent.pause".to_string(), json!({ "session_id": session_id })),
        AgentCmd::Resume { session_id } => ("agent.resume".to_string(), json!({ "session_id": session_id })),
        AgentCmd::Status { session_id } => ("agent.status".to_string(), json!({ "session_id": session_id })),
        AgentCmd::List { status, agent_type } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            if let Some(t) = agent_type {
                params["agent_type"] = json!(t);
            }
            ("agent.list".to_string(), params)
        }
        AgentCmd::Output { session_id, since } => (
            "agent.output".to_string(),
            json!({ "session_id": session_id, "since": since }),
        ),
    }
}

fn coordinator_to_ipc(cmd: &CoordinatorCmd) -> (String, serde_json::Value) {
    match cmd {
        CoordinatorCmd::Status => ("coordinator.get_goal".to_string(), json!({})),
    }
}

fn lock_to_ipc(cmd: &LockCmd) -> (String, serde_json::Value) {
    match cmd {
        LockCmd::Create {
            resource,
            holder_id,
            granted_by,
        } => (
            "lock.create".to_string(),
            json!({ "resource": resource, "holder_id": holder_id, "granted_by": granted_by }),
        ),
        LockCmd::Get { id } => ("lock.get".to_string(), json!({ "id": id })),
        LockCmd::List {
            resource,
            holder_id,
            active_only,
        } => {
            let mut params = json!({});
            if let Some(r) = resource {
                params["resource"] = json!(r);
            }
            if let Some(h) = holder_id {
                params["holder_id"] = json!(h);
            }
            if *active_only {
                params["active_only"] = json!(true);
            }
            ("lock.list".to_string(), params)
        }
        LockCmd::Release { id } => ("lock.release".to_string(), json!({ "id": id })),
        LockCmd::Expire { id } => ("lock.expire".to_string(), json!({ "id": id })),
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
