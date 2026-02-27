use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{Result, eyre};
use log::{info, warn};

use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::{AgentAction, AgentSession};
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::learning::LearningScope;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolRunner;

/// Trait for LLM calls — allows mocking in tests.
/// Phase 4 provides the real streaming implementation (`AgentLlmClient`).
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Call the LLM with a system prompt and user message, return the full response text.
    async fn call(&self, system_prompt: &str, user_message: &str) -> Result<String>;
}

/// Context loaded for an Implementer iteration.
#[derive(Debug, Clone)]
pub struct ImplementerContext {
    pub plan_title: String,
    pub plan_description: String,
    pub spec_title: String,
    pub spec_description: String,
    pub phase_title: String,
    pub phase_description: String,
    pub work_item_title: String,
    pub work_item_description: String,
    pub learnings: Vec<String>,
    pub available_tools: Vec<String>,
    pub previous_summary: Option<String>,
    pub staleness_note: Option<String>,
}

/// Load the hierarchy context for a WorkItem from Stores.
pub fn load_context(
    stores: &Stores,
    work_item_id: &str,
    tool_runner: &ToolRunner,
    previous_summary: Option<String>,
) -> Result<ImplementerContext> {
    let work_items = stores.work_items.read().unwrap();
    let wi = work_items
        .get(work_item_id)
        .ok_or_else(|| eyre!("work item not found: {}", work_item_id))?;

    let phases = stores.phases.read().unwrap();
    let phase = phases
        .get(&wi.phase_id)
        .ok_or_else(|| eyre!("phase not found: {}", wi.phase_id))?;

    let specs = stores.specs.read().unwrap();
    let spec = specs
        .get(&phase.spec_id)
        .ok_or_else(|| eyre!("spec not found: {}", phase.spec_id))?;

    let plans = stores.plans.read().unwrap();
    let plan = plans
        .get(&spec.plan_id)
        .ok_or_else(|| eyre!("plan not found: {}", spec.plan_id))?;

    // Gather learnings scoped to this work item and its ancestors
    let learnings_map = stores.learnings.read().unwrap();
    let learnings: Vec<String> = learnings_map
        .values()
        .filter(|l| {
            (l.scope == LearningScope::WorkItem && l.source_id == work_item_id)
                || (l.scope == LearningScope::Phase && l.source_id == phase.id)
                || (l.scope == LearningScope::Spec && l.source_id == spec.id)
                || (l.scope == LearningScope::Plan && l.source_id == plan.id)
                || l.scope == LearningScope::Global
        })
        .map(|l| l.content.clone())
        .collect();

    let available_tools = tool_runner.available_tools().into_iter().map(String::from).collect();

    Ok(ImplementerContext {
        plan_title: plan.title.clone(),
        plan_description: plan.description.clone(),
        spec_title: spec.title.clone(),
        spec_description: spec.description.clone(),
        phase_title: phase.title.clone(),
        phase_description: phase.description.clone(),
        work_item_title: wi.title.clone(),
        work_item_description: wi.description.clone(),
        learnings,
        available_tools,
        previous_summary,
        staleness_note: None,
    })
}

const SYSTEM_PROMPT: &str = r#"You are an Implementer agent in the Loopr development orchestrator. Your role is to implement a specific WorkItem by writing code in a Git worktree.

## Your Capabilities

You can perform the following actions (respond with a JSON array of actions):

1. `write_file` — Create or overwrite a file in the worktree
2. `read_file` — Read a file from the worktree
3. `run_tool` — Execute a configured tool (test, clippy, fmt, build)
4. `commit` — Create a git commit with specified files
5. `propose_bundle` — Submit your work as a Bundle for review
6. `create_learning` — Record a discovery or insight
7. `done` — Signal that you've completed this iteration
8. `need_help` — Request human Coordinator intervention

## Rules

- Work incrementally. Don't try to implement everything in one iteration.
- Run tests after making changes. Fix failures before proposing a Bundle.
- Create small, focused commits with descriptive messages.
- When you encounter an ambiguity, create a Learning and ask for help.
- Your code must pass `clippy` and `fmt --check` before proposing a Bundle.
- Do not modify files outside the WorkItem's scope.

## Output Format

Respond with ONLY a JSON array of AgentAction objects. Example:

```json
[
  {"action": "write_file", "path": "src/foo.rs", "content": "..."},
  {"action": "run_tool", "tool_name": "test", "args": []},
  {"action": "commit", "message": "feat(foo): add initial implementation", "paths": ["src/foo.rs"]},
  {"action": "done", "summary": "Implemented foo module with tests passing"}
]
```"#;

/// Build the user message from loaded context.
pub fn build_user_message(ctx: &ImplementerContext, iteration: u32) -> String {
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Hierarchy\n\n");
    msg.push_str(&format!("**Plan:** {} — {}\n", ctx.plan_title, ctx.plan_description));
    msg.push_str(&format!("**Spec:** {} — {}\n", ctx.spec_title, ctx.spec_description));
    msg.push_str(&format!("**Phase:** {} — {}\n", ctx.phase_title, ctx.phase_description));
    msg.push_str(&format!(
        "**WorkItem:** {} — {}\n\n",
        ctx.work_item_title, ctx.work_item_description
    ));

    if !ctx.learnings.is_empty() {
        msg.push_str("## Learnings\n\n");
        for learning in &ctx.learnings {
            msg.push_str(&format!("- {}\n", learning));
        }
        msg.push('\n');
    }

    if !ctx.available_tools.is_empty() {
        msg.push_str("## Available Tools\n\n");
        for tool in &ctx.available_tools {
            msg.push_str(&format!("- `{}`\n", tool));
        }
        msg.push('\n');
    }

    if let Some(ref note) = ctx.staleness_note {
        msg.push_str("## Staleness Warning\n\n");
        msg.push_str(note);
        msg.push_str("\n\n");
    }

    if let Some(ref summary) = ctx.previous_summary {
        msg.push_str("## Previous Iteration Summary\n\n");
        msg.push_str(summary);
        msg.push_str("\n\n");
    }

    msg.push_str(&format!("## Current Iteration: {}\n\n", iteration));
    msg.push_str("Implement the WorkItem described above. Respond with a JSON array of actions.\n");

    msg
}

/// Parse the LLM response into a list of agent actions.
/// Extracts the first JSON array found in the response text.
pub fn parse_actions(response: &str) -> Result<Vec<AgentAction>> {
    // Try direct parse first
    if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(response) {
        return Ok(actions);
    }

    // Try to find a JSON array in the response (may be wrapped in markdown code blocks)
    let trimmed = response.trim();
    if let Some(start) = trimmed.find('[')
        && let Some(end) = trimmed.rfind(']')
    {
        let json_slice = &trimmed[start..=end];
        if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(json_slice) {
            return Ok(actions);
        }
    }

    Err(eyre!("failed to parse agent actions from LLM response"))
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
    let mut ctx = load_context(params.stores, params.work_item_id, params.tool_runner, previous_summary)?;
    ctx.staleness_note = staleness_note;
    let user_message = build_user_message(&ctx, iteration);

    let response = params.llm.call(SYSTEM_PROMPT, &user_message).await?;
    let actions = parse_actions(&response)?;

    if actions.is_empty() {
        return Err(eyre!("LLM returned empty action list"));
    }

    let mut last_summary = String::new();
    for action in &actions {
        // Broadcast tool_started event for RunTool actions
        if let AgentAction::RunTool { tool_name, .. } = action {
            let _ = params
                .event_tx
                .send(DaemonEvent::agent_tool_started(params.session_id, tool_name));
        }

        let result = execute_action(
            action,
            params.tool_runner,
            params.bridge,
            params.worktree_path,
            Some(params.work_item_id),
        )
        .await?;

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
        last_summary = summary;
    }

    Ok(IterationOutcome::Continue(last_summary))
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

fn format_action_summary(_action: &AgentAction, result: &ActionResult) -> String {
    match result {
        ActionResult::ToolRun(tr) => format!("ran {} (exit {})", tr.tool_name, tr.exit_code),
        ActionResult::FileWritten(p) => format!("wrote {}", p),
        ActionResult::FileRead(content) => format!("read file ({} bytes)", content.len()),
        ActionResult::Committed(m) => format!("committed: {}", m),
        ActionResult::BundleProposed(d) => format!("proposed bundle: {}", d),
        ActionResult::Transitioned(d) => format!("transitioned: {}", d),
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::Done(s) => format!("done: {}", s),
        ActionResult::NeedHelp(r) => format!("need help: {}", r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentType};
    use crate::config::{AgentRoleConfig, Config, ProjectConfig, ToolEntry};
    use crate::domain::learning::Learning;
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
        let bridge = AgentIpcBridge::new(stores, event_tx.clone(), worktree_mgr, Config::default());
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

    // --- build_user_message tests ---

    #[test]
    fn test_build_user_message_basic() {
        let ctx = ImplementerContext {
            plan_title: "My Plan".into(),
            plan_description: "Plan desc".into(),
            spec_title: "My Spec".into(),
            spec_description: "Spec desc".into(),
            phase_title: "Phase 1".into(),
            phase_description: "Phase desc".into(),
            work_item_title: "WI-1".into(),
            work_item_description: "Do the thing".into(),
            learnings: vec![],
            available_tools: vec!["test".into(), "clippy".into()],
            previous_summary: None,
            staleness_note: None,
        };
        let msg = build_user_message(&ctx, 1);
        assert!(msg.contains("My Plan"));
        assert!(msg.contains("My Spec"));
        assert!(msg.contains("Phase 1"));
        assert!(msg.contains("WI-1"));
        assert!(msg.contains("`test`"));
        assert!(msg.contains("`clippy`"));
        assert!(msg.contains("Current Iteration: 1"));
        assert!(!msg.contains("Learnings"));
        assert!(!msg.contains("Previous Iteration"));
        assert!(!msg.contains("Staleness"));
    }

    #[test]
    fn test_build_user_message_with_learnings() {
        let ctx = ImplementerContext {
            plan_title: "P".into(),
            plan_description: "p".into(),
            spec_title: "S".into(),
            spec_description: "s".into(),
            phase_title: "Ph".into(),
            phase_description: "ph".into(),
            work_item_title: "W".into(),
            work_item_description: "w".into(),
            learnings: vec!["Bug found in parser".into()],
            available_tools: vec![],
            previous_summary: Some("Last iteration added error types".into()),
            staleness_note: None,
        };
        let msg = build_user_message(&ctx, 3);
        assert!(msg.contains("Learnings"));
        assert!(msg.contains("Bug found in parser"));
        assert!(msg.contains("Previous Iteration Summary"));
        assert!(msg.contains("Last iteration added error types"));
        assert!(msg.contains("Current Iteration: 3"));
    }

    // --- load_context tests ---

    #[test]
    fn test_load_context_success() {
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

        let ctx = load_context(&stores, &wi_id, &tool_runner, None).unwrap();
        assert_eq!(ctx.plan_title, "Test Plan");
        assert_eq!(ctx.spec_title, "Test Spec");
        assert_eq!(ctx.phase_title, "Test Phase");
        assert_eq!(ctx.work_item_title, "Test WorkItem");
        assert_eq!(ctx.available_tools, vec!["test"]);
        assert_eq!(ctx.learnings.len(), 1);
        assert!(ctx.previous_summary.is_none());
    }

    #[test]
    fn test_load_context_missing_work_item() {
        let dir = std::env::temp_dir().join(format!("loopr-impl-miss-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = setup_stores(&dir);
        let tool_runner = ToolRunner::new(&[]);

        let result = load_context(&stores, "nonexistent", &tool_runner, None);
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
        assert!(SYSTEM_PROMPT.contains("Implementer agent"));
        assert!(SYSTEM_PROMPT.contains("write_file"));
        assert!(SYSTEM_PROMPT.contains("run_tool"));
        assert!(SYSTEM_PROMPT.contains("done"));
        assert!(SYSTEM_PROMPT.contains("need_help"));
        assert!(SYSTEM_PROMPT.contains("JSON array"));
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
    fn test_build_user_message_with_staleness() {
        let ctx = ImplementerContext {
            plan_title: "P".into(),
            plan_description: "p".into(),
            spec_title: "S".into(),
            spec_description: "s".into(),
            phase_title: "Ph".into(),
            phase_description: "ph".into(),
            work_item_title: "W".into(),
            work_item_description: "w".into(),
            learnings: vec![],
            available_tools: vec![],
            previous_summary: None,
            staleness_note: Some("A new Tick 'tick-99' has been published.".into()),
        };
        let msg = build_user_message(&ctx, 2);
        assert!(msg.contains("Staleness Warning"));
        assert!(msg.contains("tick-99"));
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
}
