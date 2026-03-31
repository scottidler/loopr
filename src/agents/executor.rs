use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use eyre::{Result, eyre};
use log::{debug, error, info, warn};
use tokio::sync::broadcast;

// Note: log:: macros are still used in run_agent_task (pre-logger section) and utility functions.
// Post-logger code uses agent_log. execute_action uses agent_log passed as parameter.

use crate::agents::agent_logger::AgentLogger;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::coordinator;
use crate::agents::implementer::{self, LlmClient};
use crate::agents::integrator;
use crate::agents::llm_client::AgentLlmClient;
use crate::agents::researcher;
use crate::agents::reviewer;
use crate::agents::{Agent, AgentAction, AgentContext, AgentSession, AgentStatus, AgentType};
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::tick::TickStatus;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolResult;
use crate::worktree::manager::WorktreeManager;

/// Normalize a collection name from plural to singular for IPC method dispatch.
/// The LLM may emit "plans", "specs", "phases", "works", "bundles", "ticks"
/// but IPC methods use singular: "plan", "spec", "phase", "work", "bundle", "tick".
fn normalize_collection(collection: &str) -> &str {
    debug!("normalize_collection(collection={})", collection);
    match collection {
        "plans" => "plan",
        "specs" => "spec",
        "phases" => "phase",
        "works" => "work",
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
    debug!("create_llm_client(session_id={}, model={})", session_id, config.model);
    let client = AgentLlmClient::new(config.clone(), session_id.to_string(), event_tx.clone())?;
    info!("Agent {} using LLM client (model: {})", session_id, config.model);
    Ok(Box::new(client))
}

/// Returns the integration SHA of the latest Published Tick,
/// or "HEAD" if no Ticks have been published yet.
pub fn resolve_worktree_base(stores: &Stores) -> String {
    debug!("resolve_worktree_base()");
    let Ok(ticks) = stores.read_ticks() else {
        error!("ticks lock poisoned");
        return "HEAD".to_string();
    };
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .and_then(|t| t.integration_sha.clone())
        .unwrap_or_else(|| "HEAD".to_string())
}

/// Resolve the ID of the latest Published Tick from stores.
/// Returns None if no Published Tick exists (bootstrap case).
fn resolve_latest_published_tick_id(stores: &Stores) -> Option<String> {
    debug!("resolve_latest_published_tick_id()");
    let ticks = stores.read_ticks().ok()?;
    ticks
        .values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .map(|t| t.id.clone())
}

/// Determine the Work transition after an Implementer's agent loop exits.
///
/// Returns `Some(target_status)` to transition, or `None` to skip (sibling still active).
///
/// Decision table:
/// | Agent result | Sibling active? | Bundle state         | Work transition |
/// |---|---|---|---|
/// | Ok           | —               | —                    | "InReview"      |
/// | Err          | Yes             | —                    | (skip)          |
/// | Err          | No              | active Bundle exists | "InReview"      |
/// | Err          | No              | all Rejected/none    | "Blocked"       |
fn determine_work_handback(stores: &Stores, work_id: &str, session_id: &str, succeeded: bool) -> Option<&'static str> {
    if succeeded {
        return Some("InReview");
    }

    // If a sibling implementer is still active, don't touch the Work.
    let sessions = stores.read_agent_sessions().ok()?;
    let sibling_active = sessions.values().any(|s| {
        s.id != session_id
            && s.agent_type == AgentType::Implementer
            && s.work_id.as_deref() == Some(work_id)
            && !s.status.is_terminal()
    });
    drop(sessions);

    if sibling_active {
        return None; // let the sibling finish
    }

    // Did the agent produce a usable Bundle?
    let bundles = stores.read_bundles().ok()?;
    let has_active_bundle = bundles
        .values()
        .any(|b| b.work_id == work_id && !matches!(b.status, BundleStatus::Rejected | BundleStatus::Superseded));

    Some(if has_active_bundle { "InReview" } else { "Blocked" })
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
    debug!("run_agent_task(session_id={}, agent_type={})", session_id, agent_type);
    info!("Agent task started: {} ({})", session_id, agent_type);

    // Create a worktree for the agent before starting the loop.
    // Thinking plane agents (Coordinator, Researcher, Integrator) don't use worktrees.
    // Implementers key on work_id, Reviewers on bundle_id.
    let worktree_key = if agent_type.is_thinking_plane() {
        None
    } else {
        let Ok(sessions) = stores.read_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        let session = match sessions.get(&session_id) {
            Some(s) => s,
            None => {
                error!("Agent {} session not found in stores", session_id);
                return;
            }
        };
        match agent_type {
            AgentType::Implementer => session.work_id.clone(),
            AgentType::Reviewer => session.bundle_id.clone(),
            // Already handled by is_thinking_plane() above
            AgentType::Coordinator | AgentType::Researcher | AgentType::Integrator | AgentType::Chat => None,
        }
    };

    if let Some(ref key) = worktree_key {
        let base_ref = resolve_worktree_base(&stores);
        info!("Agent {} using worktree base: {}", session_id, base_ref);
        let worktree_path = match worktree_mgr.get_or_create(key, &base_ref) {
            Ok(path) => path,
            Err(e) => {
                error!("Agent {} worktree creation failed: {}", session_id, e);
                // Fail the session immediately — don't proceed without a worktree
                let Ok(mut sessions) = stores.write_agent_sessions() else {
                    error!("agent_sessions lock poisoned");
                    return;
                };
                if let Some(session) = sessions.get_mut(&session_id) {
                    let _ = session.transition_to(AgentStatus::Failed);
                    session.error_message = Some(format!("worktree creation failed: {}", e));
                    persist_session(&stores, session);
                }
                debug!("[agent_status] {}: -> Failed (worktree creation)", session_id);
                let _ = event_tx.send(DaemonEvent::agent_status_failed(
                    &session_id,
                    Some(format!("worktree creation failed: {}", e)),
                ));
                return;
            }
        };

        {
            let Ok(mut sessions) = stores.write_agent_sessions() else {
                error!("agent_sessions lock poisoned");
                return;
            };
            if let Some(session) = sessions.get_mut(&session_id) {
                session.worktree_path = Some(worktree_path.to_string_lossy().to_string());
                persist_session(&stores, session);
            }
            info!("Agent {} worktree created at {}", session_id, worktree_path.display());
        }
    }

    // Retain a copy of the worktree manager for cleanup after the agent loop exits.
    // The original is moved into the bridge.
    let cleanup_mgr = worktree_mgr.clone();
    let cleanup_key = worktree_key.clone();

    // Create per-agent logger
    let agent_log = match AgentLogger::new(agent_type, &session_id, stores.session_dir.as_deref()) {
        Ok(al) => al,
        Err(e) => {
            error!("Agent {} failed to create logger: {}", session_id, e);
            return;
        }
    };
    agent_log.info(&format!("Agent task started: {} ({})", session_id, agent_type));

    // Create the in-process IPC bridge for this agent
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    // Transition to Running
    {
        let Ok(mut sessions) = stores.write_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(AgentStatus::Running) {
                agent_log.error(&format!("failed to start: {}", e));
                return;
            }
            persist_session(&stores, session);
        }
    }
    debug!("[agent_status] {}: -> Running", session_id);
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Running));

    let result = run_agent_loop(&session_id, agent_type, &stores, &bridge, &event_tx, &agent_log).await;

    // Bundle-aware work handback (implementer only)
    if agent_type == AgentType::Implementer
        && let Some(ref wi_id) = worktree_key
    {
        if let Some(target_status) = determine_work_handback(&stores, wi_id, &session_id, result.is_ok()) {
            let resp = bridge.request(
                "work.transition",
                serde_json::json!({ "id": wi_id, "target_status": target_status, "role": "implementer" }),
            );
            if resp.is_error() {
                agent_log.warn(&format!(
                    "failed to transition work {} → {}: {:?}",
                    wi_id, target_status, resp.error
                ));
            } else {
                agent_log.info(&format!("transitioned work {} → {}", wi_id, target_status));
            }
        } else {
            agent_log.info(&format!(
                "skipping work handback for {} — sibling implementer still active",
                wi_id
            ));
        }
    }

    // Clean up worktree after agent loop exits (only for non-thinking-plane agents)
    if let Some(ref key) = cleanup_key {
        if let Err(e) = cleanup_mgr.cleanup(key) {
            agent_log.warn(&format!("worktree cleanup failed (key={}): {}", key, e));
        } else {
            agent_log.info(&format!("worktree cleaned up (key={})", key));
        }
    }

    // Transition to terminal state based on result
    let terminal_status = match result {
        Ok(_) => AgentStatus::Completed,
        Err(ref e) => {
            agent_log.warn(&format!("failed: {}", e));
            AgentStatus::Failed
        }
    };

    {
        let Ok(mut sessions) = stores.write_agent_sessions() else {
            error!("agent_sessions lock poisoned");
            return;
        };
        if let Some(session) = sessions.get_mut(&session_id) {
            if let Err(e) = session.transition_to(terminal_status) {
                agent_log.error(&format!("failed to transition to {:?}: {}", terminal_status, e));
                return;
            }
            if let Err(ref e) = result {
                session.error_message = Some(e.to_string());
            }
            persist_session(&stores, session);
        }
    }
    // Create a Learning from implementer failures so the Coordinator can adapt
    if terminal_status == AgentStatus::Failed && agent_type == AgentType::Implementer {
        let (wi_title, wi_id, iteration_count) = {
            let Ok(sessions) = stores.read_agent_sessions() else {
                error!("agent_sessions lock poisoned");
                return;
            };
            let session = sessions.get(&session_id);
            let wi_id = session.and_then(|s| s.work_id.clone()).unwrap_or_default();
            let iteration = session.map(|s| s.iteration).unwrap_or(0);
            let title = if !wi_id.is_empty() {
                stores
                    .read_works()
                    .ok()
                    .and_then(|works| works.get(&wi_id).map(|w| w.title.clone()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (title, wi_id, iteration)
        };

        let error_msg = result.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
        let learning = crate::domain::learning::Learning::new(
            wi_id.clone(),
            crate::domain::learning::LearningScope::Phase,
            format!(
                "Implementer failed on '{}' after {} iterations. Error: {}. \
                 Consider: splitting into smaller tasks, providing more context, \
                 or checking dependency ordering.",
                wi_title, iteration_count, error_msg
            ),
        );
        let learning_id = learning.id.clone();
        if let Ok(mut learnings) = stores.write_learnings() {
            learnings.insert(learning_id.clone(), learning.clone());
        } else {
            error!("learnings lock poisoned");
        }
        if let Ok(Some(mut store_guard)) = stores.lock_store() {
            let _ = store_guard.create(learning);
        }
        agent_log.info(&format!("created failure learning {} on '{}'", learning_id, wi_title));
    }

    debug!("[agent_status] {}: -> {:?} (terminal)", session_id, terminal_status);
    if terminal_status == AgentStatus::Failed {
        let error = result.as_ref().err().map(|e| e.to_string());
        let _ = event_tx.send(DaemonEvent::agent_status_failed(&session_id, error));
    } else {
        let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, terminal_status));
    }

    agent_log.info(&format!("task finished → {:?}", terminal_status));
}

/// Agent loop — dispatches to the appropriate agent implementation based on type.
async fn run_agent_loop(
    session_id: &str,
    agent_type: AgentType,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    event_tx: &broadcast::Sender<DaemonEvent>,
    agent_log: &AgentLogger,
) -> Result<()> {
    agent_log.debug(&format!(
        "run_agent_loop(session_id={}, agent_type={})",
        session_id, agent_type
    ));
    // Verify the bridge works by checking system status
    let status_resp = bridge.request("system.status", serde_json::json!(null));
    if status_resp.is_error() {
        return Err(eyre!("bridge health check failed: {:?}", status_resp.error));
    }
    agent_log.info("Bridge health check passed");

    let mut ctx = AgentContext::from_session_id(session_id, agent_type, stores.clone(), event_tx.clone())?;

    // Per-session tool detection: if the agent has a worktree, detect project type
    // from marker files and use appropriate tool presets instead of global defaults.
    if let Some(ref wt_path) = ctx.session.worktree_path {
        let worktree = std::path::Path::new(wt_path);
        ctx.tool_runner = Arc::new(crate::tools::ToolRunner::detect_or_default(
            worktree,
            &stores.config.agents.tools,
        ));
        ctx.tool_executor = Arc::new(crate::tools::ToolExecutor::detect_or_configured(
            worktree,
            &stores.config.agents.tools,
        ));
        agent_log.info(&format!(
            "Session tool executor: {} tools (detected from {})",
            ctx.tool_executor.available_tools().len(),
            wt_path
        ));
    }

    let (result, iteration) = match agent_type {
        AgentType::Implementer => {
            let config = stores.config.agents.implementer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let work_id = ctx
                .session
                .work_id
                .clone()
                .ok_or_else(|| eyre!("implementer session missing work_id"))?;
            let worktree_path = ctx
                .session
                .worktree_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .ok_or_else(|| eyre!("implementer session missing worktree_path"))?;
            let mut agent = implementer::ImplementerAgent::new(ctx, llm, config, work_id, worktree_path);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentType::Reviewer => {
            let config = stores.config.agents.reviewer.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let mut agent = reviewer::ReviewerAgent::new(ctx, llm, config)?;
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentType::Coordinator => {
            let config = stores.config.agents.coordinator.clone();
            let llm = create_llm_client(&config.role, session_id, event_tx)?;
            let mut agent = coordinator::CoordinatorAgent::new(ctx, llm, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentType::Researcher => {
            let config = stores.config.agents.researcher.clone();
            let llm = create_llm_client(&config, session_id, event_tx)?;
            let mut agent = researcher::ResearcherAgent::new(ctx, llm, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentType::Integrator => {
            let config = stores.config.integrator.clone();
            let mut agent = integrator::IntegratorAgent::new(ctx, config);
            let result = agent.run().await;
            (result, agent.ctx.session.iteration)
        }
        AgentType::Chat => {
            // Chat sessions are not started via run_agent_task — they use chat.submit IPC.
            // This arm should never be reached.
            return Ok(());
        }
    };

    // Unified iteration writeback
    {
        let mut sessions = stores.write_agent_sessions()?;
        if let Some(s) = sessions.get_mut(session_id) {
            s.iteration = iteration;
        }
    }

    result
}

/// Execute a single agent action. Used by the agent loop to process parsed LLM responses.
///
/// `ctx.session.agent_type` is used for role inference on Transition actions (when role is None).
pub async fn execute_action(
    action: &AgentAction,
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
) -> Result<ActionResult> {
    let tool_runner = &*ctx.tool_runner;
    let bridge = &ctx.bridge;
    let agent_type = ctx.session.agent_type;
    let agent_log = &ctx.log;
    agent_log.debug(&format!("execute_action(action={:?})", action));
    match action {
        AgentAction::RunTool { tool, args } => {
            let tool_result = tool_runner
                .run(tool, args, worktree_path)
                .await
                .map_err(|e| eyre!("run_tool '{}': {}", tool, e))?;
            Ok(ActionResult::ToolRun(tool_result))
        }
        AgentAction::WriteFile { path, content } => {
            // Validate path stays within worktree (sandbox)
            let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

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
            ctx.read_cache
                .lock()
                .expect("read_cache poisoned")
                .invalidate(&full_path);
            Ok(ActionResult::FileWritten(path.clone()))
        }
        AgentAction::EditFile {
            path,
            old_string,
            new_string,
        } => {
            let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

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

            let content = tokio::fs::read_to_string(&full_path)
                .await
                .map_err(|e| eyre!("edit_file read '{}': {}", path, e))?;

            let count = content.matches(old_string.as_str()).count();
            if count == 0 {
                return Ok(ActionResult::ActionError(format!(
                    "edit_file '{}': old_string not found in file",
                    path
                )));
            }
            if count > 1 {
                return Ok(ActionResult::ActionError(format!(
                    "edit_file '{}': old_string found {} times (must be unique - provide more context)",
                    path, count
                )));
            }

            let updated = content.replacen(old_string.as_str(), new_string, 1);
            tokio::fs::write(&full_path, &updated)
                .await
                .map_err(|e| eyre!("edit_file write '{}': {}", path, e))?;
            ctx.read_cache
                .lock()
                .expect("read_cache poisoned")
                .invalidate(&full_path);
            Ok(ActionResult::FileEdited(path.clone()))
        }
        AgentAction::ReadFile { path, offset, limit } => {
            let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

            // Stat for mtime (single syscall, kernel-cached)
            let mtime = tokio::fs::metadata(&full_path)
                .await
                .map_err(|e| eyre!("read_file '{}': {}", path, e))?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);

            // Dedup check BEFORE reading the file
            if let Some(cached_lines) = ctx
                .read_cache
                .lock()
                .expect("read_cache poisoned")
                .check_hit(&full_path, *offset, *limit, mtime)
            {
                let start = offset.unwrap_or(1).max(1);
                let effective_limit = limit.unwrap_or(500);
                let end = (start + effective_limit - 1).min(cached_lines as u64);
                return Ok(ActionResult::FileRead(format!(
                    "File unchanged since last read \
                     (lines {}-{} of {}, use offset/limit for other sections, \
                     or proceed with editing).",
                    start, end, cached_lines
                )));
            }

            // Cache miss - read the file
            let content = tokio::fs::read_to_string(&full_path)
                .await
                .map_err(|e| eyre!("read_file '{}': {}", path, e))?;
            let lines: Vec<&str> = content.lines().collect();

            // Record in cache for future dedup
            ctx.read_cache
                .lock()
                .expect("read_cache poisoned")
                .record(&full_path, *offset, *limit, mtime, lines.len());

            let start = offset.unwrap_or(1).max(1) as usize - 1;
            let effective_limit = limit.unwrap_or(500) as usize;
            let end = (start + effective_limit).min(lines.len());
            let mut numbered: Vec<String> = lines[start..end]
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
                .collect();
            if end < lines.len() && limit.is_none() {
                numbered.push(format!(
                    "\n... [{} more lines, use offset/limit to read specific sections]",
                    lines.len() - end
                ));
            }
            Ok(ActionResult::FileRead(numbered.join("\n")))
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
            // F2: Derive branch name deterministically from work_id — matches
            // WorktreeManager::create() which uses format!("agent/{}", work_id).
            // This avoids relying on `git rev-parse` which can return "main" when
            // HEAD is detached or the worktree checkout didn't switch properly.
            let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;
            let branch_name = format!("agent/{}", wi_id);

            // Fix #1: Resolve base_tick_id from latest Published Tick
            let base_tick_id = resolve_latest_published_tick_id(bridge.stores());

            let mut params = serde_json::json!({
                "work_id": wi_id,
                "branch_name": branch_name,
                "claims": claims,
                "description": description,
            });
            if let Some(tick_id) = &base_tick_id {
                params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
            }

            let resp = bridge.request("bundle.create", params.clone());
            if resp.is_error() {
                // Fix #7: On stale bundle rejection (-32002), retry once
                if let Some(ref err) = resp.error
                    && err.code == -32002
                {
                    agent_log.warn(&format!(
                        "Bundle stale ({}), retrying with fresh base_tick_id",
                        err.message
                    ));
                    let new_base_tick_id = resolve_latest_published_tick_id(bridge.stores());
                    if let Some(tick_id) = &new_base_tick_id {
                        params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
                    }
                    let retry_resp = bridge.request("bundle.create", params);
                    if retry_resp.is_error() {
                        return Err(eyre!("propose bundle failed after retry: {:?}", retry_resp.error));
                    }
                    return Ok(ActionResult::BundleProposed(description.clone()));
                }
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
            // M9: Defense-in-depth — if source_id doesn't look like a record ID,
            // use work_id if available (fixes Reviewer sending title as source_id)
            let effective_source_id = if !source_id.starts_with("wi-")
                && !source_id.starts_with("phase-")
                && !source_id.starts_with("plan-")
                && !source_id.starts_with("spec-")
            {
                work_id.unwrap_or(source_id).to_string()
            } else {
                source_id.clone()
            };
            let mut params = serde_json::json!({
                "content": content,
                "scope": scope,
                "source_id": effective_source_id,
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
            agent_log.info(&format!("CreatePlan: {} (id: {})", title, id));
            // Auto-approve Plan in non-Interactive modes
            if bridge.config().agents.coordinator.interview_mode != crate::config::InterviewMode::Interactive {
                let approve_resp = bridge.request("coordinator.accept_plan", serde_json::json!({ "plan_id": id }));
                if approve_resp.is_error() {
                    agent_log.warn(&format!(
                        "auto-approve failed for plan {}: {:?}",
                        id, approve_resp.error
                    ));
                } else {
                    agent_log.info(&format!("auto-approved plan {} (interview_mode != interactive)", id));
                }
            }
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
            agent_log.info(&format!("CreateSpec: {} (id: {})", title, id));
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
            agent_log.info(&format!("CreatePhase: {} (id: {})", title, id));
            Ok(ActionResult::RecordCreated {
                collection: "phases".to_string(),
                id,
            })
        }
        AgentAction::CreateWork {
            phase_id,
            title,
            description,
            resource_tags,
            acceptance_criteria,
            dependencies,
        } => {
            let resp = bridge.request(
                "work.create",
                serde_json::json!({
                    "phase_id": phase_id,
                    "title": title,
                    "description": description,
                    "resource_tags": resource_tags,
                    "acceptance_criteria": acceptance_criteria,
                    "dependencies": dependencies,
                }),
            );
            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Ok(ActionResult::ActionError(format!("create_work failed: {}", msg)));
            }
            let id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            agent_log.info(&format!("CreateWork: {} (id: {}, deps: {:?})", title, id, dependencies));
            Ok(ActionResult::RecordCreated {
                collection: "works".to_string(),
                id,
            })
        }

        // --- Coordinator agent management actions ---
        AgentAction::AssignAgent { agent_type, target_id } => {
            // For implementer assignments, check dependencies and auto-transition work
            if agent_type == "implementer" {
                let get_resp = bridge.request("work.get", serde_json::json!({ "id": target_id }));

                // Check dependencies: all must be Done before assignment
                if let Some(result) = &get_resp.result {
                    let deps: Vec<String> = result
                        .get("dependencies")
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();

                    for dep_id in &deps {
                        let dep_resp = bridge.request("work.get", serde_json::json!({ "id": dep_id }));
                        let dep_status = dep_resp
                            .result
                            .as_ref()
                            .and_then(|v| v.get("status"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        if dep_status != "Done" {
                            let dep_title = dep_resp
                                .result
                                .as_ref()
                                .and_then(|v| v.get("title"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown");
                            return Ok(ActionResult::DependencyNotMet {
                                work_id: target_id.clone(),
                                message: format!(
                                    "dependency '{}' ({}) is {} (must be Done)",
                                    dep_title, dep_id, dep_status
                                ),
                            });
                        }
                    }
                }

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
                            "work.transition",
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
                            "work.transition",
                            serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type }),
                        );
                        if r2.is_error() {
                            let msg = r2.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Ok(ActionResult::ActionError(format!(
                                "auto-transition Ready→InProgress failed: {}",
                                msg
                            )));
                        }
                        agent_log.info(&format!(
                            "AssignAgent: auto-transitioned work {} Draft→Ready→InProgress",
                            target_id
                        ));
                    }
                    "Ready" => {
                        // Ready → InProgress
                        let r = bridge.request(
                            "work.transition",
                            serde_json::json!({ "id": target_id, "target_status": "InProgress", "role": "coordinator", "assignee": agent_type }),
                        );
                        if r.is_error() {
                            let msg = r.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Ok(ActionResult::ActionError(format!(
                                "auto-transition Ready→InProgress failed: {}",
                                msg
                            )));
                        }
                        agent_log.info(&format!(
                            "AssignAgent: auto-transitioned work {} Ready→InProgress",
                            target_id
                        ));
                    }
                    "InProgress" => {
                        // Already correct
                    }
                    other => {
                        return Ok(ActionResult::ActionError(format!(
                            "cannot assign implementer to work {} in state '{}'",
                            target_id, other
                        )));
                    }
                }
            }

            // Map target_id to the correct param based on agent type
            let mut params = serde_json::json!({ "agent_type": agent_type });
            match agent_type.as_str() {
                "implementer" => {
                    if !target_id.starts_with("wk-") {
                        return Ok(ActionResult::ActionError(format!(
                            "assign_agent implementer: '{}' is not a work ID (expected wk-* prefix)",
                            target_id
                        )));
                    }
                    params["work_id"] = serde_json::json!(target_id);
                }
                "reviewer" => {
                    if !target_id.starts_with("bd-") {
                        return Ok(ActionResult::ActionError(format!(
                            "assign_agent reviewer: '{}' is not a bundle ID (expected bd-* prefix)",
                            target_id
                        )));
                    }
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
            agent_log.info(&format!(
                "AssignAgent: {} → {} (session: {})",
                agent_type, target_id, session_id
            ));
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
            agent_log.info(&format!(
                "SpawnResearcher: {} (scope: {}, session: {})",
                query, scope_id, session_id
            ));
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
            agent_log.info(&format!(
                "ValidateDocument {}/{}: verdict={}, issues={}",
                collection,
                id,
                verdict,
                issues.len()
            ));
            Ok(ActionResult::DocumentValidated {
                verdict,
                summary,
                issues,
            })
        }
        AgentAction::EvaluateCoverage {
            parent_collection,
            parent_id,
        } => {
            let resp = bridge.request(
                "coverage.evaluate",
                serde_json::json!({ "parent_collection": parent_collection, "parent_id": parent_id }),
            );
            if resp.is_error() {
                let msg = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "unknown coverage error".to_string());
                return Ok(ActionResult::ActionError(format!(
                    "evaluate_coverage {}/{} failed: {}",
                    parent_collection, parent_id, msg
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
            let gaps: Vec<String> = resp
                .result
                .as_ref()
                .and_then(|v| v.get("gaps"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|gap| gap.get("description").and_then(|d| d.as_str()).map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            agent_log.info(&format!(
                "EvaluateCoverage {}/{}: verdict={}, gaps={}",
                parent_collection,
                parent_id,
                verdict,
                gaps.len()
            ));
            Ok(ActionResult::CoverageEvaluated { verdict, summary, gaps })
        }
        AgentAction::InterviewQuestion { questions } => {
            let interview_mode = bridge.config().agents.coordinator.interview_mode;
            if interview_mode == crate::config::InterviewMode::Auto {
                // Auto mode: synthesize answer from goal + repo context
                let goal_text = {
                    let goals = ctx.stores.read_coordinator_goals()?;
                    goals
                        .values()
                        .find(|g| g.active)
                        .map(|g| g.goal.clone())
                        .unwrap_or_else(|| "No goal set.".to_string())
                };
                let repo_path = &bridge.config().project.repo_path;
                let readme = std::fs::read_to_string(repo_path.join("README.md"))
                    .unwrap_or_else(|_| "no README found".to_string());
                let file_tree = std::fs::read_dir(repo_path)
                    .map(|entries| {
                        entries
                            .filter_map(|e| e.ok())
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|_| "could not list files".to_string());
                let synthetic_answer = format!(
                    "Based on the goal: \"{}\"\nRepository context: {}\nFile tree: {}",
                    goal_text, readme, file_tree
                );
                // Call interview_respond to record the exchange
                let resp = bridge.request(
                    "coordinator.interview_respond",
                    serde_json::json!({ "answer": synthetic_answer }),
                );
                if resp.is_error() {
                    let msg = resp
                        .error
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| "interview auto-respond failed".to_string());
                    return Ok(ActionResult::ActionError(msg));
                }
                agent_log.info(&format!(
                    "InterviewQuestion: auto-answered {} question(s)",
                    questions.len()
                ));
                Ok(ActionResult::Done(format!(
                    "auto-answered {} interview question(s)",
                    questions.len()
                )))
            } else {
                // Interactive mode: send questions to TUI via bridge event
                let resp = bridge.request(
                    "coordinator.interview_question",
                    serde_json::json!({ "questions": questions }),
                );
                if resp.is_error() {
                    let msg = resp
                        .error
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| "interview question failed".to_string());
                    return Ok(ActionResult::ActionError(msg));
                }
                agent_log.info(&format!("InterviewQuestion: {} questions sent", questions.len()));
                Ok(ActionResult::Done(format!(
                    "sent {} interview question(s)",
                    questions.len()
                )))
            }
        }
        AgentAction::ProposePlan {
            title,
            description,
            acceptance_criteria,
        } => {
            // Create Plan as Draft via bridge
            let resp = bridge.request(
                "plan.create",
                serde_json::json!({
                    "title": title,
                    "description": description,
                    "acceptance_criteria": acceptance_criteria,
                }),
            );
            if resp.is_error() {
                let msg = resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "plan creation failed".to_string());
                return Ok(ActionResult::ActionError(msg));
            }
            let plan_id = resp
                .result
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            agent_log.info(&format!("ProposePlan: created draft plan {}", plan_id));
            // Auto-approve Plan in non-Interactive modes
            if bridge.config().agents.coordinator.interview_mode != crate::config::InterviewMode::Interactive {
                let approve_resp = bridge.request("coordinator.accept_plan", serde_json::json!({ "plan_id": plan_id }));
                if approve_resp.is_error() {
                    agent_log.warn(&format!(
                        "auto-approve failed for plan {}: {:?}",
                        plan_id, approve_resp.error
                    ));
                } else {
                    agent_log.info(&format!(
                        "auto-approved plan {} (interview_mode != interactive)",
                        plan_id
                    ));
                }
            }
            Ok(ActionResult::RecordCreated {
                collection: "plans".to_string(),
                id: plan_id,
            })
        }
        AgentAction::ReviseParent {
            collection,
            id,
            reason,
            diagnostic,
        } => {
            // Transition parent back to Draft
            let transition_resp = bridge.request(
                "record.transition",
                serde_json::json!({
                    "collection": collection,
                    "id": id,
                    "target_status": "draft",
                    "role": "coordinator",
                    "skip_validation": true,
                    "skip_reason": "bubble-up revision: parent needs refinement"
                }),
            );
            if transition_resp.is_error() {
                let msg = transition_resp
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "unknown transition error".to_string());
                return Ok(ActionResult::ActionError(format!(
                    "revise_parent {}/{} transition failed: {}",
                    collection, id, msg
                )));
            }

            // Create a diagnostic Learning
            let learning_content = format!(
                "Bubble-up revision for {}/{}: {}\nDiagnostic: {}",
                collection, id, reason, diagnostic
            );
            let _learning_resp = bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": learning_content,
                    "scope": "plan",
                    "source_id": id,
                }),
            );

            agent_log.info(&format!(
                "ReviseParent {}/{}: reason={}, diagnostic={}",
                collection, id, reason, diagnostic
            ));
            Ok(ActionResult::Transitioned(format!(
                "{}/{} → draft (bubble-up: {})",
                collection, id, reason
            )))
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
            agent_log.info(&format!(
                "Lock acquired: {} on '{}' for {}",
                lock_id, resource, holder_id
            ));
            Ok(ActionResult::LockAcquired(lock_id))
        }
        AgentAction::ReleaseLock { lock_id } => {
            let resp = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
            if resp.is_error() {
                return Err(eyre!("lock.release failed: {:?}", resp.error));
            }
            agent_log.info(&format!("Lock released: {}", lock_id));
            Ok(ActionResult::LockReleased(lock_id.clone()))
        }
        AgentAction::TriageBundle { bundle_id } => {
            if !bundle_id.starts_with("bd-") {
                return Ok(ActionResult::ActionError(format!(
                    "triage_bundle: '{}' is not a bundle ID (expected bd-* prefix). Use the bundle ID, not the work ID.",
                    bundle_id
                )));
            }
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
            agent_log.info(&format!("TriageBundle: {} → Triaged", bundle_id));
            Ok(ActionResult::Transitioned(format!("bundle/{} → Triaged", bundle_id)))
        }
        AgentAction::AcceptBundle { bundle_id } => {
            if !bundle_id.starts_with("bd-") {
                return Ok(ActionResult::ActionError(format!(
                    "accept_bundle: '{}' is not a bundle ID (expected bd-* prefix). Use the bundle ID, not the work ID.",
                    bundle_id
                )));
            }
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
            agent_log.info(&format!("AcceptBundle: {} → Accepted", bundle_id));
            Ok(ActionResult::Transitioned(format!("bundle/{} → Accepted", bundle_id)))
        }

        AgentAction::OverrideWork {
            work_id,
            target_status,
            reason,
        } => {
            // Kill any active agent sessions on this Work
            {
                let sessions = bridge.stores().read_agent_sessions()?;
                let active_sessions: Vec<String> = sessions
                    .values()
                    .filter(|s| s.work_id.as_deref() == Some(work_id) && !s.status.is_terminal())
                    .map(|s| s.id.clone())
                    .collect();
                drop(sessions);

                for sid in &active_sessions {
                    let stop_resp = bridge.request("agent.stop", serde_json::json!({ "session_id": sid }));
                    if stop_resp.is_error() {
                        agent_log.warn(&format!(
                            "OverrideWork: failed to stop session {}: {:?}",
                            sid, stop_resp.error
                        ));
                    } else {
                        agent_log.info(&format!("OverrideWork: stopped session {}", sid));
                    }
                }
            }

            // Override transition
            let resp = bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": work_id,
                    "target_status": target_status,
                    "role": "coordinator",
                    "override": true,
                    "reason": reason,
                }),
            );

            if resp.is_error() {
                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                agent_log.warn(&format!(
                    "OverrideWork: transition failed for {} (may have already recovered): {}",
                    work_id, msg
                ));
                return Ok(ActionResult::ActionError(format!(
                    "override_work transition failed: {}",
                    msg
                )));
            }

            // Release any active locks held by this Work
            {
                let locks = bridge.stores().read_locks()?;
                let work_locks: Vec<String> = locks
                    .values()
                    .filter(|l| l.holder_id == *work_id && matches!(l.status, crate::domain::lock::LockStatus::Active))
                    .map(|l| l.id.clone())
                    .collect();
                drop(locks);

                for lid in &work_locks {
                    let release_resp = bridge.request("lock.release", serde_json::json!({ "id": lid }));
                    if release_resp.is_error() {
                        agent_log.warn(&format!(
                            "OverrideWork: failed to release lock {}: {:?}",
                            lid, release_resp.error
                        ));
                    } else {
                        agent_log.info(&format!("OverrideWork: released lock {}", lid));
                    }
                }
            }

            // Create audit Learning
            let _ = bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": format!("Override: Work {} → {} (reason: {})", work_id, target_status, reason),
                    "scope": "work",
                    "source_id": work_id,
                    "applicable_roles": ["coordinator"],
                    "resource_tags": ["override", "audit"],
                }),
            );

            agent_log.info(&format!(
                "OverrideWork: {} → {} (reason: {})",
                work_id, target_status, reason
            ));
            Ok(ActionResult::Transitioned(format!(
                "override: work/{} → {}",
                work_id, target_status
            )))
        }

        // --- Researcher actions (wired in Phase 4 researcher.rs) ---
        AgentAction::SearchCode { pattern, glob, path } => {
            let repo_root = worktree_path; // For Researcher, worktree_path is the repo root
            match researcher::execute_search_code(repo_root, pattern, glob.as_deref(), path.as_deref(), agent_log).await
            {
                Ok(output) => Ok(ActionResult::FileRead(output)),
                Err(e) => Ok(ActionResult::ActionError(e.to_string())),
            }
        }
        AgentAction::SearchFiles { pattern, path } => {
            let repo_root = worktree_path;
            match researcher::execute_search_files(repo_root, pattern, path.as_deref(), agent_log).await {
                Ok(output) => Ok(ActionResult::FileRead(output)),
                Err(e) => Ok(ActionResult::ActionError(e.to_string())),
            }
        }
        AgentAction::ListDirectory { path } => {
            let repo_root = worktree_path;
            match researcher::execute_list_directory(repo_root, path, agent_log).await {
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
    FileEdited(String),
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
    /// Coverage evaluated — contains (verdict, summary, gap_descriptions).
    CoverageEvaluated {
        verdict: String,
        summary: String,
        gaps: Vec<String>,
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
    /// Work dependency not met — cannot assign agent until deps are Done.
    DependencyNotMet {
        work_id: String,
        message: String,
    },
    // M10-12: DuplicateDetected, PhaseCompleted, GoalCompleted removed — never produced by any code path.
}

/// Run a single Work item through the full Implementer lifecycle.
///
/// This is the entry point for pull-based workers. It:
/// 1. Transitions Work Ready → InProgress (returns Ok if already claimed)
/// 2. Creates an AgentSession
/// 3. Delegates to `run_agent_task` for the full lifecycle (worktree, LLM, agent loop, handback, cleanup)
///
/// Race handling: if the Ready → InProgress transition fails (another worker grabbed it),
/// returns Ok(()) immediately — this is expected contention, not an error.
pub async fn run_single_work(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    implementer_config: &crate::config::AgentRoleConfig,
    work_id: &str,
    worker_id: u32,
) -> Result<()> {
    info!("Worker {} attempting Work {}", worker_id, work_id);

    // Step 1: Transition Work Ready → InProgress.
    // Use the bridge to go through the handler (FSM validation + persistence).
    let bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
        stores.config.clone(),
    );
    let transition_resp = bridge.request(
        "work.transition",
        serde_json::json!({
            "id": work_id,
            "target_status": "InProgress",
            "role": "coordinator",
            "assignee": "implementer",
        }),
    );
    if transition_resp.is_error() {
        // Expected contention: another worker grabbed this Work first.
        info!(
            "Worker {} could not claim Work {} (likely already claimed): {:?}",
            worker_id,
            work_id,
            transition_resp.error.as_ref().map(|e| &e.message)
        );
        return Ok(());
    }

    // Step 2: Check pool capacity and dedup before creating session
    {
        let sessions = stores.read_agent_sessions()?;
        let active_count = sessions
            .values()
            .filter(|s| s.agent_type == AgentType::Implementer && !s.status.is_terminal())
            .count();
        let max_pool = stores.config.agents.implementer.max_pool as usize;
        if active_count >= max_pool {
            warn!(
                "Worker {} pool exhausted ({}/{}), skipping Work {}",
                worker_id, active_count, max_pool, work_id
            );
            return Ok(());
        }

        // Dedup: if an implementer is already running on this work, skip
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentType::Implementer && !s.status.is_terminal() && s.work_id.as_deref() == Some(work_id)
        });
        if has_existing {
            info!(
                "Worker {} skipping Work {} — implementer already running",
                worker_id, work_id
            );
            return Ok(());
        }
    }

    // Step 3: Create AgentSession
    let mut session = AgentSession::new(AgentType::Implementer, implementer_config.model.clone());
    session.work_id = Some(work_id.to_string());
    let session_id = session.id.clone();

    // Persist session
    if let Some(store) = &stores.store
        && let Ok(mut s) = store.lock().map_err(|_| eyre!("store lock poisoned"))
        && let Err(e) = s.create(session.clone())
    {
        warn!("Worker {} failed to persist session: {}", worker_id, e);
        return Err(eyre!("failed to create agent session: {}", e));
    }
    stores.write_agent_sessions()?.insert(session_id.clone(), session);
    let _ = event_tx.send(DaemonEvent::record_created("agent_session", &session_id));
    debug!("[agent_status] {}: -> Starting (worker spawn)", session_id);
    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session_id, AgentStatus::Starting));

    info!(
        "Worker {} created session {} for Work {}",
        worker_id, session_id, work_id
    );

    // Step 4: Run the full agent lifecycle (worktree, LLM, loop, handback, cleanup)
    run_agent_task(
        session_id,
        AgentType::Implementer,
        stores.clone(),
        event_tx.clone(),
        worktree_mgr.clone(),
    )
    .await;

    Ok(())
}

fn persist_session(stores: &Stores, session: &AgentSession) {
    debug!("persist_session(session_id={})", session.id);
    if let Some(store) = &stores.store
        && let Ok(mut s) = store.lock().map_err(|_| eyre!("store lock poisoned"))
        && let Err(e) = s.update(session.clone())
    {
        warn!("Failed to persist agent session {} to TaskStore: {}", session.id, e);
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig, ToolEntry};
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use taskstore::Store;

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        use crate::agents::AgentType;
        let file_path = dir.join("test-executor.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
    }

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

    /// Build a minimal AgentContext for executor tests.
    fn test_agent_context(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
    ) -> (AgentContext, broadcast::Receiver<DaemonEvent>) {
        let (event_tx, event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.tool_runner.clone(),
            tool_executor: stores.tool_executor.clone(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        (ctx, event_rx)
    }

    /// Build a minimal AgentContext with custom ToolRunner entries.
    fn test_agent_context_with_tools(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
        tool_entries: &[ToolEntry],
    ) -> AgentContext {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(tool_entries)),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(tool_entries)),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        }
    }

    /// Build a minimal AgentContext with a custom Config (e.g., for LockStrict tests).
    fn test_agent_context_with_config(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
        config: Config,
    ) -> AgentContext {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, config);
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.tool_runner.clone(),
            tool_executor: stores.tool_executor.clone(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        }
    }

    /// Helper: create a full Plan→Spec→Phase→Work hierarchy in stores via bridge.
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
            "work.create",
            serde_json::json!({"phase_id": phase_id, "title": "Test WI", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id, wi_id)
    }

    #[tokio::test]
    async fn test_transition_action_uses_correct_param() {
        let dir = TestDir::new("loopr-exec-transparam");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Transition Ready → Abandoned via execute_action (auto-promoted from Draft since acceptance_criteria present)
        // Using Abandoned since InProgress requires assignee which Transition action doesn't support
        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "Abandoned".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        // Should succeed (not "target_status required" error)
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_auto_transitions_draft() {
        let dir = TestDir::new("loopr-exec-autotrans");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Work item starts as Ready (auto-promoted from Draft) — AssignAgent should auto-transition to InProgress
        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        // The agent.start may fail (no LLM key) but the auto-transition should have happened
        // Check work item status is InProgress
        let wi_resp = ctx.bridge.request("work.get", serde_json::json!({"id": wi_id}));
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
        let dir = TestDir::new("loopr-exec-test");

        let entries = vec![ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let stores = test_stores(&dir);
        let ctx = test_agent_context_with_tools(&dir, &stores, AgentType::Implementer, &entries);

        let action = AgentAction::RunTool {
            tool: "echo-test".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::ToolRun(tool_result) = result {
            assert_eq!(tool_result.exit_code, 0);
            assert_eq!(tool_result.stdout.trim(), "hello");
        } else {
            panic!("expected ToolRun result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_write_file() {
        let dir = TestDir::new("loopr-exec-write");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "hello world".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));

        let content = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_file_lock_strict_blocks() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockstrict");

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Implementer, config);

        // Create a lock on "src/main.rs"
        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        // Attempt to write to locked path
        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_lock_advisory_allows() {
        let dir = TestDir::new("loopr-exec-lockadvisory");

        let stores = test_stores(&dir);
        // Default config has LockAdvisory
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Create a lock on "src/main.rs"
        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        // Write should succeed under advisory policy
        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "should work".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
    }

    #[tokio::test]
    async fn test_execute_action_read_file() {
        let dir = TestDir::new("loopr-exec-read");
        std::fs::write(dir.join("read-me.txt"), "file content").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::ReadFile {
            path: "read-me.txt".to_string(),
            offset: None,
            limit: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = result {
            assert!(content.contains("file content"));
        } else {
            panic!("expected FileRead result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_done() {
        let dir = TestDir::new("loopr-exec-done");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::Done {
            summary: "All done".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::Done(summary) = result {
            assert_eq!(summary, "All done");
        } else {
            panic!("expected Done result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_need_help() {
        let dir = TestDir::new("loopr-exec-help");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::NeedHelp {
            reason: "Ambiguous spec".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::NeedHelp(reason) = result {
            assert_eq!(reason, "Ambiguous spec");
        } else {
            panic!("expected NeedHelp result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_unknown_tool() {
        let dir = TestDir::new("loopr-exec-unk");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::RunTool {
            tool: "nonexistent".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock() {
        let dir = TestDir::new("loopr-exec-acqlock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-123".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockAcquired(lock_id) = &result {
            assert!(!lock_id.is_empty());
            assert_ne!(lock_id, "unknown");
        } else {
            panic!("expected LockAcquired result, got {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_lock_conflict() {
        let dir = TestDir::new("loopr-exec-lockconf");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Acquire first lock
        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-100".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LockAcquired(_)));

        // Try to acquire again on same resource — should get ActionError
        let action2 = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-200".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
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
        let dir = TestDir::new("loopr-exec-rellock");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Acquire a lock first
        let acquire_action = AgentAction::AcquireLock {
            resource: "src/lib.rs".to_string(),
            holder_id: "wi-456".to_string(),
        };
        let acquire_result = execute_action(&acquire_action, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = acquire_result {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release it
        let release_action = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        let release_result = execute_action(&release_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::LockReleased(id) = &release_result {
            assert_eq!(id, &lock_id);
        } else {
            panic!("expected LockReleased result, got {:?}", release_result);
        }
    }

    #[tokio::test]
    async fn test_execute_action_acquire_after_release() {
        let dir = TestDir::new("loopr-exec-reacq");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Acquire
        let action1 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let r1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        let lock_id = if let ActionResult::LockAcquired(id) = r1 {
            id
        } else {
            panic!("expected LockAcquired");
        };

        // Release
        let release = AgentAction::ReleaseLock {
            lock_id: lock_id.clone(),
        };
        execute_action(&release, &ctx, &dir, None).await.unwrap();

        // Re-acquire same resource by different holder — should succeed
        let action2 = AgentAction::AcquireLock {
            resource: "src/config.rs".to_string(),
            holder_id: "wi-2".to_string(),
        };
        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(matches!(r2, ActionResult::LockAcquired(_)));
    }

    #[tokio::test]
    async fn test_run_agent_task_lifecycle() {
        let dir = TestDir::new("loopr-agent-task");

        let stores = test_stores(&dir);
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

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
        let dir = TestDir::new("loopr-exec-createplan");
        let stores = test_stores(&dir);

        let action = AgentAction::CreatePlan {
            title: "New Plan".to_string(),
            description: "Plan desc".to_string(),
            acceptance_criteria: "Tests pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
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
        let dir = TestDir::new("loopr-exec-createspec");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreateSpec {
            plan_id,
            title: "New Spec".to_string(),
            description: "Spec desc".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "specs");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_phase() {
        let dir = TestDir::new("loopr-exec-createphase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, spec_id, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::CreatePhase {
            spec_id,
            title: "New Phase".to_string(),
            description: "Phase desc".to_string(),
            order: 2,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "phases");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    #[tokio::test]
    async fn test_execute_create_work() {
        let dir = TestDir::new("loopr-exec-createwi");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);
        // Transition existing WI out of Draft so draft guard doesn't block new creation
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id, "target_status": "Ready", "role": "coordinator"
            }),
        );

        let action = AgentAction::CreateWork {
            phase_id,
            title: "New WI".to_string(),
            description: "WI desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::RecordCreated { collection, id } = &result {
            assert_eq!(collection, "works");
            assert!(!id.is_empty());
        } else {
            panic!("expected RecordCreated, got: {:?}", result);
        }
    }

    // --- Group B: Git operations ---

    #[tokio::test]
    async fn test_execute_commit_success() {
        let dir = TestDir::new("loopr-exec-commit");

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
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "test commit".to_string(),
            paths: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Committed(ref msg) if msg == "test commit"),
            "expected Committed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_commit_specific_paths() {
        let dir = TestDir::new("loopr-exec-commitpaths");

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
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "add a.txt only".to_string(),
            paths: vec!["a.txt".to_string()],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Committed(_)));
    }

    #[tokio::test]
    async fn test_execute_propose_bundle() {
        let dir = TestDir::new("loopr-exec-propbundle");

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
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec!["Implemented feature X".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref desc) if desc == "My bundle"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_propose_bundle_no_work() {
        let dir = TestDir::new("loopr-exec-propnowi");

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
        let stores = test_stores(&dir);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec![],
        };
        // work_id = None should fail
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work_id"));
    }

    // --- Group C: Agent management + domain actions ---

    #[tokio::test]
    async fn test_execute_create_learning_with_all_fields() {
        let dir = TestDir::new("loopr-exec-learning");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Always add tests".to_string(),
            scope: "global".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec!["implementer".to_string()]),
            resource_tags: Some(vec!["src/".to_string()]),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::LearningCreated(ref c) if c == "Always add tests"),
            "expected LearningCreated, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_learning_minimal() {
        let dir = TestDir::new("loopr-exec-learnmin");
        let stores = test_stores(&dir);

        let action = AgentAction::CreateLearning {
            content: "Minimal learning".to_string(),
            scope: "work".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: None,
            resource_tags: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::LearningCreated(_)));
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher() {
        let dir = TestDir::new("loopr-exec-spawnres");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "How does auth work?".to_string(),
            scope_id: "plan-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
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
        let dir = TestDir::new("loopr-exec-valdoc");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ValidateDocument {
            collection: "plan".to_string(),
            id: plan_id,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
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
        let dir = TestDir::new("loopr-exec-triage");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Create a bundle for the work item
        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/test",
                "description": "Test bundle",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Bundle starts as Proposed — TriageBundle transitions Proposed → Triaged
        let action = AgentAction::TriageBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Triaged")),
            "expected Transitioned to Triaged, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle() {
        let dir = TestDir::new("loopr-exec-accept");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Create and transition bundle through Proposed → Triaged → Reviewed
        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept",
                "description": "Accept test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Accepted")),
            "expected Transitioned to Accepted, got: {:?}",
            result
        );
    }

    // --- Group D: Lifecycle paths ---

    #[tokio::test]
    async fn test_write_file_path_escape() {
        let dir = TestDir::new("loopr-exec-escape");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "../../../etc/passwd".to_string(),
            content: "pwned".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = TestDir::new("loopr-exec-readnf");
        let stores = test_stores(&dir);

        let action = AgentAction::ReadFile {
            path: "nonexistent.txt".to_string(),
            offset: None,
            limit: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_transition_role_inference() {
        let dir = TestDir::new("loopr-exec-roleinfer");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Transition without explicit role — should infer from agent type
        // WI is already Ready (auto-promoted from Draft since acceptance_criteria present)
        // Using Abandoned since InProgress requires assignee which Transition action doesn't support
        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id,
            target_status: "Abandoned".to_string(),
            role: None, // Should infer "coordinator" from AgentType::Coordinator
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(_)),
            "expected Transitioned with inferred role, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = TestDir::new("loopr-exec-writedirs");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "deep/nested/dir/file.txt".to_string(),
            content: "nested content".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
        let content = std::fs::read_to_string(dir.join("deep/nested/dir/file.txt")).unwrap();
        assert_eq!(content, "nested content");
    }

    #[tokio::test]
    async fn test_search_code_action() {
        let dir = TestDir::new("loopr-exec-searchcode");
        std::fs::write(dir.join("example.rs"), "fn main() { println!(\"hello\"); }").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::SearchCode {
            pattern: "fn main".to_string(),
            glob: None,
            path: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("fn main"))
                || matches!(result, ActionResult::ActionError(_)),
            "got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_list_directory_action() {
        let dir = TestDir::new("loopr-exec-listdir");
        std::fs::write(dir.join("file1.txt"), "a").unwrap();
        std::fs::write(dir.join("file2.txt"), "b").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ListDirectory { path: ".".to_string() };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("file1.txt")),
            "got: {:?}",
            result
        );
    }

    // --- Task #6: Additional coverage tests ---

    #[tokio::test]
    async fn test_run_agent_task_coordinator_restart_loop() {
        crate::prompts::init_defaults();
        // Coordinator restart loop: will fail due to no API key. Cancel the session during restart
        // to exercise the cancellation-during-restart path.
        let dir = TestDir::new("loopr-exec-coordrestart");

        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            agents: crate::config::AgentConfig {
                coordinator: crate::config::CoordinatorConfig {
                    idle_interval_secs: 0,
                    active_interval_secs: 0,
                    ..crate::config::CoordinatorConfig::default()
                },
                ..crate::config::AgentConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(&dir).unwrap();
        let mut custom_stores = Stores::new();
        custom_stores.store = Some(Arc::new(StdMutex::new(store)));
        custom_stores.config = config;
        let stores = Arc::new(custom_stores);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentType::Coordinator, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Cancel the session after a short delay so the restart loop exits early
        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                // Force to Running first (needed for transition to Cancelled)
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentType::Coordinator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        // Should reach a terminal state (either Failed from restarts or terminal from cancel)
        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "coordinator should be in terminal state");
    }

    #[tokio::test]
    async fn test_run_agent_task_researcher_flow() {
        crate::prompts::init_defaults();
        // Researcher flow — pre-cancel the session so it exits quickly from the loop
        let dir = TestDir::new("loopr-exec-resflow");

        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".to_string());
        session.target_id = Some("plan-1".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Cancel after short delay so the researcher exits quickly
        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentType::Researcher,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "researcher should reach terminal state");
    }

    #[tokio::test]
    async fn test_run_agent_task_integrator_flow() {
        crate::prompts::init_defaults();
        // Integrator flow — pre-cancel the session so it exits quickly from the loop
        let dir = TestDir::new("loopr-exec-integflow");

        // Use custom config with interval_secs=0 so the integrator loop doesn't block
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            integrator: crate::config::IntegratorConfig {
                interval_secs: 0,
                enabled: true,
                ..crate::config::IntegratorConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(&dir).unwrap();
        let mut custom_stores = Stores::new();
        custom_stores.store = Some(Arc::new(StdMutex::new(store)));
        custom_stores.config = config;
        let stores = Arc::new(custom_stores);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let session = AgentSession::new(AgentType::Integrator, "test-model".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        // Cancel after short delay so the integrator exits quickly
        let stores_clone = stores.clone();
        let sid_clone = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid_clone) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        run_agent_task(
            session_id.clone(),
            AgentType::Integrator,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal(), "integrator should reach terminal state");
    }

    #[tokio::test]
    async fn test_run_agent_task_worktree_cleanup() {
        // Implementer with work_id should attempt worktree creation and cleanup
        let dir = TestDir::new("loopr-exec-wtcleanup");

        // Initialize git repo (needed for worktree operations)
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

        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));

        let mut session = AgentSession::new(AgentType::Implementer, "test-model".to_string());
        session.work_id = Some("wi-test-123".to_string());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        run_agent_task(
            session_id.clone(),
            AgentType::Implementer,
            stores.clone(),
            event_tx,
            worktree_mgr,
        )
        .await;

        // Should reach terminal state (will fail due to no API key, but worktree path is exercised)
        let sessions = stores.agent_sessions.read().unwrap();
        let session = sessions.get(&session_id).unwrap();
        assert!(session.status.is_terminal());
    }

    #[tokio::test]
    async fn test_transition_role_inference_all_collections() {
        // Test normalize_collection with all collection variants
        let dir = TestDir::new("loopr-exec-transall");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, spec_id, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        // Test "plans" (plural) → normalizes to "plan"
        let action = AgentAction::Transition {
            collection: "plans".to_string(),
            id: plan_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("plans")),
            "expected Transitioned for plans, got: {:?}",
            result
        );

        // Test "specs" (plural) → normalizes to "spec"
        let action = AgentAction::Transition {
            collection: "specs".to_string(),
            id: spec_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));

        // Test "phases" (plural) → normalizes to "phase"
        let action = AgentAction::Transition {
            collection: "phases".to_string(),
            id: phase_id.clone(),
            target_status: "abandoned".to_string(),
            role: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Transitioned(_)));
    }

    #[tokio::test]
    async fn test_transition_assignee_validation_for_inprogress() {
        let dir = TestDir::new("loopr-exec-assignee");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Attempt InProgress transition without assignee using Transition action (role = coordinator)
        // This should fail because InProgress requires an assignee
        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: wi_id.clone(),
            target_status: "InProgress".to_string(),
            role: Some("coordinator".to_string()),
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        // Should fail (transition error) since InProgress requires assignee
        assert!(result.is_err(), "InProgress without assignee should fail: {:?}", result);
    }

    #[tokio::test]
    async fn test_execute_create_plan_error_path() {
        let dir = TestDir::new("loopr-exec-plnerr");
        let stores = test_stores(&dir);

        // Create a first plan (Draft)
        let action1 = AgentAction::CreatePlan {
            title: "First Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        // Try to create another plan — should get ActionError due to draft-awareness guard
        let action2 = AgentAction::CreatePlan {
            title: "Second Plan".to_string(),
            description: "desc".to_string(),
            acceptance_criteria: "pass".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Plan already exists")),
            "expected draft-awareness error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_spec_error_path() {
        let dir = TestDir::new("loopr-exec-specerr");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (plan_id, _, _, _) = create_test_hierarchy(&ctx.bridge);

        // The hierarchy already created a Draft spec under this plan (but it was transitioned to Active).
        // Create a new spec (Draft) under same plan
        let action1 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "New Spec".to_string(),
            description: "desc".to_string(),
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        // Try to create another spec under same plan — should get draft-awareness error
        let action2 = AgentAction::CreateSpec {
            plan_id: plan_id.clone(),
            title: "Another Spec".to_string(),
            description: "desc".to_string(),
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Draft Spec already exists")),
            "expected draft-awareness error for spec, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_execute_create_phase_error_path() {
        let dir = TestDir::new("loopr-exec-phaseerr");
        let stores = test_stores(&dir);

        // Use a nonexistent spec_id — should get error from bridge
        let action = AgentAction::CreatePhase {
            spec_id: "nonexistent-spec".to_string(),
            title: "New Phase".to_string(),
            description: "desc".to_string(),
            order: 1,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_phase failed")),
            "expected error for nonexistent spec, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_create_work_error_path() {
        let dir = TestDir::new("loopr-exec-wierr");
        let stores = test_stores(&dir);

        // Use a nonexistent phase_id — should get error from bridge
        let action = AgentAction::CreateWork {
            phase_id: "nonexistent-phase".to_string(),
            title: "New WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("create_work failed")),
            "expected error for nonexistent phase, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle_full() {
        // Test triage on a bundle that's not in the right state (error path)
        let dir = TestDir::new("loopr-exec-triagefull");
        let stores = test_stores(&dir);

        // Triage a nonexistent bundle (with valid bd- prefix to pass validation)
        let action = AgentAction::TriageBundle {
            bundle_id: "bd-nonexistent".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("triage_bundle failed")),
            "expected triage error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_triage_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-triage-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::TriageBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-accept-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let action = AgentAction::AcceptBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_accept_bundle_full() {
        // Test accept on a bundle that's not in the right state (error path)
        let dir = TestDir::new("loopr-exec-acceptfull");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Create a bundle but don't triage it — accept should fail
        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accepterr",
                "description": "Accept err test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("accept_bundle failed")),
            "expected accept error for un-triaged bundle, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_spawn_researcher_via_action() {
        // Test SpawnResearcher action (exercises the spawn path)
        let dir = TestDir::new("loopr-exec-spawnres2");
        let stores = test_stores(&dir);

        let action = AgentAction::SpawnResearcher {
            query: "What patterns are used in the codebase?".to_string(),
            scope_id: "spec-1".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        // agent.start will either succeed or fail (no LLM key) — both are acceptable
        assert!(
            matches!(result, ActionResult::AgentSpawned { ref agent_type, .. } if agent_type == "researcher")
                || matches!(result, ActionResult::ActionError(_)),
            "expected AgentSpawned or ActionError, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_conflict_policy_ignore() {
        // Under LockAdvisory (default), writes to locked paths should succeed
        let dir = TestDir::new("loopr-exec-lockign");
        let stores = test_stores(&dir);
        // Default config: LockAdvisory
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Create a lock on the exact file we want to write to
        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "locked.txt", "holder_id": "agent-other", "granted_by": "agent-other" }),
        );

        // Write to the locked file — should succeed under advisory policy
        let action = AgentAction::WriteFile {
            path: "locked.txt".to_string(),
            content: "advisory allows this".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileWritten(_)),
            "expected write to succeed under advisory policy, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_conflict_policy_warn() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        // Under LockStrict, writes to locked paths should return ActionError
        let dir = TestDir::new("loopr-exec-lockwarn");
        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentType::Coordinator, config);

        // Create a lock on the path
        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "strict.txt", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );

        // Write to locked path — should be blocked
        let action = AgentAction::WriteFile {
            path: "strict.txt".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked") && msg.contains("LockStrict")),
            "expected lock-blocked error, got: {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_worktree_base_no_ticks() {
        let dir = TestDir::new("loopr-wt-base-none");
        let stores = test_stores(&dir);
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[test]
    fn test_resolve_worktree_base_no_published_ticks() {
        let dir = TestDir::new("loopr-wt-base-nopub");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.integration_sha = Some("abc123".to_string());
            // Status is Open, not Published
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[test]
    fn test_resolve_worktree_base_picks_latest_published() {
        let dir = TestDir::new("loopr-wt-base-latest");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();

            let mut t1 = crate::domain::tick::Tick::new(1);
            t1.status = crate::domain::tick::TickStatus::Published;
            t1.integration_sha = Some("sha_tick_1".to_string());
            ticks.insert(t1.id.clone(), t1);

            let mut t2 = crate::domain::tick::Tick::new(3);
            t2.status = crate::domain::tick::TickStatus::Published;
            t2.integration_sha = Some("sha_tick_3".to_string());
            ticks.insert(t2.id.clone(), t2);

            let mut t3 = crate::domain::tick::Tick::new(2);
            t3.status = crate::domain::tick::TickStatus::Published;
            t3.integration_sha = Some("sha_tick_2".to_string());
            ticks.insert(t3.id.clone(), t3);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "sha_tick_3");
    }

    #[test]
    fn test_resolve_worktree_base_published_without_sha_falls_back() {
        let dir = TestDir::new("loopr-wt-base-nosha");
        let stores = test_stores(&dir);
        {
            let mut ticks = stores.ticks.write().unwrap();
            let mut t = crate::domain::tick::Tick::new(1);
            t.status = crate::domain::tick::TickStatus::Published;
            t.integration_sha = None; // Published but no SHA
            ticks.insert(t.id.clone(), t);
        }
        let base = resolve_worktree_base(&stores);
        assert_eq!(base, "HEAD");
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_not_met() {
        let dir = TestDir::new("loopr-exec-depnotmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        // Create dep_wi (the dependency) — stays in Ready status
        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Create work_wi that depends on dep_wi
        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Try to assign implementer — should fail because dep is not Done
        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::DependencyNotMet { .. }),
            "expected DependencyNotMet, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_assign_agent_dependency_met() {
        let dir = TestDir::new("loopr-exec-depmet");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        // Create dep_wi and manually set to Done by modifying store directly
        let dep_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Dep WI Done",
                "description": "dep desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
            }),
        );
        let dep_id = dep_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Transition dep to Done: Ready→InProgress→Abandoned (shorter path)
        // Then directly set to Done in both stores and TaskStore
        {
            let mut wis = stores.works.write().unwrap();
            if let Some(wi) = wis.get_mut(&dep_id) {
                wi.status = crate::domain::work::WorkStatus::Done;
                wi.updated_at = crate::id::now_millis();
                if let Some(store_arc) = &stores.store {
                    let _ = store_arc.lock().unwrap().update(wi.clone());
                }
            }
        }

        // Create work_wi that depends on dep_wi (now Done)
        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "phase_id": phase_id,
                "title": "Work WI With Met Dep",
                "description": "work desc",
                "resource_tags": ["src/"],
                "acceptance_criteria": ["pass"],
                "dependencies": [dep_id],
            }),
        );
        let wi_id = wi_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Try to assign implementer — should succeed because dep is Done
        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(
            matches!(result, ActionResult::AgentSpawned { .. }),
            "expected AgentSpawned, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_create_work_with_dependencies() {
        let dir = TestDir::new("loopr-exec-wideps");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, wi_id) = create_test_hierarchy(&ctx.bridge);

        // Create a second WI that depends on the first
        let action = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Dependent WI".to_string(),
            description: "depends on first".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![wi_id.clone()],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();

        assert!(matches!(result, ActionResult::RecordCreated { .. }));

        // Verify the dependency was stored
        if let ActionResult::RecordCreated { id, .. } = result {
            let wi = stores.works.read().unwrap().get(&id).cloned().unwrap();
            assert_eq!(wi.dependencies, vec![wi_id]);
        }
    }

    #[tokio::test]
    async fn test_create_work_duplicate_rejected() {
        let dir = TestDir::new("loopr-exec-widup");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        // Create first WI
        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result1 = execute_action(&action1, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result1, ActionResult::RecordCreated { .. }));

        // Create duplicate WI with same title — should fail
        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Unique WI".to_string(),
            description: "different desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected duplicate error, got: {:?}",
            result2
        );
    }

    #[tokio::test]
    async fn test_create_work_duplicate_case_insensitive() {
        let dir = TestDir::new("loopr-exec-widupcase");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        let (_, _, phase_id, _) = create_test_hierarchy(&ctx.bridge);

        // Create first WI
        let action1 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "Add Login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let _ = execute_action(&action1, &ctx, &dir, None).await;

        // Create duplicate with different case — should also fail
        let action2 = AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: "add login".to_string(),
            description: "desc".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["pass".to_string()],
            dependencies: vec![],
        };
        let result2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result2, ActionResult::ActionError(ref msg) if msg.contains("Duplicate")),
            "expected case-insensitive duplicate error, got: {:?}",
            result2
        );
    }

    // --- Fix #1: resolve_latest_published_tick_id tests ---

    #[test]
    fn test_resolve_latest_published_tick_id_none_at_bootstrap() {
        let dir = TestDir::new("loopr-exec-tickid-none");

        let stores = test_stores(&dir);

        // No ticks exist — should return None
        let result = resolve_latest_published_tick_id(&stores);
        assert!(result.is_none(), "expected None at bootstrap, got: {:?}", result);
    }

    #[test]
    fn test_resolve_latest_published_tick_id_returns_published() {
        let dir = TestDir::new("loopr-exec-tickid-pub");

        let stores = test_stores(&dir);

        // Insert a Published tick directly into stores
        let tick = crate::domain::tick::Tick {
            id: "tick-1".to_string(),
            number: 1,
            status: TickStatus::Published,
            bundle_ids: vec![],
            attempted_bundle_ids: vec![],
            integration_sha: Some("abc123".to_string()),
            validation_log: String::new(),
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
        };
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let result = resolve_latest_published_tick_id(&stores);
        assert_eq!(result, Some("tick-1".to_string()));
    }

    #[tokio::test]
    async fn test_propose_bundle_includes_base_tick_id() {
        let dir = TestDir::new("loopr-exec-propbase");

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
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Insert a Published tick
        let tick = crate::domain::tick::Tick {
            id: "tick-pub-1".to_string(),
            number: 1,
            status: TickStatus::Published,
            bundle_ids: vec![],
            attempted_bundle_ids: vec![],
            integration_sha: Some("abc123".to_string()),
            validation_log: String::new(),
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
        };
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Bundle with base tick".to_string(),
            claims: vec!["claim".to_string()],
        };
        // The call succeeds — bundle.create receives base_tick_id and accepts it
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Bundle with base tick"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_propose_bundle_uses_deterministic_branch_name() {
        // F2: ProposeBundle should use format!("agent/{}", work_id) not git rev-parse
        let dir = TestDir::new("loopr-exec-f2branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Create hierarchy so bundle.create has a valid work_id
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);
        // Transition work to InProgress
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator"}),
        );

        // Initialize a git repo in the dir so commit works
        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("impl.txt"), "implementation").unwrap();
        let _ = tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        let action = AgentAction::ProposeBundle {
            description: "Test bundle".to_string(),
            claims: vec!["implemented feature".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        // The bundle should be created with branch_name "agent/<wi_id>"
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed, got: {:?}",
            result
        );
        // Verify the bundle's branch name in stores
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().next().expect("should have one bundle");
        assert_eq!(
            bundle.branch_name,
            format!("agent/{}", wi_id),
            "bundle branch should be deterministic agent/<work_id>"
        );
    }

    // --- determine_work_handback tests ---

    #[test]
    fn test_handback_succeeded_returns_in_review() {
        let dir = TestDir::new("loopr-handback-ok");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", true);
        assert_eq!(result, Some("InReview"));
    }

    #[test]
    fn test_handback_failed_no_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-nobd");
        let stores = test_stores(&dir);
        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[test]
    fn test_handback_failed_with_active_bundle_returns_in_review() {
        let dir = TestDir::new("loopr-handback-actbd");
        let stores = test_stores(&dir);

        // Insert an Accepted bundle for the work
        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("InReview"));
    }

    #[test]
    fn test_handback_failed_all_rejected_bundles_returns_blocked() {
        let dir = TestDir::new("loopr-handback-rejbd");
        let stores = test_stores(&dir);

        // Insert a Rejected bundle for the work
        let mut bundle = crate::domain::bundle::Bundle::new(
            "wi-1".to_string(),
            None,
            "agent/wi-1".to_string(),
            vec!["claim".into()],
        );
        bundle.status = BundleStatus::Rejected;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    #[test]
    fn test_handback_failed_sibling_active_returns_none() {
        let dir = TestDir::new("loopr-handback-sib");
        let stores = test_stores(&dir);

        // Insert a non-terminal sibling implementer session for the same work_id
        let mut sibling = AgentSession::new(AgentType::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, None);
    }

    #[test]
    fn test_handback_failed_sibling_terminal_checks_bundles() {
        let dir = TestDir::new("loopr-handback-sibterm");
        let stores = test_stores(&dir);

        // Insert a terminal sibling implementer session — should NOT count as active
        let mut sibling = AgentSession::new(AgentType::Implementer, "test-model".into());
        sibling.work_id = Some("wi-1".to_string());
        sibling.transition_to(AgentStatus::Running).unwrap();
        sibling.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(sibling.id.clone(), sibling);

        // No bundles → Blocked
        let result = determine_work_handback(&stores, "wi-1", "sess-1", false);
        assert_eq!(result, Some("Blocked"));
    }

    // --- ReadFile dedup tests ---

    #[tokio::test]
    async fn test_read_file_dedup_returns_unchanged_on_second_read() {
        let dir = TestDir::new("loopr-exec-dedup");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        // First read: returns file content
        let r1 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r1 {
            assert!(content.contains("line1"), "first read should return content");
        } else {
            panic!("expected FileRead, got: {:?}", r1);
        }

        // Second read (same file, unchanged): returns dedup message
        let r2 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                content.contains("File unchanged since last read"),
                "second read should return dedup message, got: {}",
                content
            );
            assert!(content.contains("3"), "should mention total lines");
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_write() {
        let dir = TestDir::new("loopr-exec-dedup-write");
        std::fs::write(dir.join("target.rs"), "original\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        // First read: populate cache
        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        // Write to the file: should invalidate cache
        let write_action = AgentAction::WriteFile {
            path: "target.rs".to_string(),
            content: "updated\n".to_string(),
        };
        execute_action(&write_action, &ctx, &dir, None).await.unwrap();

        // Third read: should return fresh content, not dedup
        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("updated"),
                "read after write should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after write");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_edit() {
        let dir = TestDir::new("loopr-exec-dedup-edit");
        std::fs::write(dir.join("target.rs"), "hello world\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        // First read: populate cache
        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        // Edit the file: should invalidate cache
        let edit_action = AgentAction::EditFile {
            path: "target.rs".to_string(),
            old_string: "hello".to_string(),
            new_string: "goodbye".to_string(),
        };
        execute_action(&edit_action, &ctx, &dir, None).await.unwrap();

        // Read again: should return fresh content
        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("goodbye"),
                "read after edit should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after edit");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_different_offset_no_dedup() {
        let dir = TestDir::new("loopr-exec-dedup-offset");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentType::Implementer);

        let action1 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };
        let action2 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: Some(1),
            limit: None,
        };

        // Read with no offset
        execute_action(&action1, &ctx, &dir, None).await.unwrap();

        // Read with explicit offset=1: different cache key, should NOT dedup
        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                !content.contains("File unchanged"),
                "different offset should not dedup, got: {}",
                content
            );
            assert!(content.contains("line1"));
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }
}
