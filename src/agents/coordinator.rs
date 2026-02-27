use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use log::{info, warn};
use tokio::sync::broadcast;

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::generation::{
    self, GenerationLevel, build_phase_prompt, build_plan_prompt, build_spec_prompt, build_work_item_prompt,
};
use crate::agents::implementer::{self, IterationOutcome, LlmClient};
use crate::agents::{AgentSession, AgentStatus, AgentType};
use crate::config::CoordinatorConfig;
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::lock::LockStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::tick::TickStatus;
use crate::domain::work_item::WorkItemStatus;
use crate::ipc::protocol::DaemonEvent;

const SYSTEM_PROMPT: &str = r#"You are the Coordinator agent in the Loopr development orchestrator. You are the project manager and engineering manager. You own the full pipeline: Plan → Spec → Phase → WorkItem → Bundle → Tick.

## Your Responsibilities

1. Assess the current state of the project
2. Decide what level needs attention (Plan, Spec, Phase, or Code)
3. Create hierarchy records (Plans, Specs, Phases, WorkItems)
4. Triage and accept Bundles (Proposed→Triaged, Reviewed→Accepted)
5. Assign work to Implementer and Reviewer agents
6. Manage resource locks (acquire before assignment, release on completion)
7. Spawn Researchers when you need codebase information
8. Track progress and mark completed items
9. Create process-level Learnings

## Your Capabilities

Respond with a JSON array of actions:

1. `create_plan`      {"action": "create_plan", "title": "...", "description": "...", "acceptance_criteria": "..."}
2. `create_spec`      {"action": "create_spec", "plan_id": "...", "title": "...", "description": "..."}
3. `create_phase`     {"action": "create_phase", "spec_id": "...", "title": "...", "description": "...", "order": 1}
4. `create_work_item` {"action": "create_work_item", "phase_id": "...", "title": "...", "description": "..."}
5. `assign_agent`     {"action": "assign_agent", "agent_type": "implementer", "target_id": "work-item-id"}
6. `spawn_researcher` {"action": "spawn_researcher", "query": "...", "scope_id": "spec-id"}
7. `acquire_lock`     {"action": "acquire_lock", "resource": "src/agents/mod.rs", "holder_id": "work-item-id"}
8. `release_lock`     {"action": "release_lock", "lock_id": "lock-id"}
9. `validate_document` {"action": "validate_document", "collection": "plans", "id": "plan-id"}
10. `triage_bundle`   {"action": "triage_bundle", "bundle_id": "..."}
11. `accept_bundle`   {"action": "accept_bundle", "bundle_id": "..."}
12. `transition`      {"action": "transition", "collection": "plans", "id": "...", "target_status": "active"}
13. `create_learning` {"action": "create_learning", "content": "...", "scope": "plan", "source_id": "..."}
14. `need_help`       {"action": "need_help", "reason": "..."}
15. `done`            {"action": "done", "summary": "..."}

## Rules

- Operate at ONE level per iteration. Don't advance all levels at once.
- Check for existing Drafts before generating new documents.
- Always validate documents before transitioning Draft → Active.
- Create WorkItems small enough to fit in half a context window.
- Don't assign more agents than pool_size allows (check active sessions).
- Acquire locks on resources BEFORE assigning Implementers.
- When acceptance criteria are met, mark the Plan Complete.

## Output Format

Respond with ONLY a JSON array of actions."#;

/// Build a state summary string from stores for the Coordinator's context.
///
/// Uses lock-snapshot pattern: acquires each lock briefly, clones/summarizes, releases.
/// The summary is designed to fit within the Coordinator's state_summary token budget (3000 tokens).
pub fn build_state_summary(stores: &Stores) -> String {
    let mut summary = String::with_capacity(4096);

    // --- Plans ---
    {
        let plans = stores.plans.read().unwrap();
        let mut non_terminal: Vec<_> = plans
            .values()
            .filter(|p| !matches!(p.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|p| p.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### Plans\n");
            for p in &non_terminal {
                summary.push_str(&format!("- [{}] {} ({})\n", p.id, p.title, p.status));
            }
            summary.push('\n');
        }
    }

    // --- Specs ---
    {
        let specs = stores.specs.read().unwrap();
        let mut non_terminal: Vec<_> = specs
            .values()
            .filter(|s| !matches!(s.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|s| s.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### Specs\n");
            for s in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, plan: {})\n",
                    s.id, s.title, s.status, s.plan_id
                ));
            }
            summary.push('\n');
        }
    }

    // --- Phases ---
    {
        let phases = stores.phases.read().unwrap();
        let mut non_terminal: Vec<_> = phases
            .values()
            .filter(|p| !matches!(p.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by(|a, b| a.order.cmp(&b.order).then(a.created_at.cmp(&b.created_at)));
        if !non_terminal.is_empty() {
            summary.push_str("### Phases\n");
            for p in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, spec: {}, order: {})\n",
                    p.id, p.title, p.status, p.spec_id, p.order
                ));
            }
            summary.push('\n');
        }
    }

    // --- WorkItems ---
    {
        let work_items = stores.work_items.read().unwrap();
        let mut non_terminal: Vec<_> = work_items
            .values()
            .filter(|w| !matches!(w.status, WorkItemStatus::Done | WorkItemStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|w| w.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### WorkItems\n");
            for w in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, phase: {})\n",
                    w.id, w.title, w.status, w.phase_id
                ));
            }
            summary.push('\n');
        }
    }

    // --- Bundles (non-terminal) ---
    {
        let bundles = stores.bundles.read().unwrap();
        let mut pending: Vec<_> = bundles
            .values()
            .filter(|b| {
                !matches!(
                    b.status,
                    BundleStatus::Merged | BundleStatus::Rejected | BundleStatus::Superseded
                )
            })
            .collect();
        pending.sort_by_key(|b| b.created_at);
        if !pending.is_empty() {
            summary.push_str("### Bundles\n");
            for b in &pending {
                summary.push_str(&format!("- [{}] {} (wi: {})\n", b.id, b.status, b.work_item_id));
            }
            summary.push('\n');
        }
    }

    // --- Ticks (non-terminal) ---
    {
        let ticks = stores.ticks.read().unwrap();
        let mut active: Vec<_> = ticks
            .values()
            .filter(|t| !matches!(t.status, TickStatus::Published | TickStatus::Failed))
            .collect();
        active.sort_by_key(|t| t.created_at);
        if !active.is_empty() {
            summary.push_str("### Ticks\n");
            for t in &active {
                summary.push_str(&format!("- [{}] {}\n", t.id, t.status));
            }
            summary.push('\n');
        }
    }

    // --- Active Agent Sessions ---
    {
        let sessions = stores.agent_sessions.read().unwrap();
        let mut active: Vec<_> = sessions.values().filter(|s| !s.status.is_terminal()).collect();
        active.sort_by_key(|s| s.created_at);
        if !active.is_empty() {
            summary.push_str("### Active Agents\n");
            for s in &active {
                let target = s
                    .work_item_id
                    .as_deref()
                    .or(s.bundle_id.as_deref())
                    .or(s.target_id.as_deref())
                    .unwrap_or("global");
                summary.push_str(&format!(
                    "- [{}] {} {} (target: {})\n",
                    s.id, s.agent_type, s.status, target
                ));
            }
            summary.push('\n');
        }
    }

    // --- Active Locks ---
    {
        let locks = stores.locks.read().unwrap();
        let active: Vec<_> = locks.values().filter(|l| l.status == LockStatus::Active).collect();
        if !active.is_empty() {
            summary.push_str("### Active Locks\n");
            for l in &active {
                summary.push_str(&format!("- [{}] {} (holder: {})\n", l.id, l.resource, l.holder_id));
            }
            summary.push('\n');
        }
    }

    if summary.is_empty() {
        summary.push_str("No active records. The project is starting from scratch.\n");
    }

    summary
}

/// Check if the session has been cancelled (re-read from stores).
fn is_session_cancelled(stores: &Stores, session_id: &str) -> bool {
    let sessions = stores.agent_sessions.read().unwrap();
    sessions
        .get(session_id)
        .map(|s| s.status == AgentStatus::Cancelled)
        .unwrap_or(true) // missing session = treat as cancelled
}

/// Build a generation-specific footer for the Coordinator's context message.
///
/// Handles three cases:
/// 1. **New generation** — no document at this level → generate from scratch.
/// 2. **Re-generation** — Draft exists with failed validation → regenerate with accumulated failures.
/// 3. **Validation cap reached** — Draft has exceeded max_validation_attempts → NeedHelp signal.
///
/// Returns None when no generation/re-generation is needed (normal iteration).
fn build_generation_footer(stores: &Stores, goal: &str, max_validation_attempts: u32) -> Option<String> {
    // Case 1: Check if generation is needed at any level (no document exists)
    if let Some(level) = generation::determine_generation_level(stores) {
        let prompt = match level {
            GenerationLevel::Plan => build_plan_prompt(goal, &[], &[]),
            GenerationLevel::Spec => {
                let plan = generation::find_active_plan(stores)?;
                build_spec_prompt(&plan, &[], &[], &[])
            }
            GenerationLevel::Phase => {
                let plan = generation::find_active_plan(stores)?;
                let specs = generation::find_active_specs_for_plan(stores, &plan.id);
                let spec = specs.into_iter().next()?;
                build_phase_prompt(&spec, &[], &[])
            }
            GenerationLevel::WorkItem => {
                let phase = generation::find_phase_needing_work_items(stores)?;
                let existing = generation::find_work_items_for_phase(stores, &phase.id);
                build_work_item_prompt(&phase, &existing, &[], &[])
            }
        };

        info!(
            "Coordinator generation needed at level: {} (prompt level: {})",
            level, prompt.level
        );
        return Some(prompt.user_message);
    }

    // Case 3: Check if validation cap is reached → signal NeedHelp
    if generation::is_validation_cap_reached(stores, max_validation_attempts) {
        info!(
            "Coordinator: validation cap reached ({} attempts), signaling need_help",
            max_validation_attempts
        );
        return Some(format!(
            "A Draft document has failed validation {} times (the maximum). \
             You cannot fix it further. Respond with:\n\
             [{{\"action\": \"need_help\", \"reason\": \"Document failed validation {} times, needs human review\"}}]",
            max_validation_attempts, max_validation_attempts
        ));
    }

    // Case 2: Check if a Draft exists with failed validation → re-generate with accumulated failures
    if let Some(regen) = generation::find_draft_needing_regeneration(stores, max_validation_attempts) {
        info!(
            "Coordinator re-generation needed: {} {} (attempt {}/{})",
            regen.collection,
            regen.target_id,
            regen.attempt_count + 1,
            max_validation_attempts
        );
        let prompt = match regen.level {
            GenerationLevel::Plan => build_plan_prompt(goal, &[], &regen.accumulated_failures),
            GenerationLevel::Spec => {
                let plan = generation::find_active_plan(stores)?;
                build_spec_prompt(&plan, &[], &[], &regen.accumulated_failures)
            }
            GenerationLevel::Phase => {
                let plan = generation::find_active_plan(stores)?;
                let specs = generation::find_active_specs_for_plan(stores, &plan.id);
                let spec = specs.into_iter().next()?;
                build_phase_prompt(&spec, &[], &regen.accumulated_failures)
            }
            // WorkItems don't go through Draft→Active validation cycle
            GenerationLevel::WorkItem => return None,
        };

        // Prepend context about the failed Draft
        let mut footer = format!(
            "## Re-generation Required (attempt {}/{})\n\n\
             The existing Draft document ({}/{}) failed validation. \
             You must create a NEW, improved version that addresses ALL the failures listed below. \
             First, transition the failed Draft to 'abandoned', then create a new document.\n\n",
            regen.attempt_count + 1,
            max_validation_attempts,
            regen.collection,
            regen.target_id,
        );
        footer.push_str(&prompt.user_message);
        return Some(footer);
    }

    None
}

/// Check if the Coordinator should mark any Phases as complete based on WorkItem status.
/// Returns summaries of any phases that were detected as complete.
pub fn check_phase_completion(stores: &Stores) -> Vec<String> {
    let plan = match generation::find_active_plan(stores) {
        Some(p) => p,
        None => return vec![],
    };
    let specs = generation::find_active_specs_for_plan(stores, &plan.id);
    let mut completed = Vec::new();
    for spec in &specs {
        let phases = generation::find_active_phases_for_spec(stores, &spec.id);
        for phase in &phases {
            if generation::is_phase_complete(stores, &phase.id) {
                completed.push(format!(
                    "Phase '{}' (id: {}) has all WorkItems Done",
                    phase.title, phase.id
                ));
            }
        }
    }
    completed
}

/// Run a single coordinator iteration: load context → call LLM → parse → execute actions.
async fn run_coordinator_iteration(
    llm: &dyn LlmClient,
    session: &AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &CoordinatorConfig,
    iteration: u32,
    previous_summary: Option<String>,
) -> Result<IterationOutcome> {
    // Check if any phases have completed (all WorkItems Done)
    let completed_phases = check_phase_completion(stores);
    for cp in &completed_phases {
        info!("Coordinator {} detected: {}", session.id, cp);
    }

    let state_summary = build_state_summary(stores);

    // Find active plan for scope_ids (if any)
    let active_plan_id = {
        let plans = stores.plans.read().unwrap();
        plans
            .values()
            .find(|p| p.status == HierarchyStatus::Active)
            .map(|p| p.id.clone())
    };

    // Check if generation is needed — if so, use a targeted generation prompt as footer.
    // Otherwise use the default "assess and act" footer.
    let goal = {
        let goals = stores.coordinator_goals.read().unwrap();
        goals
            .values()
            .find(|g| g.active)
            .map(|g| g.goal.clone())
            .unwrap_or_else(|| "No goal set.".to_string())
    };

    let event_tx = bridge.event_tx();

    let footer = match build_generation_footer(stores, &goal, config.max_validation_attempts) {
        Some(gen_footer) => gen_footer,
        None => "Assess the project state and decide what action to take next. \
                 Respond with a JSON array of actions."
            .to_string(),
    };

    let builder = ContextBuilder::new(stores, Role::Coordinator)
        .with_state_summary(state_summary)
        .with_previous_summary(previous_summary)
        .with_iteration(iteration)
        .with_footer(footer);

    let _ = active_plan_id; // used in generation footer indirectly via stores

    let assembled = builder.build(SYSTEM_PROMPT);

    info!(
        "Coordinator {} iteration {} context: ~{} tokens",
        session.id, iteration, assembled.token_estimate
    );

    let _ = event_tx.send(DaemonEvent::agent_status_changed(
        &session.id,
        AgentStatus::WaitingForLlm,
    ));

    let response = llm.call(&assembled.system_prompt, &assembled.user_message).await?;
    info!(
        "Coordinator {} raw LLM response ({} chars): {}",
        session.id,
        response.len(),
        &response[..response.len().min(800)]
    );
    let actions = implementer::parse_actions(&response)?;

    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session.id, AgentStatus::Running));

    if actions.is_empty() {
        return Ok(IterationOutcome::Done("No actions needed".to_string()));
    }

    // Use repo root as the "worktree" path for Coordinator (thinking plane — no actual worktree)
    let repo_root = &stores.config.project.repo_path;
    let tool_runner = &*stores.tool_runner;

    let mut last_summary = String::new();
    for action in &actions {
        let result = match execute_action(action, tool_runner, bridge, repo_root, None, AgentType::Coordinator).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Coordinator {} action failed (non-fatal): {e}", session.id);
                ActionResult::ActionError(e.to_string())
            }
        };

        let summary = format_action_summary(&result);
        let _ = event_tx.send(DaemonEvent::agent_action_completed(&session.id, &summary));

        match &result {
            ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
            ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
            _ => {}
        }
        last_summary = summary;
    }

    Ok(IterationOutcome::Continue(last_summary))
}

/// Run the Coordinator's long-lived loop with adaptive timer.
///
/// Unlike Implementer (fixed max_iterations), Coordinator runs indefinitely:
/// - `Done` → sleep idle_interval (30s), then check again
/// - `Continue` → sleep active_interval (5s), then iterate
/// - `NeedHelp` or `Err` → exit the loop
///
/// Checks for cancellation before each iteration.
pub async fn run_coordinator(
    llm: &dyn LlmClient,
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &CoordinatorConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    let mut iteration: u32 = 0;
    let mut previous_summary: Option<String> = None;

    loop {
        // Check cancellation
        if is_session_cancelled(stores, &session.id) {
            info!("Coordinator {} cancelled, exiting loop", session.id);
            return Ok(());
        }

        iteration = iteration.saturating_add(1);
        session.iteration = iteration;
        // Persist iteration to stores so agent list/status reflect progress
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&session.id) {
                s.iteration = session.iteration;
            }
        }
        info!("Coordinator {} iteration {}", session.id, iteration);

        let outcome = run_coordinator_iteration(
            llm,
            session,
            stores,
            bridge,
            config,
            iteration,
            previous_summary.clone(),
        )
        .await;

        let interval = match &outcome {
            Ok(IterationOutcome::Done(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
                info!("Coordinator {} idle: {}", session.id, summary);
                previous_summary = Some(summary.clone());
                config.idle_interval_secs
            }
            Ok(IterationOutcome::Continue(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
                info!("Coordinator {} continue: {}", session.id, summary);
                previous_summary = Some(summary.clone());
                config.active_interval_secs
            }
            Ok(IterationOutcome::NeedHelp(reason)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, reason));
                warn!("Coordinator {} needs help: {}", session.id, reason);
                return Err(eyre!("coordinator needs help: {}", reason));
            }
            Err(e) => {
                warn!("Coordinator {} iteration {} failed: {}", session.id, iteration, e);
                return Err(eyre!("coordinator iteration failed: {}", e));
            }
        };

        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

fn format_action_summary(result: &ActionResult) -> String {
    match result {
        ActionResult::ToolRun(tr) => format!("ran {} (exit {})", tr.tool_name, tr.exit_code),
        ActionResult::FileWritten(p) => format!("wrote {}", p),
        ActionResult::FileRead(content) => format!("read file ({} bytes)", content.len()),
        ActionResult::Committed(m) => format!("committed: {}", m),
        ActionResult::BundleProposed(d) => format!("proposed bundle: {}", d),
        ActionResult::Transitioned(d) => format!("transitioned: {}", d),
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::LockAcquired(id) => format!("lock acquired: {}", id),
        ActionResult::LockReleased(id) => format!("lock released: {}", id),
        ActionResult::DocumentValidated {
            verdict,
            summary,
            issues,
        } => {
            if issues.is_empty() {
                format!("validated: {} — {}", verdict, summary)
            } else {
                format!("validated: {} — {} ({} issues)", verdict, summary, issues.len())
            }
        }
        ActionResult::Done(s) => format!("done: {}", s),
        ActionResult::NeedHelp(r) => format!("need help: {}", r),
        ActionResult::ActionError(e) => format!("ERROR: {}", e),
        ActionResult::RecordCreated { collection, id } => format!("created {}: {}", collection, id),
        ActionResult::AgentSpawned { session_id, agent_type } => format!("spawned {} ({})", agent_type, session_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentSession;
    use crate::config::{Config, ProjectConfig};
    use crate::domain::bundle::Bundle;
    use crate::domain::lock::Lock;
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::tick::Tick;
    use crate::domain::work_item::WorkItem;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    /// Mock LLM client for testing.
    struct MockLlm {
        responses: StdMutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: StdMutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(r#"[{"action": "done", "summary": "No more responses"}]"#.to_string())
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
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

    // --- build_state_summary tests ---

    #[test]
    fn test_build_state_summary_empty() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-empty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("No active records"));
    }

    #[test]
    fn test_build_state_summary_with_plan() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-plan-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "Tests pass".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### Plans"));
        assert!(summary.contains("Test Plan"));
        assert!(summary.contains("draft"));
    }

    #[test]
    fn test_build_state_summary_excludes_completed() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-excl-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Done Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Complete;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let summary = build_state_summary(&stores);
        assert!(!summary.contains("Done Plan"));
        assert!(summary.contains("No active records"));
    }

    #[test]
    fn test_build_state_summary_with_work_items() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-wi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let wi = WorkItem::new("ph-1".into(), "Add auth".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### WorkItems"));
        assert!(summary.contains("Add auth"));
    }

    #[test]
    fn test_build_state_summary_with_bundles() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-bun-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let bundle = Bundle::new("wi-1".into(), None, "branch-1".into(), "claims".into());
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### Bundles"));
        assert!(summary.contains("Proposed"));
    }

    #[test]
    fn test_build_state_summary_with_active_sessions() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-sess-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let session = AgentSession::new(AgentType::Implementer, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### Active Agents"));
        assert!(summary.contains("implementer"));
    }

    #[test]
    fn test_build_state_summary_with_locks() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-lock-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let lock = Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into());
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### Active Locks"));
        assert!(summary.contains("src/main.rs"));
    }

    #[test]
    fn test_build_state_summary_excludes_terminal_sessions() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-termsess-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Implementer, "model".into());
        session.status = AgentStatus::Completed;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let summary = build_state_summary(&stores);
        assert!(!summary.contains("### Active Agents"));
    }

    // --- is_session_cancelled tests ---

    #[test]
    fn test_is_session_cancelled_false() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-canc1-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Coordinator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        assert!(!is_session_cancelled(&stores, &sid));
    }

    #[test]
    fn test_is_session_cancelled_true() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-canc2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Coordinator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        assert!(is_session_cancelled(&stores, &sid));
    }

    #[test]
    fn test_is_session_cancelled_missing() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-canc3-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert!(is_session_cancelled(&stores, "nonexistent-id"));
    }

    // --- run_coordinator_iteration tests ---

    #[tokio::test]
    async fn test_coordinator_iteration_done() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-itdone-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![r#"[{"action": "done", "summary": "Nothing to do"}]"#.to_string()]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome =
            run_coordinator_iteration(&llm, &session, &stores, &bridge, &CoordinatorConfig::default(), 1, None)
                .await
                .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Nothing to do")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_need_help() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-ithelp-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![
            r#"[{"action": "need_help", "reason": "Unclear requirements"}]"#.to_string(),
        ]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome =
            run_coordinator_iteration(&llm, &session, &stores, &bridge, &CoordinatorConfig::default(), 1, None)
                .await
                .unwrap();

        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref s) if s.contains("Unclear")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_continue_with_stub_actions() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-itstub-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![
            r#"[{"action": "create_plan", "title": "Auth", "description": "Add auth", "acceptance_criteria": "Tests pass"}]"#.to_string(),
        ]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome =
            run_coordinator_iteration(&llm, &session, &stores, &bridge, &CoordinatorConfig::default(), 1, None)
                .await
                .unwrap();

        // CreatePlan is now wired — creates a real plan via bridge, returns Continue
        assert!(matches!(outcome, IterationOutcome::Continue(_)));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_empty_actions_is_done() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-itempty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec!["[]".to_string()]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome =
            run_coordinator_iteration(&llm, &session, &stores, &bridge, &CoordinatorConfig::default(), 1, None)
                .await
                .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(_)));
    }

    // --- run_coordinator tests ---

    #[tokio::test]
    async fn test_coordinator_exits_on_need_help() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-runhelp-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![r#"[{"action": "need_help", "reason": "I'm stuck"}]"#.to_string()]);

        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = CoordinatorConfig::default();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let result = run_coordinator(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    #[tokio::test]
    async fn test_coordinator_exits_on_cancellation() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-runcanc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![]);

        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = CoordinatorConfig::default();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let result = run_coordinator(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_ok()); // Cancelled = graceful exit
    }

    // --- format_action_summary tests ---

    #[test]
    fn test_format_action_summary_done() {
        let result = ActionResult::Done("Complete".into());
        assert_eq!(format_action_summary(&result), "done: Complete");
    }

    #[test]
    fn test_format_action_summary_record_created() {
        let result = ActionResult::RecordCreated {
            collection: "plans".into(),
            id: "plan-123".into(),
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("created plans"));
        assert!(summary.contains("plan-123"));
    }

    #[test]
    fn test_format_action_summary_agent_spawned() {
        let result = ActionResult::AgentSpawned {
            session_id: "sess-abc".into(),
            agent_type: "implementer".into(),
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("spawned implementer"));
        assert!(summary.contains("sess-abc"));
    }

    #[test]
    fn test_format_action_summary_error() {
        let result = ActionResult::ActionError("something broke".into());
        assert_eq!(format_action_summary(&result), "ERROR: something broke");
    }

    #[test]
    fn test_format_action_summary_document_validated_pass() {
        let result = ActionResult::DocumentValidated {
            verdict: "pass".into(),
            summary: "All criteria met".into(),
            issues: vec![],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: pass"));
        assert!(summary.contains("All criteria met"));
        assert!(!summary.contains("issues"));
    }

    #[test]
    fn test_format_action_summary_document_validated_fail_with_issues() {
        let result = ActionResult::DocumentValidated {
            verdict: "fail".into(),
            summary: "Incomplete document".into(),
            issues: vec!["Missing criteria".into(), "Too vague".into()],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: fail"));
        assert!(summary.contains("2 issues"));
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_sections() {
        assert!(SYSTEM_PROMPT.contains("Coordinator agent"));
        assert!(SYSTEM_PROMPT.contains("create_plan"));
        assert!(SYSTEM_PROMPT.contains("assign_agent"));
        assert!(SYSTEM_PROMPT.contains("need_help"));
        assert!(SYSTEM_PROMPT.contains("JSON array"));
    }

    // --- comprehensive state summary ---

    #[test]
    fn test_build_state_summary_comprehensive() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-comp-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // Add plan
        let plan = Plan::new("My Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Add spec
        let spec = Spec::new(plan_id.clone(), "My Spec".into(), "spec desc".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Add phase
        let phase = Phase::new(spec_id.clone(), "Phase 1".into(), "phase desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Add work item
        let wi = WorkItem::new(phase_id.clone(), "WI 1".into(), "wi desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        // Add tick
        let tick = Tick::new(1);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let summary = build_state_summary(&stores);
        assert!(summary.contains("### Plans"));
        assert!(summary.contains("### Specs"));
        assert!(summary.contains("### Phases"));
        assert!(summary.contains("### WorkItems"));
        assert!(summary.contains("### Ticks"));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_persists() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-itpersist-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // MockLlm: iterations 1,2 return Continue, iteration 3 returns NeedHelp to exit loop
        let llm = MockLlm::new(vec![
            r#"[{"action": "create_learning", "content": "iter 1", "scope": "global", "source_id": "test"}]"#
                .to_string(),
            r#"[{"action": "create_learning", "content": "iter 2", "scope": "global", "source_id": "test"}]"#
                .to_string(),
            r#"[{"action": "need_help", "reason": "done testing"}]"#.to_string(),
        ]);

        let mut session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = CoordinatorConfig {
            active_interval_secs: 0,
            idle_interval_secs: 0,
            ..CoordinatorConfig::default()
        };

        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let _ = run_coordinator(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;

        // Session iteration should be 3 (need_help on iteration 3)
        assert_eq!(session.iteration, 3);

        // The iteration should also be persisted in stores
        let stored_iteration = stores
            .agent_sessions
            .read()
            .unwrap()
            .get(&session.id)
            .map(|s| s.iteration)
            .unwrap_or(0);
        assert_eq!(stored_iteration, 3, "iteration should be persisted in stores");
    }
}
