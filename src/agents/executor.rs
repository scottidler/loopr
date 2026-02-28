use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use log::{error, info, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::coordinator;
use crate::agents::implementer::{self, LlmClient};
use crate::agents::integrator_task;
use crate::agents::llm_client::AgentLlmClient;
use crate::agents::researcher;
use crate::agents::reviewer;
use crate::agents::{AgentAction, AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::{ToolResult, ToolRunner};
use crate::worktree::manager::WorktreeManager;

/// Normalize a collection name from plural to singular for IPC method dispatch.
/// The LLM may emit "plans", "specs", "phases", "work_items", "bundles", "ticks"
/// but IPC methods use singular: "plan", "spec", "phase", "work_item", "bundle", "tick".
fn normalize_collection(collection: &str) -> &str {
    match collection {
        "plans" => "plan",
        "specs" => "spec",
        "phases" => "phase",
        "work_items" => "work_item",
        "bundles" => "bundle",
        "ticks" => "tick",
        other => other,
    }
}

/// Create the LLM client. Fails if the API key env var is not set.
fn create_llm_client(
    config: &crate::config::AgentRoleConfig,
    session_id: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<Box<dyn LlmClient>> {
    let client = AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone())?;
    info!("Agent {} using LLM client (model: {})", session_id, config.model);
    Ok(Box::new(client))
}

/// Run an agent task as a Tokio task. This is spawned from the agent.start handler.
///
/// Currently implements a minimal lifecycle: Starting → Running → Completed/Failed.
/// Phase 2 will add the full Implementer/Reviewer loops with LLM calls.
pub async fn run_agent_task(
    session_id: String,
    agent_type: AgentType,
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
) {
    info!("Agent task started: {} ({})", session_id, agent_type);

    // Create a worktree for the agent before starting the loop.
    // Thinking plane agents (Coordinator, Researcher, Integrator) don't use worktrees.
    // Implementers key on work_item_id, Reviewers on bundle_id.
    let worktree_key = if agent_type.is_thinking_plane() {
        None
    } else {
        let sessions = stores.agent_sessions.read().unwrap();
        let session = match sessions.get(&session_id) {
            Some(s) => s,
            None => {
                error!("Agent {} session not found in stores", session_id);
                return;
            }
        };
        match agent_type {
            AgentType::Implementer => session.work_item_id.clone(),
            AgentType::Reviewer => session.bundle_id.clone(),
            // Already handled by is_thinking_plane() above
            AgentType::Coordinator | AgentType::Researcher | AgentType::Integrator => None,
        }
    };

    if let Some(ref key) = worktree_key {
        let worktree_path = match worktree_mgr.create(key, "HEAD") {
            Ok(path) => Some(path),
            Err(crate::worktree::manager::WorktreeError::AlreadyExists(_)) => {
                info!("Agent {} worktree already exists for {}", session_id, key);
                Some(worktree_mgr.worktree_path(key))
            }
            Err(e) => {
                warn!("Agent {} failed to create worktree: {}", session_id, e);
                None
            }
        };

        if let Some(ref path) = worktree_path {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(session) = sessions.get_mut(&session_id) {
                session.worktree_path = Some(path.to_string_lossy().to_string());
                persist_session(&stores, session);
            }
            info!("Agent {} worktree created at {}", session_id, path.display());
        }
    }

    // Retain a copy of the worktree manager for cleanup after the agent loop exits.
    // The original is moved into the bridge.
    let cleanup_mgr = worktree_mgr.clone();
    let cleanup_key = worktree_key.clone();

    // Create the in-process IPC bridge for this agent
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    // Transition to Running
    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(AgentStatus::Running) {
                error!("Agent {} failed to start: {}", session_id, e);
                return;
            }
            persist_session(&stores, session);
        }
    }
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

    let result = if agent_type == AgentType::Coordinator {
        let max_restarts = 3u32;
        let restart_delay = stores.config.agents.coordinator.idle_interval_secs * 2;
        let mut attempt = 0u32;
        let mut result = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx).await;

        while result.is_err() && attempt < max_restarts {
            attempt += 1;
            warn!(
                "Coordinator {} failed (attempt {}/{}), restarting in {}s",
                session_id, attempt, max_restarts, restart_delay
            );
            tokio::time::sleep(Duration::from_secs(restart_delay)).await;
            // Check cancellation during sleep
            let cancelled = stores
                .agent_sessions
                .read()
                .unwrap()
                .get(&session_id)
                .is_some_and(|s| s.status == AgentStatus::Cancelled);
            if cancelled {
                break;
            }
            result = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx).await;
        }
        result
    } else {
        run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx).await
    };

    // Clean up worktree after agent loop exits (only for non-thinking-plane agents)
    if let Some(ref key) = cleanup_key {
        if let Err(e) = cleanup_mgr.cleanup(key) {
            warn!("Worktree cleanup failed for {} (key={}): {}", session_id, key, e);
        } else {
            info!("Worktree cleaned up for {} (key={})", session_id, key);
        }
    }

    // Transition to terminal state based on result
    let terminal_status = match result {
        Ok(_) => AgentStatus::Completed,
        Err(ref e) => {
            warn!("Agent {} failed: {}", session_id, e);
            AgentStatus::Failed
        }
    };

    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(terminal_status) {
                error!(
                    "Agent {} failed to transition to {:?}: {}",
                    session_id, terminal_status, e
                );
                return;
            }
            if let Err(ref e) = result {
                session.error_message = Some(e.to_string());
            }
            persist_session(&stores, session);
        }
    }
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, terminal_status));

    info!(
        "Agent task finished: {} ({}) → {:?}",
        session_id, agent_type, terminal_status
    );
}

/// Agent loop — dispatches to the appropriate agent implementation based on type.
async fn run_agent_loop(
    session_id: &str,
    agent_type: AgentType,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    // Verify the bridge works by checking system status
    let status_resp = bridge.request("system.status", serde_json::json!(null));
    if status_resp.is_error() {
        return Err(eyre!("bridge health check failed: {:?}", status_resp.error));
    }
    info!("Agent {} bridge health check passed", session_id);

    match agent_type {
        AgentType::Implementer => {
            let tool_runner = &stores.tool_runner;
            let config = stores.config.agents.implementer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;

            // Clone session out for the implementer loop
            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = implementer::run_implementer(
                llm.as_ref(),
                &mut session,
                stores,
                tool_runner,
                bridge,
                &config,
                event_tx,
            )
            .await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Reviewer => {
            let config = stores.config.agents.reviewer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;

            // Clone session out for the reviewer loop
            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = reviewer::run_reviewer(llm.as_ref(), &mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Coordinator => {
            let config = stores.config.agents.coordinator.clone();
            let llm = create_llm_client(&config.role, session_id, event_tx)?;

            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result =
                coordinator::run_coordinator(llm.as_ref(), &mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Researcher => {
            let config = stores.config.agents.researcher.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;

            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result =
                researcher::run_researcher(llm.as_ref(), &mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
        AgentType::Integrator => {
            let config = stores.config.integrator.clone();

            let mut session = {
                let sessions = stores.agent_sessions.read().unwrap();
                sessions
                    .get(session_id)
                    .ok_or_else(|| eyre!("session not found: {}", session_id))?
                    .clone()
            };

            let result = integrator_task::run_integrator(&mut session, stores, bridge, &config, event_tx).await;

            // Write back updated session iteration count
            {
                let mut sessions = stores.agent_sessions.write().unwrap();
                if let Some(s) = sessions.get_mut(session_id) {
                    s.iteration = session.iteration;
                }
            }

            result
        }
    }
}

/// Execute a single agent action. Used by the agent loop to process parsed LLM responses.
///
/// `agent_type` is used for role inference on Transition actions (when role is None).
pub async fn execute_action(
    action: &AgentAction,
    tool_runner: &ToolRunner,
    bridge: &AgentIpcBridge,
    worktree_path: &Path,
    work_item_id: Option<&str>,
    agent_type: AgentType,
) -> Result<ActionResult> {
    match action {
        AgentAction::RunTool { tool_name, args } => {
            let tool_result = tool_runner
                .run(tool_name, args, worktree_path)
                .await
                .map_err(|e| eyre!("run_tool '{}': {}", tool_name, e))?;
            Ok(ActionResult::ToolRun(tool_result))
        }
        AgentAction::WriteFile { path, content } => {
            // Validate path stays within worktree (sandbox)
            let full_path = worktree_path.join(path);
            let canonical = full_path.canonicalize().unwrap_or_else(|_| full_path.clone());
            let worktree_canonical = worktree_path
                .canonicalize()
                .unwrap_or_else(|_| worktree_path.to_path_buf());
            if !canonical.starts_with(&worktree_canonical) {
                return Err(eyre!("path escapes worktree: {}", path));
            }

            // Advisory lock check: under LockStrict, reject writes to locked resources
            if bridge.config().strategy.conflict_policy == crate::config::ConflictPolicy::LockStrict {
                let lock_resp = bridge.request(
                    "lock.list",
                    serde_json::json!({ "resource": path, "active_only": true }),
                );
                if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array())
                    && !locks.is_empty()
                {
                    let holder = locks[0].get("holder_id").and_then(|v| v.as_str()).unwrap_or("unknown");
                    return Ok(ActionResult::ActionError(format!(
                        "file '{}' locked by {} (policy: LockStrict)",
                        path, holder
                    )));
                }
            }

            if let Some(parent) = full_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| eyre!("write_file mkdir '{}': {}", path, e))?;
            }
            tokio::fs::write(&full_path, content)
                .await
                .map_err(|e| eyre!("write_file '{}': {}", path, e))?;
            Ok(ActionResult::FileWritten(path.clone()))
        }
        AgentAction::ReadFile { path } => {
            let full_path = worktree_path.join(path);
            let content = tokio::fs::read_to_string(&full_path)
                .await
                .map_err(|e| eyre!("read_file '{}': {}", path, e))?;
            Ok(ActionResult::FileRead(content))
        }
        AgentAction::Commit { message, paths } => {
            // Stage specified paths (or all changes if empty)
            let add_args = if paths.is_empty() { vec!["-A".to_string()] } else { paths.clone() };
            let mut add_cmd = tokio::process::Command::new("git");
            add_cmd.arg("add").args(&add_args).current_dir(worktree_path);
            let add_out = add_cmd.output().await?;
            if !add_out.status.success() {
                let stderr = String::from_utf8_lossy(&add_out.stderr);
                return Err(eyre!("git add failed: {}", stderr));
            }

            // Commit
            let mut commit_cmd = tokio::process::Command::new("git");
            commit_cmd.args(["commit", "-m", message]).current_dir(worktree_path);
            let commit_out = commit_cmd.output().await?;
            if !commit_out.status.success() {
                let stderr = String::from_utf8_lossy(&commit_out.stderr);
                return Err(eyre!("git commit failed: {}", stderr));
            }
            Ok(ActionResult::Committed(message.clone()))
        }
        AgentAction::ProposeBundle { description, claims } => {
            // Get the current branch name from the worktree
            let mut branch_cmd = tokio::process::Command::new("git");
            branch_cmd
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(worktree_path);
            let branch_out = branch_cmd.output().await?;
            let branch_name = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();

            let wi_id = work_item_id.ok_or_else(|| eyre!("propose_bundle requires work_item_id"))?;
            let resp = bridge.request(
                "bundle.create",
                serde_json::json!({
                    "work_item_id": wi_id,
                    "branch_name": branch_name,
                    "claims": claims,
                    "description": description,
                }),
            );
            if resp.is_error() {
                return Err(eyre!("propose bundle failed: {:?}", resp.error));
            }
            Ok(ActionResult::BundleProposed(description.clone()))
        }
        AgentAction::Transition {
            collection,
            id,
            target_status,
            role,
        } => {
            // If role is not specified, infer from agent_type
            let effective_role = role
                .as_ref()
                .map(|r| r.to_string())
                .unwrap_or_else(|| agent_type.default_role().to_string());
            let params = serde_json::json!({ "id": id, "target_status": target_status, "role": effective_role });
            let method = format!("{}.transition", normalize_collection(collection));
            let resp = bridge.request(&method, params);
            if resp.is_error() {
                return Err(eyre!("transition failed: {:?}", resp.error));
            }
            Ok(ActionResult::Transitioned(format!(
                "{}/{} → {}",
                collection, id, target_status
            )))
        }
        AgentAction::CreateLearning {
            content,
            scope,
            source_id,
            applicable_roles,
            resource_tags,
        } => {
            let mut params = serde_json::json!({
                "content": content,
                "scope": scope,
                "source_id": source_id,
            });
            if let Some(roles) = applicable_roles {
                params["applicable_roles"] = serde_json::json!(roles);
            }
            if let Some(tags) = resource_tags {
                params["resource_tags"] = serde_json::json!(tags);
            }
            let resp = bridge.request("learning.create", params);
            if resp.is_error() {
                return Err(eyre!("create learning failed: {:?}", resp.error));
            }
            Ok(ActionResult::LearningCreated(content.clone()))
        }
        AgentAction::Done { summary } => Ok(ActionResult::Done(summary.clone())),
        AgentAction::NeedHelp { reason } => Ok(ActionResult::NeedHelp(reason.clone())),

        // --- Coordinator document-creation actions ---
        AgentAction::CreatePlan {
            title,
            description,
            acceptance_criteria,
        } => {
            // Gap #27: Draft-awareness guard
            let list_resp = bridge.request("plan.list", serde_json::json!({}));
            if let Some(plans) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
                let has_draft = plans.iter().any(|p| {
                    p.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase()) == Some("draft".to_string())
                });
                if has_draft {
                    return Ok(ActionResult::ActionError(
                        "A Draft Plan already exists. Iterate on the existing Draft instead of creating a new one."
                            .into(),
                    ));
                }
            }
            let resp = bridge.request(
                "plan.create",
                serde_json::json!({
                    "title": title,
                    "description": description,
                    "acceptance_criteria": acceptance_criteria,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("create_plan failed: {}", msg)));
            }
            let id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("CreatePlan: {} (id: {})", title, id);
            Ok(ActionResult::RecordCreated {
                collection: "plans".to_string(),
                id,
            })
        }
        AgentAction::CreateSpec {
            plan_id,
            title,
            description,
        } => {
            // Gap #27: Draft-awareness guard for specs under same plan
            let list_resp = bridge.request("spec.list", serde_json::json!({}));
            if let Some(specs) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
                let has_draft = specs.iter().any(|s| {
                    s.get("plan_id").and_then(|v| v.as_str()) == Some(plan_id)
                        && s.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase())
                            == Some("draft".to_string())
                });
                if has_draft {
                    return Ok(ActionResult::ActionError(
                        "A Draft Spec already exists under this Plan. Iterate on the existing Draft.".into(),
                    ));
                }
            }
            let resp = bridge.request(
                "spec.create",
                serde_json::json!({
                    "plan_id": plan_id,
                    "title": title,
                    "description": description,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("create_spec failed: {}", msg)));
            }
            let id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("CreateSpec: {} (id: {})", title, id);
            Ok(ActionResult::RecordCreated {
                collection: "specs".to_string(),
                id,
            })
        }
        AgentAction::CreatePhase {
            spec_id,
            title,
            description,
            order,
        } => {
            // Gap #27: Draft-awareness guard for phases under same spec
            let list_resp = bridge.request("phase.list", serde_json::json!({}));
            if let Some(phases) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
                let has_draft = phases.iter().any(|p| {
                    p.get("spec_id").and_then(|v| v.as_str()) == Some(spec_id)
                        && p.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase())
                            == Some("draft".to_string())
                });
                if has_draft {
                    return Ok(ActionResult::ActionError(
                        "A Draft Phase already exists under this Spec. Iterate on the existing Draft.".into(),
                    ));
                }
            }
            let resp = bridge.request(
                "phase.create",
                serde_json::json!({
                    "spec_id": spec_id,
                    "title": title,
                    "description": description,
                    "order": order,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("create_phase failed: {}", msg)));
            }
            let id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("CreatePhase: {} (id: {})", title, id);
            Ok(ActionResult::RecordCreated {
                collection: "phases".to_string(),
                id,
            })
        }
        AgentAction::CreateWorkItem {
            phase_id,
            title,
            description,
            resource_tags,
            acceptance_criteria,
        } => {
            // Gap #27: Draft-awareness guard for work items under same phase
            let list_resp = bridge.request("work_item.list", serde_json::json!({}));
            if let Some(items) = list_resp.result.as_ref().and_then(|v| v.as_array()) {
                let has_draft = items.iter().any(|w| {
                    w.get("phase_id").and_then(|v| v.as_str()) == Some(phase_id)
                        && w.get("status").and_then(|v| v.as_str()).map(|s| s.to_lowercase())
                            == Some("draft".to_string())
                });
                if has_draft {
                    return Ok(ActionResult::ActionError(
                        "A Draft WorkItem already exists under this Phase. Iterate on the existing Draft.".into(),
                    ));
                }
            }
            let resp = bridge.request(
                "work_item.create",
                serde_json::json!({
                    "phase_id": phase_id,
                    "title": title,
                    "description": description,
                    "resource_tags": resource_tags,
                    "acceptance_criteria": acceptance_criteria,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("create_work_item failed: {}", msg)));
            }
            let id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("CreateWorkItem: {} (id: {})", title, id);
            Ok(ActionResult::RecordCreated {
                collection: "work_items".to_string(),
                id,
            })
        }

        // --- Coordinator agent management actions ---
        AgentAction::AssignAgent { agent_type, target_id } => {
            // For implementer assignments, auto-transition work item to InProgress if needed
            if agent_type == "implementer" {
                let get_resp = bridge.request("work_item.get", serde_json::json!({ "id": target_id }));
                let wi_status = get_resp
                    .result
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                match wi_status {
                    "Draft" => {
                        // Draft → Ready → InProgress (assignee set on InProgress transition)
                        let r1 = bridge.request(
                            "work_item.transition",
                            serde_json::json!({ "id": target_id, "target_status": "Ready", "role": "coordinator" }),
                        );
                        if r1.is_error() {
                            let msg = r1.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Ok(ActionResult::ActionError(format!(
                                "auto-transition Draft→Ready failed: {}",
                                msg
                            )));
                        }
                        let r2 = bridge.request(
                            "work_item.transition",
                            serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type }),
                        );
                        if r2.is_error() {
                            let msg = r2.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Ok(ActionResult::ActionError(format!(
                                "auto-transition Ready→InProgress failed: {}",
                                msg
                            )));
                        }
                        info!(
                            "AssignAgent: auto-transitioned work item {} Draft→Ready→InProgress",
                            target_id
                        );
                    }
                    "Ready" => {
                        // Ready → InProgress
                        let r = bridge.request(
                            "work_item.transition",
                            serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type }),
                        );
                        if r.is_error() {
                            let msg = r.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Ok(ActionResult::ActionError(format!(
                                "auto-transition Ready→InProgress failed: {}",
                                msg
                            )));
                        }
                        info!(
                            "AssignAgent: auto-transitioned work item {} Ready→InProgress",
                            target_id
                        );
                    }
                    "InProgress" => {
                        // Already correct
                    }
                    other => {
                        return Ok(ActionResult::ActionError(format!(
                            "cannot assign implementer to work item {} in state '{}'",
                            target_id, other
                        )));
                    }
                }
            }

            // Map target_id to the correct param based on agent type
            let mut params = serde_json::json!({ "agent_type": agent_type });
            match agent_type.as_str() {
                "implementer" => {
                    params["work_item_id"] = serde_json::json!(target_id);
                }
                "reviewer" => {
                    params["bundle_id"] = serde_json::json!(target_id);
                }
                _ => {
                    params["target_id"] = serde_json::json!(target_id);
                }
            }
            let resp = bridge.request("agent.start", params);
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("assign_agent failed: {}", msg)));
            }
            let session_id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("AssignAgent: {} → {} (session: {})", agent_type, target_id, session_id);
            Ok(ActionResult::AgentSpawned {
                session_id,
                agent_type: agent_type.clone(),
            })
        }
        AgentAction::SpawnResearcher { query, scope_id } => {
            let resp = bridge.request(
                "agent.start",
                serde_json::json!({
                    "agent_type": "researcher",
                    "target_id": scope_id,
                    "query": query,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("spawn_researcher failed: {}", msg)));
            }
            let session_id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!(
                "SpawnResearcher: {} (scope: {}, session: {})",
                query, scope_id, session_id
            );
            Ok(ActionResult::AgentSpawned {
                session_id,
                agent_type: "researcher".to_string(),
            })
        }
        AgentAction::ValidateDocument { collection, id } => {
            let resp = bridge.request(
                "validator.validate",
                serde_json::json!({ "collection": collection, "id": id }),
            );
            if resp.is_error() {
                let msg = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "unknown validation error".to_string());
                return Ok(ActionResult::ActionError(format!(
                    "validate_document {}/{} failed: {}",
                    collection, id, msg
                )));
            }
            let verdict = resp
                .result
                .as_ref()
                .and_then(|v| v.get("verdict"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let summary = resp
                .result
                .as_ref()
                .and_then(|v| v.get("summary"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let issues: Vec<String> = resp
                .result
                .as_ref()
                .and_then(|v| v.get("issues"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|issue| issue.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            info!(
                "ValidateDocument {}/{}: verdict={}, issues={}",
                collection,
                id,
                verdict,
                issues.len()
            );
            Ok(ActionResult::DocumentValidated {
                verdict,
                summary,
                issues,
            })
        }
        AgentAction::AcquireLock { resource, holder_id } => {
            // Check if there's already an active lock on this resource
            let check_resp = bridge.request(
                "lock.list",
                serde_json::json!({ "resource": resource, "active_only": true }),
            );
            if check_resp.is_error() {
                return Err(eyre!("lock.list failed: {:?}", check_resp.error));
            }
            if let Some(result) = &check_resp.result
                && let Some(locks) = result.as_array()
                && !locks.is_empty()
            {
                // Resource already locked — return as ActionError so the LLM can self-correct
                let existing_holder = locks[0]
                    .get("holder_id")
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or("unknown");
                let existing_id = locks[0]
                    .get("id")
                    .and_then(|v: &serde_json::Value| v.as_str())
                    .unwrap_or("unknown");
                return Ok(ActionResult::ActionError(format!(
                    "resource '{}' already locked by {} (lock_id: {})",
                    resource, existing_holder, existing_id
                )));
            }

            // Create the lock — granted_by is the holder_id (self-granted by coordinator)
            let resp = bridge.request(
                "lock.create",
                serde_json::json!({
                    "resource": resource,
                    "holder_id": holder_id,
                    "granted_by": holder_id,
                }),
            );
            if resp.is_error() {
                return Err(eyre!("lock.create failed: {:?}", resp.error));
            }
            let lock_id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            info!("Lock acquired: {} on '{}' for {}", lock_id, resource, holder_id);
            Ok(ActionResult::LockAcquired(lock_id))
        }
        AgentAction::ReleaseLock { lock_id } => {
            let resp = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
            if resp.is_error() {
                return Err(eyre!("lock.release failed: {:?}", resp.error));
            }
            info!("Lock released: {}", lock_id);
            Ok(ActionResult::LockReleased(lock_id.clone()))
        }
        AgentAction::TriageBundle { bundle_id } => {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Triaged",
                    "role": "coordinator",
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("triage_bundle failed: {}", msg)));
            }
            info!("TriageBundle: {} → Triaged", bundle_id);
            Ok(ActionResult::Transitioned(format!("bundle/{} → Triaged", bundle_id)))
        }
        AgentAction::AcceptBundle { bundle_id } => {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Accepted",
                    "role": "coordinator",
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("accept_bundle failed: {}", msg)));
            }
            info!("AcceptBundle: {} → Accepted", bundle_id);
            Ok(ActionResult::Transitioned(format!("bundle/{} → Accepted", bundle_id)))
        }

        // --- Researcher actions (wired in Phase 4 researcher.rs) ---
        AgentAction::SearchCode { pattern, glob, path } => {
            let repo_root = worktree_path; // For Researcher, worktree_path is the repo root
            match researcher::execute_search_code(repo_root, pattern, glob.as_deref(), path.as_deref()).await {
                Ok(output) => Ok(ActionResult::FileRead(output)),
                Err(e) => Ok(ActionResult::ActionError(e.to_string())),
            }
        }
        AgentAction::SearchFiles { pattern, path } => {
            let repo_root = worktree_path;
            match researcher::execute_search_files(repo_root, pattern, path.as_deref()).await {
                Ok(output) => Ok(ActionResult::FileRead(output)),
                Err(e) => Ok(ActionResult::ActionError(e.to_string())),
            }
        }
        AgentAction::ListDirectory { path } => {
            let repo_root = worktree_path;
            match researcher::execute_list_directory(repo_root, path).await {
                Ok(output) => Ok(ActionResult::FileRead(output)),
                Err(e) => Ok(ActionResult::ActionError(e.to_string())),
            }
        }
    }
}

/// Result of executing a single agent action.
#[derive(Debug)]
pub enum ActionResult {
    ToolRun(ToolResult),
    FileWritten(String),
    FileRead(String),
    Committed(String),
    BundleProposed(String),
    Transitioned(String),
    LearningCreated(String),
    /// Lock acquired — contains lock_id.
    LockAcquired(String),
    /// Lock released — contains lock_id.
    LockReleased(String),
    /// Document validated — contains (verdict, summary, issue_messages).
    DocumentValidated {
        verdict: String,
        summary: String,
        issues: Vec<String>,
    },
    Done(String),
    NeedHelp(String),
    /// Non-fatal error — fed back to the LLM so it can self-correct.
    ActionError(String),
    /// Record created via bridge — contains (collection, id).
    RecordCreated {
        collection: String,
        id: String,
    },
    /// Agent session spawned — contains (session_id, agent_type).
    AgentSpawned {
        session_id: String,
        agent_type: String,
    },
}

fn persist_session(stores: &Stores, session: &AgentSession) {
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(session.clone())
    {
        warn!("Failed to persist agent session {} to TaskStore: {}", session.id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig, ToolEntry};
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    fn test_stores(dir: &Path) -> Arc<Stores> {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    /// Helper: create a full Plan→Spec→Phase→WorkItem hierarchy in stores via bridge.
    fn create_test_hierarchy(bridge: &AgentIpcBridge) -> (String, String, String, String) {
        let plan_resp = bridge.request(
            "plan.create",
            serde_json::json!({"title": "Test Plan", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let plan_id = plan_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "plan.transition",
            serde_json::json!({"id": plan_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let spec_resp = bridge.request(
            "spec.create",
            serde_json::json!({"plan_id": plan_id, "title": "Test Spec", "description": "desc"}),
        );
        let spec_id = spec_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "spec.transition",
            serde_json::json!({"id": spec_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let phase_resp = bridge.request(
            "phase.create",
            serde_json::json!({"spec_id": spec_id, "title": "Test Phase", "description": "desc", "order": 1}),
        );
        let phase_id = phase_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "phase.transition",
            serde_json::json!({"id": phase_id, "target_status": "active", "role": "coordinator", "skip_validation": true}),
        );
        let wi_resp = bridge.request(
            "work_item.create",
            serde_json::json!({"phase_id": phase_id, "title": "Test WI", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id, wi_id)
    }

    #[tokio::test]
    async fn test_transition_action_uses_correct_param() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-transparam-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        // Transition Draft → Ready via execute_action
        let action = AgentAction::Transition {
            collection: "work_item".to_string(),
            id: wi_id.clone(),
            target_status: "Ready".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();

        // Should succeed (not "target_status required" error)
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_auto_transitions_draft() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-autotrans-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        // Work item starts as Draft — AssignAgent should auto-transition to InProgress
        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();

        // The agent.start may fail (no LLM key) but the auto-transition should have happened
        // Check work item status is InProgress
        let wi_resp = bridge.request("work_item.get", serde_json::json!({"id": wi_id}));
        let wi_status = wi_resp
            .result
            .as_ref()
            .unwrap()
            .get("status")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(
            wi_status, "InProgress",
            "work item should be InProgress after auto-transition"
        );

        // The overall result should be either AgentSpawned or ActionError (if agent.start failed due to no LLM)
        // but the transition should have succeeded regardless
        assert!(
            matches!(result, ActionResult::AgentSpawned { .. } | ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_action_run_tool() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-test-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let entries = vec![ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let runner = ToolRunner::new(&entries);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::RunTool {
            tool_name: "echo-test".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        if let ActionResult::ToolRun(tool_result) = result {
            assert_eq!(tool_result.exit_code, 0);
            assert_eq!(tool_result.stdout.trim(), "hello");
        } else {
            panic!("expected ToolRun result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_write_file() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-write-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "hello world".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));

        let content = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_file_lock_strict_blocks() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = std::env::temp_dir().join(format!("loopr-exec-lockstrict-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let bridge = AgentIpcBridge::new(stores, event_tx, worktree_mgr, config);

        // Create a lock on "src/main.rs"
        let lock_resp = bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        // Attempt to write to locked path
        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_lock_advisory_allows() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-lockadvisory-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        // Default config has LockAdvisory
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        // Create a lock on "src/main.rs"
        let lock_resp = bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        // Write should succeed under advisory policy
        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "should work".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
    }

    #[tokio::test]
    async fn test_execute_action_read_file() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-read-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("read-me.txt"), "file content").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::ReadFile {
            path: "read-me.txt".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        if let ActionResult::FileRead(content) = result {
            assert_eq!(content, "file content");
        } else {
            panic!("expected FileRead result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_done() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-done-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::Done {
            summary: "All done".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        if let ActionResult::Done(summary) = result {
            assert_eq!(summary, "All done");
        } else {
            panic!("expected Done result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_need_help() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-help-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::NeedHelp {
            reason: "Ambiguous spec".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        if let ActionResult::NeedHelp(reason) = result {
            assert_eq!(reason, "Ambiguous spec");
        } else {
            panic!("expected NeedHelp result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_unknown_tool() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-unk-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::RunTool {
            tool_name: "nonexistent".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-acqlock-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-123".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::LockAcquired(lock_id) = &result {
            assert!(!lock_id.is_empty());
            assert_ne!(lock_id, "unknown");
        } else {
            panic!("expected LockAcquired result, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock_conflict() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-lockconf-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        // Acquire first lock
        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-100".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::LockAcquired(_)));

        // Try to acquire again on same resource — should get ActionError
        let action2 = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-200".to_string(),
        };
        let result2 = execute_action(&action2, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::ActionError(msg) = &result2 {
            assert!(
                msg.contains("already locked"),
                "expected conflict message, got: {}",
                msg
            );
        } else {
            panic!("expected ActionError for lock conflict, got {:?}", result2);
        }
    }

    #[tokio::test]
    async fn test_execute_action_release_lock() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-rellock-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        // Acquire a lock first
        let acquire_action = AgentAction::AcquireLock {
            resource: "src/lib.rs".to_string(),
            holder_id: "wi-456".to_string(),
        };
        let acquire_result = execute_action(&acquire_action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = acquire_result {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release it
        let release_action = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        let release_result = execute_action(&release_action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::LockReleased(id) = &release_result {
            assert_eq!(id, &lock_id);
        } else {
            panic!("expected LockReleased result, got {:?}", release_result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_after_release() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-reacq-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        // Acquire
        let action1 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let r1 = execute_action(&action1, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = r1 {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release
        let release = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        execute_action(&release, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();

        // Re-acquire same resource by different holder — should succeed
        let action2 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-2".to_string(),
        };
        let r2 = execute_action(&action2, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(matches!(r2, ActionResult::LockAcquired(_)));
    }

    #[tokio::test]
    async fn test_run_agent_task_lifecycle() {
        let dir = std::env::temp_dir().join(format!("loopr-agent-task-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let stores = test_stores(&dir);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));

        // Create an agent session
        let session = AgentSession::new(AgentType::Implementer, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Run the agent task
        run_agent_task(
            session_id.clone(),
            AgentType::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        // Check session reached a terminal state
        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal());

        // Drain events — should have status changes
        let mut events = vec![];
        while let Ok(e) = event_rx.try_recv() {
            events.push(e);
        }
        assert!(
            events.iter().any(|e| e.event == "agent.status_changed"),
            "expected agent status change events"
        );
    }

    // --- Group A: Record creation actions ---

    #[tokio::test]
    async fn test_execute_create_plan() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-createplan-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::CreatePlan {
            title: "New Plan".to_string(),
            description: "Plan desc".to_string(),
            acceptance_criteria: "Tests pass".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "plans");
            assert!(!id.is_empty());
            assert_ne!(id, "unknown");
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_spec() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-createspec-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (plan_id, _, _, _) = create_test_hierarchy(&bridge);

        let action = AgentAction::CreateSpec {
            plan_id,
            title: "New Spec".to_string(),
            description: "Spec desc".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "specs");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_phase() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-createphase-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, spec_id, _, _) = create_test_hierarchy(&bridge);

        let action = AgentAction::CreatePhase {
            spec_id,
            title: "New Phase".to_string(),
            description: "Phase desc".to_string(),
            order: 2,
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "phases");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_work_item() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-createwi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&bridge);
        // Transition existing WI out of Draft so draft guard doesn't block new creation
        bridge.request(
            "work_item.transition",
            serde_json::json!({
                "id": wi_id, "target_status": "Ready", "role": "coordinator"
            }),
        );

        let action = AgentAction::CreateWorkItem {
            phase_id,
            title: "New WI".to_string(),
            description: "WI desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "work_items");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    // --- Group B: Git operations ---

    #[tokio::test]
    async fn test_execute_commit_success() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-commit-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Initialize a git repo
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        // Create a file to commit
        std::fs::write(dir.join("test.txt"), "hello").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::Commit {
            message: "test commit".to_string(),
            paths: vec![],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::Committed(ref msg) if msg == "test commit"),
            "expected Committed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_commit_specific_paths() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-commitpaths-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Initialize git repo
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::Commit {
            message: "add a.txt only".to_string(),
            paths: vec!["a.txt".to_string()],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::Committed(_)));
    }

    #[tokio::test]
    async fn test_execute_propose_bundle() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-propbundle-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Initialize git repo
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        // Need an initial commit for branch name
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec!["Implemented feature X".to_string()],
        };
        let result = execute_action(&action, &runner, &bridge, &dir, Some(&wi_id), AgentType::Implementer)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref desc) if desc == "My bundle"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_propose_bundle_no_work_item() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-propnowi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Initialize git repo
        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec![],
        };
        // work_item_id = None should fail
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work_item_id"));
    }

    // --- Group C: Agent management + domain actions ---

    #[tokio::test]
    async fn test_execute_create_learning_with_all_fields() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-learning-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::CreateLearning {
            content: "Always add tests".to_string(),
            scope: "global".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec!["implementer".to_string()]),
            resource_tags: Some(vec!["src/".to_string()]),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::LearningCreated(ref c) if c == "Always add tests"),
            "expected LearningCreated, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_learning_minimal() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-learnmin-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::CreateLearning {
            content: "Minimal learning".to_string(),
            scope: "workitem".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: None,
            resource_tags: None,
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::LearningCreated(_)));
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-spawnres-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::SpawnResearcher {
            query: "How does auth work?".to_string(),
            scope_id: "plan-1".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        // agent.start may fail (no LLM key), which returns ActionError
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_validate_document() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-valdoc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (plan_id, _, _, _) = create_test_hierarchy(&bridge);

        let action = AgentAction::ValidateDocument {
            collection: "plan".to_string(),
            id: plan_id,
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        // Validation may fail (no LLM key) returning ActionError, or succeed with DocumentValidated
        assert!(
            matches!(
                result,
                ActionResult::DocumentValidated { .. } | ActionResult::ActionError(_)
            ),
            "expected DocumentValidated or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-triage-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        // Create a bundle for the work item
        let bundle_resp = bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_item_id": wi_id,
                "branch_name": "feature/test",
                "description": "Test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Bundle starts as Proposed — TriageBundle transitions Proposed → Triaged
        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Triaged")),
            "expected Transitioned to Triaged, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-accept-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        // Create and transition bundle through Proposed → Triaged → Reviewed
        let bundle_resp = bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_item_id": wi_id,
                "branch_name": "feature/accept",
                "description": "Accept test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Accepted")),
            "expected Transitioned to Accepted, got: {:?}",
            result
        );
    }

    // --- Group D: Lifecycle paths ---

    #[tokio::test]
    async fn test_write_file_path_escape() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-escape-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::WriteFile {
            path: "../../../etc/passwd".to_string(),
            content: "pwned".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("escapes worktree"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-readnf-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::ReadFile {
            path: "nonexistent.txt".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_transition_role_inference() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-roleinfer-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let (_, _, _, wi_id) = create_test_hierarchy(&bridge);

        // Transition without explicit role — should infer from agent type
        let action = AgentAction::Transition {
            collection: "work_item".to_string(),
            id: wi_id,
            target_status: "Ready".to_string(),
            role: None, // Should infer "coordinator" from AgentType::Coordinator
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Coordinator)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned with inferred role, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-writedirs-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::WriteFile {
            path: "deep/nested/dir/file.txt".to_string(),
            content: "nested content".to_string(),
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Implementer)
            .await
            .unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
        let content = std::fs::read_to_string(dir.join("deep/nested/dir/file.txt")).unwrap();
        assert_eq!(content, "nested content");
    }

    #[tokio::test]
    async fn test_search_code_action() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-searchcode-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("example.rs"), "fn main() { println!(\"hello\"); }").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::SearchCode {
            pattern: "fn main".to_string(),
            glob: None,
            path: None,
        };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Researcher)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("fn main"))
                || matches!(result, ActionResult::ActionError(_)),
            "got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_list_directory_action() {
        let dir = std::env::temp_dir().join(format!("loopr-exec-listdir-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file1.txt"), "a").unwrap();
        std::fs::write(dir.join("file2.txt"), "b").unwrap();

        let runner = ToolRunner::new(&[]);
        let stores = test_stores(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.clone(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());

        let action = AgentAction::ListDirectory { path: ".".to_string() };
        let result = execute_action(&action, &runner, &bridge, &dir, None, AgentType::Researcher)
            .await
            .unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("file1.txt")),
            "got: {:?}",
            result
        );
    }
}
