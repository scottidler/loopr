use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, eyre};
use log::{info, warn};

use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::{AgentAction, AgentSession, AgentType};
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::role::Role;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolRunner;

/// Trait for LLM calls — allows mocking in tests.
/// Phase 4 provides the real streaming implementation (`AgentLlmClient`).
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Call the LLM with a system prompt and user message, return the full response text.
    async fn call(&self, system_prompt: &str, user_message: &str) -> Result<String>;
}

/// Parse the LLM response into a list of agent actions.
/// Tolerates prose before/after the JSON array — finds `[` and its matching `]`.
pub fn parse_actions(response: &str) -> Result<Vec<AgentAction>> {
    // Try direct parse first
    if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(response) {
        return Ok(actions);
    }

    // Find the first `[` and try increasingly larger `]` matches until one parses.
    // This handles: prose before JSON, code fences, multiple arrays (takes first valid one).
    let trimmed = response.trim();
    if let Some(start) = trimmed.find('[') {
        let rest = &trimmed[start..];
        // Try every `]` from nearest to farthest
        for (offset, _) in rest.match_indices(']') {
            let candidate = &rest[..=offset];
            if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(candidate)
                && !actions.is_empty()
            {
                return Ok(actions);
            }
            // Also try element-by-element
            if let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(candidate) {
                let actions: Vec<AgentAction> = values
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect();
                if !actions.is_empty() {
                    return Ok(actions);
                }
            }
        }
    }

    let snippet: String = response.chars().take(500).collect();
    Err(eyre!(
        "failed to parse agent actions from LLM response (response snippet: {})",
        snippet
    ))
}

/// Build a focused state summary for the implementer: active locks and sibling agents.
pub fn build_implementer_summary(stores: &Stores, work_item_id: &str) -> String {
    use crate::domain::lock::LockStatus;

    let mut summary = String::with_capacity(512);

    // Active locks on resources
    {
        let locks = stores.locks.read().unwrap();
        let active: Vec<_> = locks.values().filter(|l| l.status == LockStatus::Active).collect();
        if !active.is_empty() {
            summary.push_str("### Active Locks\n");
            for l in &active {
                summary.push_str(&format!("- {} (holder: {})\n", l.resource, l.holder_id));
            }
            summary.push('\n');
        }
    }

    // Active agents working on sibling work items
    {
        let sessions = stores.agent_sessions.read().unwrap();
        let siblings: Vec<_> = sessions
            .values()
            .filter(|s| {
                !s.status.is_terminal() && s.work_item_id.as_deref() != Some(work_item_id) && s.work_item_id.is_some()
            })
            .collect();
        if !siblings.is_empty() {
            summary.push_str("### Sibling Agents\n");
            for s in &siblings {
                summary.push_str(&format!(
                    "- {} {} (wi: {})\n",
                    s.agent_type,
                    s.status,
                    s.work_item_id.as_deref().unwrap_or("?")
                ));
            }
            summary.push('\n');
        }
    }

    summary
}

/// Outcome of a single implementer iteration.
#[derive(Debug)]
pub enum IterationOutcome {
    Continue(String),
    Done(String),
    NeedHelp(String),
}

/// Shared resources for implementer iterations.
pub struct IterationParams<'a> {
    pub llm: &'a dyn LlmClient,
    pub stores: &'a Arc<Stores>,
    pub tool_runner: &'a ToolRunner,
    pub bridge: &'a AgentIpcBridge,
    pub work_item_id: &'a str,
    pub worktree_path: &'a Path,
    pub session_id: &'a str,
    pub event_tx: &'a broadcast::Sender<DaemonEvent>,
}

/// Run a single implementer iteration: load context → prompt → call LLM → parse → execute.
pub async fn run_iteration(
    params: &IterationParams<'_>,
    iteration: u32,
    previous_summary: Option<String>,
    staleness_note: Option<String>,
) -> Result<IterationOutcome> {
    let state_summary = build_implementer_summary(params.stores, params.work_item_id);
    let assembled = ContextBuilder::new(params.stores, Role::Implementer)
        .load_work_item_hierarchy(params.work_item_id)?
        .with_coordinator_goal()
        .with_state_summary(state_summary)
        .with_tools(params.tool_runner)
        .with_previous_summary(previous_summary)
        .with_staleness_note(staleness_note)
        .with_iteration(iteration)
        .with_footer("Implement the WorkItem described above. Respond with a JSON array of actions.".into())
        .build(&crate::prompts::store().implementer);

    info!(
        "Implementer {} context: ~{} tokens",
        params.session_id, assembled.token_estimate
    );
    let response = params
        .llm
        .call(&assembled.system_prompt, &assembled.user_message)
        .await?;
    info!(
        "Implementer {} raw LLM response ({} chars): {}",
        params.session_id,
        response.len(),
        &response[..response.len().min(800)]
    );
    let actions = parse_actions(&response)?;

    if actions.is_empty() {
        return Err(eyre!("LLM returned empty action list"));
    }

    let mut summaries = Vec::new();
    for action in &actions {
        // Broadcast tool_started event for RunTool actions
        if let AgentAction::RunTool { tool_name, .. } = action {
            let _ = params
                .event_tx
                .send(DaemonEvent::agent_tool_started(params.session_id, tool_name));
        }

        let result = match execute_action(
            action,
            params.tool_runner,
            params.bridge,
            params.worktree_path,
            Some(params.work_item_id),
            AgentType::Implementer,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Implementer {} action failed (non-fatal): {e}", params.session_id);
                ActionResult::ActionError(e.to_string())
            }
        };

        // Broadcast tool_completed event for RunTool results
        if let ActionResult::ToolRun(ref tr) = result {
            let _ = params.event_tx.send(DaemonEvent::agent_tool_completed(
                params.session_id,
                &tr.tool_name,
                tr.exit_code,
                tr.duration_ms,
            ));
        }

        let summary = format_action_summary(action, &result);
        let _ = params
            .event_tx
            .send(DaemonEvent::agent_action_completed(params.session_id, &summary));

        match &result {
            ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
            ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
            _ => {}
        }
        summaries.push(summary);
    }

    Ok(IterationOutcome::Continue(summaries.join("\n")))
}

/// Check a broadcast receiver for `tick.published` events, returning the latest tick ID if found.
fn drain_tick_published(event_rx: &mut broadcast::Receiver<DaemonEvent>) -> Option<String> {
    let mut latest_tick_id: Option<String> = None;
    loop {
        match event_rx.try_recv() {
            Ok(event) if event.event == "tick.published" => {
                if let Some(tid) = event.data.get("tick_id").and_then(|v| v.as_str()) {
                    latest_tick_id = Some(tid.to_string());
                }
            }
            Ok(_) => {} // ignore non-tick events
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                // Missed messages — continue draining from current position
                continue;
            }
        }
    }
    latest_tick_id
}

/// Run the full implementer loop with iteration cap.
pub async fn run_implementer(
    llm: &dyn LlmClient,
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    tool_runner: &ToolRunner,
    bridge: &AgentIpcBridge,
    config: &AgentRoleConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    let work_item_id = session
        .work_item_id
        .as_ref()
        .ok_or_else(|| eyre!("implementer session missing work_item_id"))?
        .clone();

    let worktree_path = session
        .worktree_path
        .as_ref()
        .map(std::path::PathBuf::from)
        .ok_or_else(|| eyre!("implementer session missing worktree_path — was the worktree created?"))?;

    let max_iterations = config.max_iterations;
    let mut previous_summary: Option<String> = None;
    let mut event_rx = event_tx.subscribe();

    let params = IterationParams {
        llm,
        stores,
        tool_runner,
        bridge,
        work_item_id: &work_item_id,
        worktree_path: &worktree_path,
        session_id: &session.id,
        event_tx,
    };

    for i in 1..=max_iterations {
        session.iteration = i;
        // Persist iteration to stores so agent list/status reflect progress
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&session.id) {
                s.iteration = session.iteration;
            }
        }
        info!("Implementer {} iteration {}/{}", session.id, i, max_iterations);

        // Check for staleness (tick.published events since last iteration)
        let staleness_note = if let Some(new_tick_id) = drain_tick_published(&mut event_rx) {
            info!(
                "Implementer {} detected stale: new tick {} published",
                session.id, new_tick_id
            );
            let _ = event_tx.send(DaemonEvent::agent_staleness_detected(&session.id, &new_tick_id));
            Some(format!(
                "A new Tick '{}' has been published since your last iteration. \
                 Your worktree may be based on an older Tick. Review your changes \
                 for conflicts and re-run tests before proposing a Bundle. \
                 Use the latest base when proposing.",
                new_tick_id
            ))
        } else {
            None
        };

        match run_iteration(&params, i, previous_summary.clone(), staleness_note).await {
            Ok(IterationOutcome::Done(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, i, &summary));
                info!("Implementer {} completed: {}", session.id, summary);
                return Ok(());
            }
            Ok(IterationOutcome::NeedHelp(reason)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, i, &reason));
                warn!("Implementer {} needs help: {}", session.id, reason);
                return Err(eyre!("agent needs help: {}", reason));
            }
            Ok(IterationOutcome::Continue(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, i, &summary));
                info!("Implementer {} iteration {} done: {}", session.id, i, summary);
                previous_summary = Some(summary);
            }
            Err(e) => {
                warn!("Implementer {} iteration {} failed: {}", session.id, i, e);
                return Err(e);
            }
        }
    }

    Err(eyre!("implementer reached max iterations ({})", max_iterations))
}

/// Max chars of file/tool output to include in the action summary fed back to the LLM.
const MAX_SUMMARY_CONTENT: usize = 4000;

fn truncate_content(content: &str, max: usize) -> String {
    if content.len() <= max {
        content.to_string()
    } else {
        format!("{}...\n[truncated, {} total bytes]", &content[..max], content.len())
    }
}

fn format_action_summary(action: &AgentAction, result: &ActionResult) -> String {
    match result {
        ActionResult::ToolRun(tr) => {
            let mut s = format!("ran {} (exit {})", tr.tool_name, tr.exit_code);
            if !tr.stdout.is_empty() {
                s.push_str(&format!(
                    "\nstdout:\n```\n{}\n```",
                    truncate_content(&tr.stdout, MAX_SUMMARY_CONTENT)
                ));
            }
            if !tr.stderr.is_empty() {
                s.push_str(&format!(
                    "\nstderr:\n```\n{}\n```",
                    truncate_content(&tr.stderr, MAX_SUMMARY_CONTENT)
                ));
            }
            s
        }
        ActionResult::FileWritten(p) => format!("wrote {}", p),
        ActionResult::FileRead(content) => {
            let path = if let AgentAction::ReadFile { path } = action { path.as_str() } else { "?" };
            format!(
                "read {} ({} bytes):\n```\n{}\n```",
                path,
                content.len(),
                truncate_content(content, MAX_SUMMARY_CONTENT)
            )
        }
        ActionResult::Committed(m) => format!("committed: {}", m),
        ActionResult::BundleProposed(d) => format!("proposed bundle: {}", d),
        ActionResult::Transitioned(d) => format!("transitioned: {}", d),
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::LockAcquired(id) => format!("lock acquired: {}", id),
        ActionResult::LockReleased(id) => format!("lock released: {}", id),
        ActionResult::DocumentValidated { verdict, summary, .. } => {
            format!("validated: {} — {}", verdict, summary)
        }
        ActionResult::Done(s) => format!("done: {}", s),
        ActionResult::NeedHelp(r) => format!("need help: {}", r),
        ActionResult::ActionError(e) => format!("ERROR: {}", e),
        ActionResult::RecordCreated { collection, id } => format!("created {}: {}", collection, id),
        ActionResult::AgentSpawned { session_id, agent_type } => {
            format!("spawned {} ({})", agent_type, session_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentType};
    use crate::config::{AgentRoleConfig, Config, ProjectConfig, ToolEntry};
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::work_item::WorkItem;
    use crate::tools::ToolRunner;
    use crate::worktree::manager::WorktreeManager;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;
    use tokio::sync::broadcast;

    /// Mock LLM client that returns a canned response.
    struct MockLlm {
        response: String,
    }

    impl MockLlm {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    /// Mock LLM that fails.
    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Err(eyre!("LLM call failed"))
        }
    }

    fn setup_stores(dir: &Path) -> Arc<Stores> {
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

        // Populate hierarchy
        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let spec = Spec::new(plan_id.clone(), "Test Spec".into(), "A test spec".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let phase = Phase::new(spec_id.clone(), "Test Phase".into(), "A test phase".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let wi = WorkItem::new(phase_id.clone(), "Test WorkItem".into(), "Implement the feature".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        // Add a learning
        let learning = Learning::new(
            phase_id,
            LearningScope::Phase,
            "Previous iteration found a bug in parsing".into(),
        );
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        Arc::new(stores)
    }

    fn get_work_item_id(stores: &Stores) -> String {
        stores.work_items.read().unwrap().keys().next().unwrap().clone()
    }

    fn test_bridge_with_tx(stores: Arc<Stores>, dir: &Path) -> (AgentIpcBridge, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        (bridge, event_tx)
    }

    // --- parse_actions tests ---

    #[test]
    fn test_parse_actions_direct_json() {
        let json = r#"[{"action": "done", "summary": "All done"}]"#;
        let actions = parse_actions(json).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::Done { .. }));
    }

    #[test]
    fn test_parse_actions_wrapped_in_code_block() {
        let response = "Here are the actions:\n```json\n[{\"action\": \"done\", \"summary\": \"done\"}]\n```";
        let actions = parse_actions(response).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn test_parse_actions_multiple() {
        let json = r#"[
            {"action": "write_file", "path": "src/foo.rs", "content": "fn foo() {}"},
            {"action": "run_tool", "tool_name": "test", "args": []},
            {"action": "done", "summary": "Implemented foo"}
        ]"#;
        let actions = parse_actions(json).unwrap();
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn test_parse_actions_invalid() {
        let bad = "This is not JSON at all";
        assert!(parse_actions(bad).is_err());
    }

    #[test]
    fn test_parse_actions_empty_array() {
        let json = "[]";
        let actions = parse_actions(json).unwrap();
        assert!(actions.is_empty());
    }

    // --- context builder integration tests ---

    #[test]
    fn test_context_builder_for_implementer() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-ctx-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let tool_runner = ToolRunner::new(&[ToolEntry {
            name: "test".into(),
            command: "echo ok".into(),
            timeout_secs: 10,
            worktree: true,
        }]);
        let wi_id = get_work_item_id(&stores);

        let assembled = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .with_tools(&tool_runner)
            .with_iteration(1)
            .with_footer("Respond with JSON.".into())
            .build(&crate::prompts::store().implementer);

        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test WorkItem"));
        assert!(assembled.user_message.contains("`test`"));
        assert!(assembled.user_message.contains("Current Iteration: 1"));
        assert!(assembled.system_prompt.contains("Implementer agent"));
    }

    #[test]
    fn test_context_builder_missing_work_item() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-miss-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);

        let result = ContextBuilder::new(&stores, Role::Implementer).load_work_item_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work item not found"));
    }

    // --- run_iteration tests ---

    #[tokio::test]
    async fn test_run_iteration_done() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-iter-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new(r#"[{"action": "done", "summary": "All done"}]"#);
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let outcome = run_iteration(&params, 1, None, None).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s == "All done"));
    }

    #[tokio::test]
    async fn test_run_iteration_need_help() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-help-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new(r#"[{"action": "need_help", "reason": "Ambiguous spec"}]"#);
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let outcome = run_iteration(&params, 1, None, None).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref r) if r == "Ambiguous spec"));
    }

    #[tokio::test]
    async fn test_run_iteration_continue_with_actions() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-cont-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new(
            r#"[
            {"action": "write_file", "path": "test.txt", "content": "hello"},
            {"action": "read_file", "path": "test.txt"}
        ]"#,
        );
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let outcome = run_iteration(&params, 1, None, None).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Continue(_)));
        // File should have been written
        assert!(dir.join("test.txt").exists());
    }

    #[tokio::test]
    async fn test_run_iteration_llm_failure() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-fail-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = FailingLlm;
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let result = run_iteration(&params, 1, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_iteration_bad_response() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-bad-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new("This is not valid JSON");
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let result = run_iteration(&params, 1, None, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_iteration_empty_actions() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-empty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new("[]");
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let result = run_iteration(&params, 1, None, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty action list"));
    }

    // --- run_implementer tests ---

    #[tokio::test]
    async fn test_run_implementer_completes_on_done() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-run-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let config = AgentRoleConfig::default_implementer();

        let llm = MockLlm::new(r#"[{"action": "done", "summary": "Complete"}]"#);

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_item_id = Some(wi_id);
        session.worktree_path = Some(dir.to_string_lossy().into());

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_ok());
        assert_eq!(session.iteration, 1);
    }

    #[tokio::test]
    async fn test_run_implementer_max_iterations() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-max-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3;

        // LLM always returns a write action (no done), so loop should hit max
        let llm = MockLlm::new(r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#);

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_item_id = Some(wi_id);
        session.worktree_path = Some(dir.to_string_lossy().into());

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max iterations"));
        assert_eq!(session.iteration, 3);
    }

    #[tokio::test]
    async fn test_run_implementer_missing_work_item_id() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-nowi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = MockLlm::new("[]");

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        // No work_item_id set

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing work_item_id"));
    }

    #[tokio::test]
    async fn test_run_implementer_need_help_stops() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-helpstop-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let config = AgentRoleConfig::default_implementer();

        let llm = MockLlm::new(r#"[{"action": "need_help", "reason": "Stuck"}]"#);

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_item_id = Some(wi_id);
        session.worktree_path = Some(dir.to_string_lossy().into());

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_instructions() {
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().implementer;
        assert!(prompt.contains("Implementer agent"));
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("run_tool"));
        assert!(prompt.contains("done"));
        assert!(prompt.contains("need_help"));
        assert!(prompt.contains("JSON array"));
    }

    // --- staleness cascade tests ---

    #[test]
    fn test_drain_tick_published_empty() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        drop(tx);
        let result = drain_tick_published(&mut rx);
        assert!(result.is_none());
    }

    #[test]
    fn test_drain_tick_published_finds_tick() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::tick_published("tick-42", "abc123"));
        let result = drain_tick_published(&mut rx);
        assert_eq!(result, Some("tick-42".to_string()));
    }

    #[test]
    fn test_drain_tick_published_returns_latest() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::tick_published("tick-1", "aaa"));
        let _ = tx.send(DaemonEvent::tick_published("tick-2", "bbb"));
        let _ = tx.send(DaemonEvent::tick_published("tick-3", "ccc"));
        let result = drain_tick_published(&mut rx);
        assert_eq!(result, Some("tick-3".to_string()));
    }

    #[test]
    fn test_drain_tick_published_ignores_other_events() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::record_created("work_item", "wi-1"));
        let _ = tx.send(DaemonEvent::transition_completed(
            "bundle",
            "b-1",
            "draft",
            "proposed",
            "implementer",
        ));
        let result = drain_tick_published(&mut rx);
        assert!(result.is_none());
    }

    #[test]
    fn test_drain_tick_published_mixed_events() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::record_created("work_item", "wi-1"));
        let _ = tx.send(DaemonEvent::tick_published("tick-5", "sha5"));
        let _ = tx.send(DaemonEvent::record_updated("bundle", "b-1"));
        let result = drain_tick_published(&mut rx);
        assert_eq!(result, Some("tick-5".to_string()));
    }

    #[test]
    fn test_context_builder_with_staleness() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-stale2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);

        let assembled = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_item_hierarchy(&wi_id)
            .unwrap()
            .with_staleness_note(Some("A new Tick 'tick-99' has been published.".into()))
            .with_iteration(2)
            .build(&crate::prompts::store().implementer);
        assert!(assembled.user_message.contains("Staleness Warning"));
        assert!(assembled.user_message.contains("tick-99"));
    }

    #[tokio::test]
    async fn test_run_iteration_with_staleness_note() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-stale-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let llm = MockLlm::new(r#"[{"action": "done", "summary": "Rebased and done"}]"#);
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-session",
            event_tx: &event_tx,
        };

        let staleness = Some("New tick published, please review.".to_string());
        let outcome = run_iteration(&params, 2, None, staleness).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(_)));
    }

    #[tokio::test]
    async fn test_run_implementer_detects_staleness() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-staleness-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3;

        // Mock LLM that sends tick.published during the first call, so it's available
        // by the time the second iteration starts and drains events.
        struct StalenessLlm {
            call_count: std::sync::atomic::AtomicU32,
            event_tx: broadcast::Sender<DaemonEvent>,
        }

        #[async_trait]
        impl LlmClient for StalenessLlm {
            async fn call(&self, _system: &str, user_msg: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // First iteration: send tick.published, then return a continue action.
                    // The event will be in the broadcast buffer for the next drain.
                    let _ = self.event_tx.send(DaemonEvent::tick_published("tick-new", "newshaval"));
                    Ok(r#"[{"action": "write_file", "path": "x.txt", "content": "v1"}]"#.to_string())
                } else {
                    // Second iteration: should see staleness warning in user message
                    assert!(
                        user_msg.contains("Staleness Warning"),
                        "Expected staleness context in user message"
                    );
                    Ok(r#"[{"action": "done", "summary": "Handled staleness"}]"#.to_string())
                }
            }
        }

        let llm = StalenessLlm {
            call_count: std::sync::atomic::AtomicU32::new(0),
            event_tx: event_tx.clone(),
        };

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_item_id = Some(wi_id);
        session.worktree_path = Some(dir.to_string_lossy().into());

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
    }

    #[tokio::test]
    async fn test_action_results_accumulate() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-accum-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        // LLM returns multiple actions — all summaries should be joined
        let llm = MockLlm::new(
            r#"[
            {"action": "write_file", "path": "a.txt", "content": "aaa"},
            {"action": "write_file", "path": "b.txt", "content": "bbb"},
            {"action": "read_file", "path": "a.txt"}
        ]"#,
        );
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-accum",
            event_tx: &event_tx,
        };

        let outcome = run_iteration(&params, 1, None, None).await.unwrap();
        if let IterationOutcome::Continue(summary) = outcome {
            // All action summaries should be present, joined by newline
            assert!(summary.contains("wrote a.txt"), "missing a.txt summary: {}", summary);
            assert!(summary.contains("wrote b.txt"), "missing b.txt summary: {}", summary);
            assert!(summary.contains("read a.txt"), "missing read summary: {}", summary);
        } else {
            panic!("expected Continue, got: {:?}", outcome);
        }
    }

    #[test]
    fn test_parse_actions_skips_malformed_in_fallback() {
        // Array with one valid and one malformed action — fallback should skip the bad one
        let json = r#"[
            {"action": "done", "summary": "All good"},
            {"action": "totally_bogus", "invalid_field": 42}
        ]"#;
        let actions = parse_actions(json).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::Done { .. }));
    }

    #[test]
    fn test_build_implementer_summary_empty_locks_and_siblings() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-emptysummary-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);

        // No active locks, no sibling agents — summary should be empty
        let summary = build_implementer_summary(&stores, &wi_id);
        assert!(summary.is_empty(), "expected empty summary, got: {}", summary);
    }

    #[tokio::test]
    async fn test_run_iteration_done_stops_remaining_actions() {
        // When a Done action appears mid-list, subsequent actions should not execute
        let dir = std::env::temp_dir().join(format!("loopr-impl-donestop-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        // Done comes first, then a write_file that should NOT execute
        let llm = MockLlm::new(
            r#"[
            {"action": "done", "summary": "Early exit"},
            {"action": "write_file", "path": "should_not_exist.txt", "content": "nope"}
        ]"#,
        );
        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-donestop",
            event_tx: &event_tx,
        };

        let outcome = run_iteration(&params, 1, None, None).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s == "Early exit"));
        // The file after Done should NOT have been written
        assert!(!dir.join("should_not_exist.txt").exists());
    }

    #[test]
    fn test_drain_tick_published_closed_channel() {
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(4);
        // Send a tick, then close
        let _ = tx.send(DaemonEvent::tick_published("tick-closed", "sha"));
        drop(tx);
        // Should still find the tick before hitting Closed
        let result = drain_tick_published(&mut rx);
        assert_eq!(result, Some("tick-closed".to_string()));
    }

    #[test]
    fn test_drain_tick_published_lagged_channel() {
        // Create a very small buffer so sending more messages than capacity causes lag
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(2);
        // Send 4 messages to overflow the buffer of 2 — receiver will lag
        let _ = tx.send(DaemonEvent::record_created("x", "1"));
        let _ = tx.send(DaemonEvent::record_created("x", "2"));
        let _ = tx.send(DaemonEvent::record_created("x", "3"));
        let _ = tx.send(DaemonEvent::tick_published("tick-lagged", "sha"));
        // drain should recover from lag and find the tick
        let result = drain_tick_published(&mut rx);
        assert_eq!(result, Some("tick-lagged".to_string()));
    }

    #[test]
    fn test_format_action_summary_truncated_tool_output() {
        use crate::tools::ToolResult;

        // Create stdout that exceeds MAX_SUMMARY_CONTENT (4000 chars)
        let long_output = "x".repeat(5000);
        let tr = ToolResult {
            tool_name: "big_tool".to_string(),
            exit_code: 0,
            stdout: long_output.clone(),
            stderr: String::new(),
            duration_ms: 100,
            truncated: false,
        };
        let action = AgentAction::RunTool {
            tool_name: "big_tool".to_string(),
            args: vec![],
        };
        let summary = format_action_summary(&action, &ActionResult::ToolRun(tr));
        assert!(summary.contains("ran big_tool (exit 0)"));
        assert!(summary.contains("[truncated, 5000 total bytes]"));
        // Should NOT contain the full 5000-char output
        assert!(summary.len() < long_output.len());
    }

    #[tokio::test]
    async fn test_run_implementer_session_iteration_persisted() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-persist-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 2;

        // LLM returns a write on first call, done on second
        struct TwoShotLlm {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait]
        impl LlmClient for TwoShotLlm {
            async fn call(&self, _system: &str, _user: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok(r#"[{"action": "write_file", "path": "iter.txt", "content": "1"}]"#.to_string())
                } else {
                    Ok(r#"[{"action": "done", "summary": "Persisted iteration"}]"#.to_string())
                }
            }
        }

        let llm = TwoShotLlm {
            call_count: std::sync::atomic::AtomicU32::new(0),
        };

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        let session_id = session.id.clone();
        session.work_item_id = Some(wi_id);
        session.worktree_path = Some(dir.to_string_lossy().into());

        // Insert session into stores so the loop can update it
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session.clone());

        let result = run_implementer(&llm, &mut session, &stores, &tool_runner, &bridge, &config, &event_tx).await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
        assert_eq!(session.iteration, 2);

        // Verify the iteration was persisted in stores
        let sessions = stores.agent_sessions.read().unwrap();
        let stored = sessions.get(&session_id).unwrap();
        assert_eq!(stored.iteration, 2, "Stored iteration should be 2");
    }

    #[tokio::test]
    async fn test_implementer_context_includes_goal() {
        use crate::domain::coordinator_goal::CoordinatorGoal;
        use std::sync::Mutex as StdMutex2;

        let dir = std::env::temp_dir().join(format!("loopr-impl-goal-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);

        // Set a coordinator goal
        let goal = CoordinatorGoal::new("Build a REST API for user management".to_string());
        stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);

        // Use a CapturingLlm that captures the prompt
        struct CapturingLlm {
            captured_user_msg: StdMutex2<Option<String>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn call(&self, _system_prompt: &str, user_message: &str) -> Result<String> {
                *self.captured_user_msg.lock().unwrap() = Some(user_message.to_string());
                Ok(r#"[{"action": "done", "summary": "done"}]"#.to_string())
            }
        }

        let llm = CapturingLlm {
            captured_user_msg: StdMutex2::new(None),
        };
        let tool_runner = ToolRunner::new(&[]);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);

        let params = IterationParams {
            llm: &llm,
            stores: &stores,
            tool_runner: &tool_runner,
            bridge: &bridge,
            work_item_id: &wi_id,
            worktree_path: &dir,
            session_id: "test-goal",
            event_tx: &event_tx,
        };

        let _ = run_iteration(&params, 1, None, None).await.unwrap();

        let captured = llm.captured_user_msg.lock().unwrap();
        let user_msg = captured.as_ref().expect("LLM should have been called");
        assert!(
            user_msg.contains("Project Goal"),
            "expected 'Project Goal' in context, got: {}",
            &user_msg[..user_msg.len().min(500)]
        );
        assert!(
            user_msg.contains("REST API for user management"),
            "expected goal text in context"
        );
    }

    #[test]
    fn test_build_implementer_summary_with_locks_and_siblings() {
        use crate::domain::lock::{Lock, LockStatus};
        let dir = std::env::temp_dir().join(format!("loopr-impl-fullsummary-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let wi_id = get_work_item_id(&stores);

        // Add an active lock
        let mut lock = Lock::new("src/main.rs".into(), "wi-other".into(), "coordinator".into());
        lock.status = LockStatus::Active;
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        // Add a sibling agent session (different work_item_id, not terminal)
        let mut session =
            crate::agents::AgentSession::new(crate::agents::AgentType::Implementer, "test-model".to_string());
        session.work_item_id = Some("wi-sibling".to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let summary = build_implementer_summary(&stores, &wi_id);
        assert!(summary.contains("### Active Locks"), "should contain locks section");
        assert!(summary.contains("src/main.rs"), "should contain lock resource");
        assert!(
            summary.contains("### Sibling Agents"),
            "should contain siblings section"
        );
        assert!(summary.contains("wi-sibling"), "should contain sibling work_item_id");
    }
}
