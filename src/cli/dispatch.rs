use std::path::Path;

use eyre::{Context, Result, bail};
use serde_json::json;

use crate::domain::role::Role;
use crate::ipc::client::IpcClient;

use super::{AgentCmd, BundleCmd, Command, CoordinatorCmd, CrudCmd, LearningCmd, LockCmd, TickCmd, WorktreeCmd};

/// Connect to the daemon, send the IPC request for the given CLI command,
/// print the result, and exit.
pub async fn run(command: &Command, socket_path: &Path, role: Role) -> Result<()> {
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
    } = command
    {
        return run_headless(socket_path, goal, *timeout, plan.as_deref(), *no_monitor).await;
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
) -> Result<()> {
    let mut client = IpcClient::connect(socket_path)
        .await
        .context("failed to connect to daemon - is it running?")?;
    client.handshake(crate::version()).await.context("handshake failed")?;

    // Step 1: Set the goal
    let (resp, _) = client
        .request("coordinator.set_goal", json!({ "goal": goal }))
        .await
        .context("failed to set goal")?;
    if let Some(err) = resp.error {
        bail!("set_goal error: {}", err.message);
    }
    let goal_id = resp
        .result
        .as_ref()
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    eprintln!("Goal set: {} ({})", goal, goal_id);

    // Step 2: If plan text provided, accept it directly
    if let Some(plan) = plan_text {
        let (resp, _) = client
            .request("coordinator.accept_plan", json!({ "plan": plan }))
            .await
            .context("failed to accept plan")?;
        if let Some(err) = resp.error {
            bail!("accept_plan error: {}", err.message);
        }
        eprintln!("Plan accepted, orchestration starting.");
    }

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

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

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
            resource_tags,
            acceptance_criteria,
            dependencies,
        } => {
            let mut params = json!({
                "title": title,
                "description": description,
            });
            if let Some(parent_id) = parent {
                // Use the correct parent key based on collection
                let key = match collection {
                    "spec" => "plan_id",
                    "phase" => "spec_id",
                    "work" => "phase_id",
                    _ => "parent_id",
                };
                params[key] = json!(parent_id);
            }
            if let Some(order) = order {
                params["order"] = json!(order);
            }
            // Work-specific fields
            if collection == "work" {
                if !resource_tags.is_empty() {
                    params["resource_tags"] = json!(resource_tags);
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
                let key = match collection {
                    "spec" => "plan_id",
                    "phase" => "spec_id",
                    "work" => "phase_id",
                    _ => "parent_id",
                };
                params[key] = json!(parent_id);
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
        CoordinatorCmd::Set { goal } => ("coordinator.set_goal".to_string(), json!({ "goal": goal })),
        CoordinatorCmd::Clear => ("coordinator.clear_goal".to_string(), json!({})),
        CoordinatorCmd::Status => ("coordinator.get_goal".to_string(), json!({})),
        CoordinatorCmd::AcceptPlan { plan } => ("coordinator.accept_plan".to_string(), json!({ "plan": plan })),
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
mod tests {
    use super::*;

    #[test]
    fn test_plan_create_mapping() {
        let cmd = Command::Plan {
            cmd: CrudCmd::Create {
                title: "My Plan".to_string(),
                description: "A test".to_string(),
                parent: None,
                order: None,
                resource_tags: vec![],
                acceptance_criteria: vec![],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "plan.create");
        assert_eq!(params["title"], "My Plan");
        assert_eq!(params["description"], "A test");
    }

    #[test]
    fn test_spec_create_mapping_with_parent() {
        let cmd = Command::Spec {
            cmd: CrudCmd::Create {
                title: "My Spec".to_string(),
                description: "".to_string(),
                parent: Some("plan-1".to_string()),
                order: None,
                resource_tags: vec![],
                acceptance_criteria: vec![],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "spec.create");
        assert_eq!(params["plan_id"], "plan-1");
    }

    #[test]
    fn test_phase_create_mapping_with_order() {
        let cmd = Command::Phase {
            cmd: CrudCmd::Create {
                title: "Phase 1".to_string(),
                description: "".to_string(),
                parent: Some("spec-1".to_string()),
                order: Some(1),
                resource_tags: vec![],
                acceptance_criteria: vec![],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "phase.create");
        assert_eq!(params["spec_id"], "spec-1");
        assert_eq!(params["order"], 1);
    }

    #[test]
    fn test_work_transition_mapping() {
        let cmd = Command::Work {
            cmd: CrudCmd::Transition {
                id: "wi-1".to_string(),
                status: "Ready".to_string(),
                skip_validation: false,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "work.transition");
        assert_eq!(params["id"], "wi-1");
        assert_eq!(params["target_status"], "Ready");
        assert_eq!(params["role"], "coordinator");
    }

    #[test]
    fn test_bundle_create_mapping() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::Create {
                work_id: "wi-1".to_string(),
                branch: "feature/foo".to_string(),
                description: "A bundle".to_string(),
                base_tick_id: Some("tick-1".to_string()),
                claims: vec![],
                touched_paths: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.create");
        assert_eq!(params["work_id"], "wi-1");
        assert_eq!(params["branch_name"], "feature/foo");
        assert_eq!(params["base_tick_id"], "tick-1");
    }

    #[test]
    fn test_tick_validate_mapping() {
        let cmd = Command::Tick {
            cmd: TickCmd::Validate { id: "t-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "integrator.validate");
        assert_eq!(params["tick_id"], "t-1");
    }

    #[test]
    fn test_tick_publish_mapping() {
        let cmd = Command::Tick {
            cmd: TickCmd::Publish { id: "t-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "integrator.publish");
        assert_eq!(params["tick_id"], "t-1");
    }

    #[test]
    fn test_worktree_create_mapping() {
        let cmd = Command::Worktree {
            cmd: WorktreeCmd::Create {
                work_id: "wi-1".to_string(),
                git_ref: "main".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "worktree.create");
        assert_eq!(params["work_id"], "wi-1");
        assert_eq!(params["ref"], "main");
    }

    #[test]
    fn test_worktree_list_mapping() {
        let cmd = Command::Worktree { cmd: WorktreeCmd::List };
        let (method, _params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "worktree.list");
    }

    #[test]
    fn test_learning_reinforce_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Reinforce { id: "l-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.reinforce");
        assert_eq!(params["id"], "l-1");
    }

    #[test]
    fn test_lock_list_with_filters() {
        let cmd = Command::Lock {
            cmd: LockCmd::List {
                resource: Some("src/main.rs".to_string()),
                holder_id: None,
                active_only: true,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.list");
        assert_eq!(params["resource"], "src/main.rs");
        assert_eq!(params["active_only"], true);
    }

    #[test]
    fn test_status_mapping() {
        let cmd = Command::Status;
        let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "system.status");
    }

    #[test]
    fn test_shutdown_mapping() {
        let cmd = Command::Shutdown;
        let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "system.shutdown");
    }

    #[test]
    fn test_get_and_list_mappings() {
        let cmd = Command::Plan {
            cmd: CrudCmd::Get { id: "p-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "plan.get");
        assert_eq!(params["id"], "p-1");

        let cmd = Command::Plan {
            cmd: CrudCmd::List { parent: None },
        };
        let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "plan.list");
    }

    #[test]
    fn test_transition_role_is_serde_compatible() {
        // Regression: dispatch must emit role values that handlers can deserialize.
        use crate::domain::role::Role;
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            let cmd = Command::Plan {
                cmd: CrudCmd::Transition {
                    id: "p-1".to_string(),
                    status: "active".to_string(),
                    skip_validation: false,
                },
            };
            let (_, params) = command_to_ipc(&cmd, role);
            let role_str = params["role"].as_str().unwrap();
            let quoted = format!("\"{}\"", role_str);
            let parsed: Role = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
            assert_eq!(role, parsed);
        }
    }

    #[test]
    fn test_plan_transition_normalizes_status_to_lowercase() {
        // Regression: plan/spec/phase use serde(rename_all = "lowercase"),
        // so dispatch must lowercase the user-provided status string.
        for input in ["Active", "ACTIVE", "active", "AcTiVe"] {
            let cmd = Command::Plan {
                cmd: CrudCmd::Transition {
                    id: "p-1".to_string(),
                    status: input.to_string(),
                    skip_validation: false,
                },
            };
            let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
            assert_eq!(
                params["target_status"], "active",
                "input '{}' should normalize to 'active'",
                input
            );
        }
    }

    #[test]
    fn test_spec_transition_normalizes_status_to_lowercase() {
        let cmd = Command::Spec {
            cmd: CrudCmd::Transition {
                id: "s-1".to_string(),
                status: "Active".to_string(),
                skip_validation: false,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(params["target_status"], "active");
    }

    #[test]
    fn test_phase_transition_normalizes_status_to_lowercase() {
        let cmd = Command::Phase {
            cmd: CrudCmd::Transition {
                id: "ph-1".to_string(),
                status: "Complete".to_string(),
                skip_validation: false,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(params["target_status"], "complete");
    }

    #[test]
    fn test_work_transition_preserves_status_casing() {
        // WorkStatus uses default serde (PascalCase), so dispatch must NOT lowercase.
        let cmd = Command::Work {
            cmd: CrudCmd::Transition {
                id: "wi-1".to_string(),
                status: "InProgress".to_string(),
                skip_validation: false,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(params["target_status"], "InProgress");
    }

    #[test]
    fn test_bundle_transition_role_is_serde_compatible() {
        for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
            let cmd = Command::Bundle {
                cmd: BundleCmd::Transition {
                    id: "b-1".to_string(),
                    status: "Triaged".to_string(),
                },
            };
            let (_, params) = command_to_ipc(&cmd, role);
            let role_str = params["role"].as_str().unwrap();
            let quoted = format!("\"{}\"", role_str);
            let parsed: Role = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
            assert_eq!(role, parsed);
        }
    }

    #[test]
    fn test_tick_transition_role_is_serde_compatible() {
        let cmd = Command::Tick {
            cmd: TickCmd::Transition {
                id: "t-1".to_string(),
                status: "Sealing".to_string(),
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Integrator);
        let role_str = params["role"].as_str().unwrap();
        let quoted = format!("\"{}\"", role_str);
        let parsed: Role =
            serde_json::from_str(&quoted).unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
        assert_eq!(Role::Integrator, parsed);
    }

    #[test]
    fn test_init_mapping() {
        let cmd = Command::Init;
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "system.init");
        assert_eq!(params, json!({}));
    }

    #[test]
    fn test_validate_mapping() {
        let cmd = Command::Validate {
            collection: "plan".to_string(),
            id: "plan-1".to_string(),
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "validator.validate");
        assert_eq!(params["collection"], "plan");
        assert_eq!(params["id"], "plan-1");
    }

    #[test]
    fn test_report_mapping() {
        let cmd = Command::Report { id: "vr-1".to_string() };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "validator.report");
        assert_eq!(params["id"], "vr-1");
    }

    #[test]
    fn test_reports_mapping() {
        let cmd = Command::Reports {
            collection: "plans".to_string(),
            target_id: "plan-1".to_string(),
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "validator.reports");
        assert_eq!(params["collection"], "plans");
        assert_eq!(params["target_id"], "plan-1");
    }

    #[test]
    fn test_agent_start_implementer_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::StartImplementer {
                work_id: "wi-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.start");
        assert_eq!(params["agent_type"], "implementer");
        assert_eq!(params["work_id"], "wi-1");
    }

    #[test]
    fn test_agent_start_reviewer_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::StartReviewer {
                bundle_id: "b-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.start");
        assert_eq!(params["agent_type"], "reviewer");
        assert_eq!(params["bundle_id"], "b-1");
    }

    #[test]
    fn test_agent_stop_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Stop {
                session_id: "sess-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.stop");
        assert_eq!(params["session_id"], "sess-1");
    }

    #[test]
    fn test_agent_pause_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Pause {
                session_id: "sess-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.pause");
        assert_eq!(params["session_id"], "sess-1");
    }

    #[test]
    fn test_agent_resume_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Resume {
                session_id: "sess-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.resume");
        assert_eq!(params["session_id"], "sess-1");
    }

    #[test]
    fn test_agent_status_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Status {
                session_id: "sess-1".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.status");
        assert_eq!(params["session_id"], "sess-1");
    }

    #[test]
    fn test_agent_list_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::List {
                status: None,
                agent_type: None,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.list");
        assert_eq!(params, json!({}));
    }

    #[test]
    fn test_agent_list_with_filters_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::List {
                status: Some("running".to_string()),
                agent_type: Some("implementer".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.list");
        assert_eq!(params["status"], "running");
        assert_eq!(params["agent_type"], "implementer");
    }

    #[test]
    fn test_agent_start_coordinator_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::StartCoordinator,
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.start");
        assert_eq!(params["agent_type"], "coordinator");
    }

    #[test]
    fn test_agent_start_researcher_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::StartResearcher {
                query: "How does auth work?".to_string(),
                target_id: Some("wi-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.start");
        assert_eq!(params["agent_type"], "researcher");
        assert_eq!(params["query"], "How does auth work?");
        assert_eq!(params["target_id"], "wi-1");
    }

    #[test]
    fn test_agent_start_researcher_no_target() {
        let cmd = Command::Agent {
            cmd: AgentCmd::StartResearcher {
                query: "test".to_string(),
                target_id: None,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert!(params["target_id"].is_null());
    }

    #[test]
    fn test_coordinator_set_goal_mapping() {
        let cmd = Command::Coordinator {
            cmd: CoordinatorCmd::Set {
                goal: "Build auth".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "coordinator.set_goal");
        assert_eq!(params["goal"], "Build auth");
    }

    #[test]
    fn test_coordinator_clear_goal_mapping() {
        let cmd = Command::Coordinator {
            cmd: CoordinatorCmd::Clear,
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "coordinator.clear_goal");
        assert_eq!(params, json!({}));
    }

    #[test]
    fn test_coordinator_get_goal_mapping() {
        let cmd = Command::Coordinator {
            cmd: CoordinatorCmd::Status,
        };
        let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "coordinator.get_goal");
    }

    #[test]
    fn test_crud_spec_with_parent_and_order() {
        // Spec create with both parent (plan_id) and order set
        let cmd = Command::Spec {
            cmd: CrudCmd::Create {
                title: "Auth Spec".to_string(),
                description: "JWT tokens".to_string(),
                parent: Some("plan-42".to_string()),
                order: Some(3),
                resource_tags: vec![],
                acceptance_criteria: vec![],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "spec.create");
        assert_eq!(params["title"], "Auth Spec");
        assert_eq!(params["description"], "JWT tokens");
        assert_eq!(params["plan_id"], "plan-42");
        assert_eq!(params["order"], 3);
    }

    #[test]
    fn test_tick_list_with_status_filter() {
        let cmd = Command::Tick {
            cmd: TickCmd::List {
                status: Some("Published".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "tick.list");
        assert_eq!(params["status"], "Published");
    }

    #[test]
    fn test_lock_list_multiple_filters() {
        // All three filters set: resource, holder_id, and active_only
        let cmd = Command::Lock {
            cmd: LockCmd::List {
                resource: Some("src/lib.rs".to_string()),
                holder_id: Some("wi-7".to_string()),
                active_only: true,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.list");
        assert_eq!(params["resource"], "src/lib.rs");
        assert_eq!(params["holder_id"], "wi-7");
        assert_eq!(params["active_only"], true);
    }

    #[test]
    fn test_bundle_create_with_base_tick_id() {
        // Bundle create without base_tick_id — key should be absent
        let cmd = Command::Bundle {
            cmd: BundleCmd::Create {
                work_id: "wi-5".to_string(),
                branch: "feat/bar".to_string(),
                description: "No tick".to_string(),
                base_tick_id: None,
                claims: vec![],
                touched_paths: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.create");
        assert_eq!(params["work_id"], "wi-5");
        assert_eq!(params["branch_name"], "feat/bar");
        assert_eq!(params["description"], "No tick");
        assert!(params.get("base_tick_id").is_none() || params["base_tick_id"].is_null());
    }

    #[test]
    fn test_worktree_cleanup_mapping() {
        let cmd = Command::Worktree {
            cmd: WorktreeCmd::Cleanup {
                work_id: "wi-cleanup".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "worktree.cleanup");
        assert_eq!(params["work_id"], "wi-cleanup");
    }

    #[test]
    fn test_worktree_refresh_mapping() {
        let cmd = Command::Worktree {
            cmd: WorktreeCmd::Refresh {
                work_id: "wi-refresh".to_string(),
                git_ref: "main".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "worktree.refresh");
        assert_eq!(params["work_id"], "wi-refresh");
        assert_eq!(params["ref"], "main");
    }

    #[test]
    fn test_agent_start_with_all_optional_params() {
        // StartResearcher with target_id set — all optional params populated
        let cmd = Command::Agent {
            cmd: AgentCmd::StartResearcher {
                query: "What API patterns exist?".to_string(),
                target_id: Some("spec-9".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.start");
        assert_eq!(params["agent_type"], "researcher");
        assert_eq!(params["query"], "What API patterns exist?");
        assert_eq!(params["target_id"], "spec-9");

        // Also test integrator start (no optional params — different path)
        let cmd2 = Command::Agent {
            cmd: AgentCmd::StartIntegrator,
        };
        let (method2, params2) = command_to_ipc(&cmd2, Role::Integrator);
        assert_eq!(method2, "agent.start");
        assert_eq!(params2["agent_type"], "integrator");
    }

    #[test]
    fn test_coordinator_goal_clear_mapping() {
        let cmd = Command::Coordinator {
            cmd: CoordinatorCmd::Clear,
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "coordinator.clear_goal");
        assert_eq!(params, json!({}));
        // Verify no extra fields are present
        assert!(params.as_object().unwrap().is_empty());
    }

    // --- Coverage gap tests for uncovered branches ---

    #[test]
    fn test_work_create_with_parent_uses_phase_id() {
        let cmd = Command::Work {
            cmd: CrudCmd::Create {
                title: "Implement auth".to_string(),
                description: "JWT".to_string(),
                parent: Some("phase-1".to_string()),
                order: None,
                resource_tags: vec!["src/".to_string()],
                acceptance_criteria: vec!["tests pass".to_string()],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "work.create");
        assert_eq!(params["phase_id"], "phase-1");
        assert_eq!(params["resource_tags"], json!(["src/"]));
        assert_eq!(params["acceptance_criteria"], json!(["tests pass"]));
    }

    #[test]
    fn test_work_create_skips_work_fields_for_non_work() {
        // resource_tags should NOT appear in plan.create params
        let cmd = Command::Plan {
            cmd: CrudCmd::Create {
                title: "Plan".to_string(),
                description: "".to_string(),
                parent: None,
                order: None,
                resource_tags: vec!["should-be-ignored".to_string()],
                acceptance_criteria: vec![],
                dependencies: vec![],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "plan.create");
        assert!(params.get("resource_tags").is_none());
    }

    #[test]
    fn test_bundle_create_with_claims_and_touched_paths() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::Create {
                work_id: "wi-1".to_string(),
                branch: "feature/auth".to_string(),
                description: "".to_string(),
                base_tick_id: None,
                claims: vec!["Add JWT".to_string()],
                touched_paths: vec!["src/auth.rs".to_string()],
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.create");
        assert_eq!(params["claims"], json!(["Add JWT"]));
        assert_eq!(params["touched_paths"], json!(["src/auth.rs"]));
    }

    #[test]
    fn test_spec_list_with_parent_uses_plan_id() {
        let cmd = Command::Spec {
            cmd: CrudCmd::List {
                parent: Some("plan-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "spec.list");
        assert_eq!(params["plan_id"], "plan-1");
    }

    #[test]
    fn test_phase_list_with_parent_uses_spec_id() {
        let cmd = Command::Phase {
            cmd: CrudCmd::List {
                parent: Some("spec-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "phase.list");
        assert_eq!(params["spec_id"], "spec-1");
    }

    #[test]
    fn test_work_list_with_parent_uses_phase_id() {
        let cmd = Command::Work {
            cmd: CrudCmd::List {
                parent: Some("phase-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "work.list");
        assert_eq!(params["phase_id"], "phase-1");
    }

    #[test]
    fn test_transition_skip_validation_flag() {
        let cmd = Command::Plan {
            cmd: CrudCmd::Transition {
                id: "p-1".to_string(),
                status: "active".to_string(),
                skip_validation: true,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(params["skip_validation"], true);
    }

    #[test]
    fn test_bundle_get_mapping() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::Get { id: "b-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.get");
        assert_eq!(params["id"], "b-1");
    }

    #[test]
    fn test_bundle_list_with_work_filter() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::List {
                work_id: Some("wi-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.list");
        assert_eq!(params["work_id"], "wi-1");
    }

    #[test]
    fn test_bundle_list_no_filter() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::List { work_id: None },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.list");
        assert_eq!(params, json!({}));
    }

    #[test]
    fn test_tick_create_mapping() {
        let cmd = Command::Tick {
            cmd: TickCmd::Create { number: 42 },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "tick.create");
        assert_eq!(params["number"], 42);
    }

    #[test]
    fn test_tick_get_mapping() {
        let cmd = Command::Tick {
            cmd: TickCmd::Get { id: "t-5".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "tick.get");
        assert_eq!(params["id"], "t-5");
    }

    #[test]
    fn test_learning_create_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Create {
                source_id: "wi-1".to_string(),
                scope: "global".to_string(),
                content: "Always use snake_case".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.create");
        assert_eq!(params["source_id"], "wi-1");
        assert_eq!(params["scope"], "global");
        assert_eq!(params["content"], "Always use snake_case");
    }

    #[test]
    fn test_learning_get_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Get { id: "l-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.get");
        assert_eq!(params["id"], "l-1");
    }

    #[test]
    fn test_learning_list_mapping() {
        let cmd = Command::Learning { cmd: LearningCmd::List };
        let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.list");
    }

    #[test]
    fn test_learning_contradict_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Contradict { id: "l-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.contradict");
        assert_eq!(params["id"], "l-1");
    }

    #[test]
    fn test_learning_promote_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Promote { id: "l-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.promote");
        assert_eq!(params["id"], "l-1");
    }

    #[test]
    fn test_learning_demote_mapping() {
        let cmd = Command::Learning {
            cmd: LearningCmd::Demote { id: "l-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "learning.demote");
        assert_eq!(params["id"], "l-1");
    }

    #[test]
    fn test_lock_create_mapping() {
        let cmd = Command::Lock {
            cmd: LockCmd::Create {
                resource: "src/main.rs".to_string(),
                holder_id: "wi-1".to_string(),
                granted_by: "coordinator".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.create");
        assert_eq!(params["resource"], "src/main.rs");
        assert_eq!(params["holder_id"], "wi-1");
        assert_eq!(params["granted_by"], "coordinator");
    }

    #[test]
    fn test_lock_get_mapping() {
        let cmd = Command::Lock {
            cmd: LockCmd::Get { id: "lk-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.get");
        assert_eq!(params["id"], "lk-1");
    }

    #[test]
    fn test_lock_release_mapping() {
        let cmd = Command::Lock {
            cmd: LockCmd::Release { id: "lk-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.release");
        assert_eq!(params["id"], "lk-1");
    }

    #[test]
    fn test_lock_expire_mapping() {
        let cmd = Command::Lock {
            cmd: LockCmd::Expire { id: "lk-1".to_string() },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.expire");
        assert_eq!(params["id"], "lk-1");
    }

    #[test]
    fn test_tick_list_no_filter() {
        let cmd = Command::Tick {
            cmd: TickCmd::List { status: None },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Integrator);
        assert_eq!(method, "tick.list");
        assert_eq!(params, json!({}));
    }

    #[test]
    fn test_lock_list_holder_id_only() {
        let cmd = Command::Lock {
            cmd: LockCmd::List {
                resource: None,
                holder_id: Some("wi-3".to_string()),
                active_only: false,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "lock.list");
        assert_eq!(params["holder_id"], "wi-3");
        assert!(params.get("resource").is_none() || params["resource"].is_null());
        assert!(params.get("active_only").is_none() || params["active_only"].is_null());
    }

    #[test]
    fn test_agent_output_mapping() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Output {
                session_id: "sess-42".to_string(),
                since: 5,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.output");
        assert_eq!(params["session_id"], "sess-42");
        assert_eq!(params["since"], 5);
    }

    #[test]
    fn test_agent_output_default_since() {
        let cmd = Command::Agent {
            cmd: AgentCmd::Output {
                session_id: "sess-1".to_string(),
                since: 0,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "agent.output");
        assert_eq!(params["session_id"], "sess-1");
        assert_eq!(params["since"], 0);
    }

    #[test]
    fn test_coordinator_accept_plan_mapping() {
        let cmd = Command::Coordinator {
            cmd: CoordinatorCmd::AcceptPlan {
                plan: "Build auth module".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "coordinator.accept_plan");
        assert_eq!(params["plan"], "Build auth module");
    }
}
