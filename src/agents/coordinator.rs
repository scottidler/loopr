use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, eyre};
use log::{info, warn};
use tokio::sync::broadcast;

use crate::agents::AgentAction;
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
use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};
use crate::domain::lock::LockStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::tick::TickStatus;
use crate::domain::work_item::WorkItemStatus;
use crate::ipc::protocol::DaemonEvent;

/// Infer the hierarchy level of a coordinator action for one-level-per-iteration guard (Gap #28).
fn infer_action_level(action: &AgentAction) -> Option<&'static str> {
    match action {
        AgentAction::CreatePlan { .. } => Some("plan"),
        AgentAction::CreateSpec { .. } => Some("spec"),
        AgentAction::CreatePhase { .. } => Some("phase"),
        AgentAction::CreateWorkItem { .. } | AgentAction::AssignAgent { .. } => Some("work_item"),
        _ => None,
    }
}

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

// ---------------------------------------------------------------------------
// FSM state management helpers
// ---------------------------------------------------------------------------

/// Load or create the CoordinatorState for the active goal.
fn load_or_create_coordinator_state(stores: &Stores) -> Option<CoordinatorState> {
    // Find active goal
    let goal_id = {
        let goals = stores.coordinator_goals.read().unwrap();
        goals.values().find(|g| g.active).map(|g| g.id.clone())?
    };

    // Check if we already have a non-terminal state for this goal
    {
        let states = stores.coordinator_states.read().unwrap();
        if let Some(existing) = states
            .values()
            .find(|s| s.goal_id == goal_id && !s.fsm_state.is_terminal())
        {
            return Some(existing.clone());
        }
    }

    // Create new state
    let state = CoordinatorState::new(goal_id);
    let id = state.id.clone();
    stores
        .coordinator_states
        .write()
        .unwrap()
        .insert(id.clone(), state.clone());
    if let Some(store_arc) = &stores.store {
        let _ = store_arc.lock().unwrap().create(state.clone());
    }
    Some(state)
}

/// Persist the CoordinatorState to both in-memory and TaskStore.
fn persist_coordinator_state(stores: &Stores, state: &CoordinatorState) {
    stores
        .coordinator_states
        .write()
        .unwrap()
        .insert(state.id.clone(), state.clone());
    if let Some(store_arc) = &stores.store {
        let _ = store_arc.lock().unwrap().update(state.clone());
    }
}

/// Determine the FSM state footer — state-specific instructions for the LLM.
fn build_fsm_footer(stores: &Stores, coord_state: &CoordinatorState, goal: &str, config: &CoordinatorConfig) -> String {
    match coord_state.fsm_state {
        CoordinatorFsmState::Planning => {
            // Use existing generation footer logic for Plan→Spec→Phase hierarchy
            if let Some(gen_footer) = build_generation_footer(stores, goal, config.max_validation_attempts) {
                gen_footer
            } else {
                // All hierarchy levels exist — ready to transition to ActivatePhase
                "All planning artifacts (Plan, Spec, Phases) are created and active. \
                 Respond with: [{\"action\": \"done\", \"summary\": \"Planning complete, ready to activate first phase\"}]"
                    .to_string()
            }
        }
        CoordinatorFsmState::ActivatePhase => {
            // Find the next phase to activate and generate WorkItems for it
            let phase_info = find_next_phase_to_activate(stores, coord_state);
            match phase_info {
                Some((phase_id, phase_title)) => {
                    let phase = {
                        let phases = stores.phases.read().unwrap();
                        phases.get(&phase_id).cloned()
                    };
                    if let Some(phase) = phase {
                        let existing = generation::find_work_items_for_phase(stores, &phase.id);
                        if existing.is_empty() {
                            let prompt = build_work_item_prompt(&phase, &existing, &[], &[]);
                            format!(
                                "## Activating Phase: {} (id: {})\n\n\
                                 Generate WorkItems for this phase. Each WorkItem should have clear \
                                 acceptance criteria and declare dependencies on other WorkItems in this phase \
                                 using their IDs.\n\n{}",
                                phase_title, phase_id, prompt.user_message
                            )
                        } else {
                            format!(
                                "Phase '{}' already has {} WorkItems. \
                                 Respond with: [{{\"action\": \"done\", \"summary\": \"Phase {} WorkItems ready\"}}]",
                                phase_title,
                                existing.len(),
                                phase_title
                            )
                        }
                    } else {
                        "No phase found to activate. Respond with: [{\"action\": \"done\", \"summary\": \"No phases available\"}]".to_string()
                    }
                }
                None => "All phases have been completed. \
                     Respond with: [{\"action\": \"done\", \"summary\": \"All phases complete\"}]"
                    .to_string(),
            }
        }
        CoordinatorFsmState::Executing => {
            // Build executing context — monitor work items, assign agents, triage bundles
            let phase_status = build_phase_status(stores, coord_state);
            format!(
                "## Executing Phase\n\n{}\n\n\
                 Monitor WorkItem statuses. Assign implementers to Ready WorkItems whose dependencies are all Done. \
                 Triage proposed Bundles. Accept reviewed Bundles. Transition Integrated WorkItems to Done. \
                 If a WorkItem is Blocked or has failed, consider retrying.\n\n\
                 Respond with a JSON array of actions.",
                phase_status
            )
        }
        CoordinatorFsmState::PhaseGate => {
            let phase_status = build_phase_status(stores, coord_state);
            format!(
                "## Phase Gate Check\n\n{}\n\n\
                 All WorkItems in this phase should be in a terminal state (Done, Abandoned, or NeedHelp). \
                 If all are Done, the phase is complete. \
                 Respond with: [{{\"action\": \"done\", \"summary\": \"Phase gate passed\"}}]",
                phase_status
            )
        }
        CoordinatorFsmState::GoalComplete => {
            format!(
                "Goal is complete. {} phases were completed. \
                 Respond with: [{{\"action\": \"done\", \"summary\": \"Goal complete\"}}]",
                coord_state.phases_completed.len()
            )
        }
    }
}

/// Find the next phase that hasn't been completed yet (by order).
fn find_next_phase_to_activate(stores: &Stores, coord_state: &CoordinatorState) -> Option<(String, String)> {
    let plan = generation::find_active_plan(stores)?;
    let specs = generation::find_active_specs_for_plan(stores, &plan.id);

    for spec in &specs {
        let phases = generation::find_active_phases_for_spec(stores, &spec.id);
        for phase in &phases {
            if !coord_state.phases_completed.contains(&phase.id) {
                return Some((phase.id.clone(), phase.title.clone()));
            }
        }
    }

    // Also check for phases that are still in Draft/Active status
    let phases = stores.phases.read().unwrap();
    let mut ordered: Vec<_> = phases
        .values()
        .filter(|p| !coord_state.phases_completed.contains(&p.id))
        .filter(|p| !matches!(p.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned))
        .collect();
    ordered.sort_by_key(|p| p.order);
    ordered.first().map(|p| (p.id.clone(), p.title.clone()))
}

/// Build a status summary for the current phase's work items.
fn build_phase_status(stores: &Stores, coord_state: &CoordinatorState) -> String {
    let phase_id = match &coord_state.current_phase_id {
        Some(id) => id,
        None => return "No active phase.".to_string(),
    };

    let phase_title = {
        let phases = stores.phases.read().unwrap();
        phases.get(phase_id).map(|p| p.title.clone()).unwrap_or_default()
    };

    let work_items = stores.work_items.read().unwrap();
    let phase_wis: Vec<_> = work_items.values().filter(|w| &w.phase_id == phase_id).collect();

    let mut status_counts = std::collections::HashMap::new();
    for wi in &phase_wis {
        *status_counts.entry(format!("{}", wi.status)).or_insert(0u32) += 1;
    }

    let mut summary = format!(
        "Phase: {} (id: {})\nWorkItems: {} total\n",
        phase_title,
        phase_id,
        phase_wis.len()
    );
    for (status, count) in &status_counts {
        summary.push_str(&format!("  - {}: {}\n", status, count));
    }

    // Show retry counts for items with attempts
    for wi in &phase_wis {
        let attempts = coord_state.attempts(&wi.id);
        if attempts > 0 {
            summary.push_str(&format!("  [{}] {} — {} attempts\n", wi.id, wi.title, attempts));
        }
    }

    summary
}

/// Advance the FSM state based on the current iteration outcome.
/// Returns the new FSM state if a transition should occur.
fn check_fsm_transition(
    stores: &Stores,
    coord_state: &CoordinatorState,
    config: &CoordinatorConfig,
) -> Option<CoordinatorFsmState> {
    match coord_state.fsm_state {
        CoordinatorFsmState::Planning => {
            // Transition when: Active Plan AND Active Spec AND all Phases Active
            let plan = generation::find_active_plan(stores)?;
            let specs = generation::find_active_specs_for_plan(stores, &plan.id);
            if specs.is_empty() {
                return None;
            }
            let has_phases = specs
                .iter()
                .any(|s| !generation::find_active_phases_for_spec(stores, &s.id).is_empty());
            if has_phases { Some(CoordinatorFsmState::ActivatePhase) } else { None }
        }
        CoordinatorFsmState::ActivatePhase => {
            // Transition when: WorkItems for current phase exist and are Ready
            if let Some(ref phase_id) = coord_state.current_phase_id {
                let wis = generation::find_work_items_for_phase(stores, phase_id);
                if !wis.is_empty() {
                    return Some(CoordinatorFsmState::Executing);
                }
            }
            // If we just set current_phase_id, wait for WorkItems to be created
            None
        }
        CoordinatorFsmState::Executing => {
            // Check phase timeout
            if let Some(activated_at) = coord_state.phase_activated_at {
                let elapsed_ms = crate::id::now_millis() - activated_at;
                let timeout_ms = config.phase_timeout_secs as i64 * 1000;
                if elapsed_ms > timeout_ms {
                    return Some(CoordinatorFsmState::PhaseGate);
                }
            }

            // Check goal timeout
            let goal_elapsed_ms = crate::id::now_millis() - coord_state.goal_started_at;
            let goal_timeout_ms = config.goal_timeout_secs as i64 * 1000;
            if goal_elapsed_ms > goal_timeout_ms {
                return Some(CoordinatorFsmState::GoalComplete);
            }

            // Transition when: all WIs in current phase are terminal
            if let Some(ref phase_id) = coord_state.current_phase_id {
                let wis = generation::find_work_items_for_phase(stores, phase_id);
                if !wis.is_empty()
                    && wis
                        .iter()
                        .all(|w| matches!(w.status, WorkItemStatus::Done | WorkItemStatus::Abandoned))
                {
                    return Some(CoordinatorFsmState::PhaseGate);
                }
            }
            None
        }
        CoordinatorFsmState::PhaseGate => {
            // Check if there are more phases
            let next = find_next_phase_to_activate(stores, coord_state);
            if next.is_some() {
                Some(CoordinatorFsmState::ActivatePhase)
            } else {
                Some(CoordinatorFsmState::GoalComplete)
            }
        }
        CoordinatorFsmState::GoalComplete => None,
    }
}

/// Context for a single coordinator iteration, bundled to avoid too-many-arguments.
struct IterationContext<'a> {
    llm: &'a dyn LlmClient,
    session: &'a AgentSession,
    stores: &'a Arc<Stores>,
    bridge: &'a AgentIpcBridge,
    config: &'a CoordinatorConfig,
    iteration: u32,
    previous_summary: Option<String>,
}

/// Run a single coordinator iteration: load context → call LLM → parse → execute actions.
/// Now dispatches based on FSM state.
async fn run_coordinator_iteration(
    ctx: &IterationContext<'_>,
    coord_state: &mut CoordinatorState,
) -> Result<IterationOutcome> {
    let session = ctx.session;
    let stores = ctx.stores;
    let config = ctx.config;
    let bridge = ctx.bridge;
    let iteration = ctx.iteration;
    // Check for FSM state transitions before the iteration
    if let Some(new_state) = check_fsm_transition(stores, coord_state, config) {
        info!(
            "Coordinator {} FSM transition: {} → {}",
            session.id, coord_state.fsm_state, new_state
        );

        // Handle ActivatePhase: find and set the next phase
        if new_state == CoordinatorFsmState::ActivatePhase {
            let next_phase = find_next_phase_to_activate(stores, coord_state);
            if let Some((phase_id, phase_title)) = next_phase {
                info!(
                    "Coordinator {} activating phase: {} ({})",
                    session.id, phase_title, phase_id
                );
                coord_state.activate_phase(phase_id);
                // activate_phase sets state to Executing — but we want ActivatePhase first for WI generation
                coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
            } else {
                coord_state.transition_to(CoordinatorFsmState::GoalComplete);
            }
        } else if new_state == CoordinatorFsmState::PhaseGate {
            coord_state.transition_to(CoordinatorFsmState::PhaseGate);
            coord_state.complete_phase();
        } else if new_state == CoordinatorFsmState::GoalComplete {
            coord_state.transition_to(CoordinatorFsmState::GoalComplete);
            // Deactivate the goal
            let mut goals = stores.coordinator_goals.write().unwrap();
            if let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id) {
                goal.deactivate();
            }
            persist_coordinator_state(stores, coord_state);
            return Ok(IterationOutcome::Done(format!(
                "Goal complete: {} phases completed",
                coord_state.phases_completed.len()
            )));
        } else {
            coord_state.transition_to(new_state);
        }
        persist_coordinator_state(stores, coord_state);
    }

    // Check if any phases have completed (all WorkItems Done) — legacy helper
    let completed_phases = check_phase_completion(stores);
    for cp in &completed_phases {
        info!("Coordinator {} detected: {}", session.id, cp);
    }

    let state_summary = build_state_summary(stores);

    let goal = {
        let goals = stores.coordinator_goals.read().unwrap();
        goals
            .values()
            .find(|g| g.active)
            .map(|g| g.goal.clone())
            .unwrap_or_else(|| "No goal set.".to_string())
    };

    let event_tx = bridge.event_tx();

    // Build FSM-aware footer
    let footer = build_fsm_footer(stores, coord_state, &goal, config);

    // Add FSM state context to state summary
    let fsm_context = format!(
        "## Coordinator FSM State: {}\nCurrent Phase: {}\nPhases Completed: {}\n\n",
        coord_state.fsm_state,
        coord_state.current_phase_id.as_deref().unwrap_or("none"),
        coord_state.phases_completed.len()
    );

    let builder = ContextBuilder::new(stores, Role::Coordinator)
        .with_state_summary(format!("{}{}", fsm_context, state_summary))
        .with_previous_summary(ctx.previous_summary.clone())
        .with_iteration(iteration)
        .with_footer(footer);

    let assembled = builder.build(&crate::prompts::store().coordinator);

    info!(
        "Coordinator {} iteration {} (FSM: {}) context: ~{} tokens",
        session.id, iteration, coord_state.fsm_state, assembled.token_estimate
    );

    let _ = event_tx.send(DaemonEvent::agent_status_changed(
        &session.id,
        AgentStatus::WaitingForLlm,
    ));

    let response = ctx.llm.call(&assembled.system_prompt, &assembled.user_message).await?;
    info!(
        "Coordinator {} raw LLM response ({} chars): {}",
        session.id,
        response.len(),
        &response[..response.len().min(800)]
    );
    let mut actions = implementer::parse_actions(&response)?;

    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session.id, AgentStatus::Running));

    if actions.is_empty() {
        return Ok(IterationOutcome::Done("No actions needed".to_string()));
    }

    // Gap #28: One-level-per-iteration guard — filter mixed-level actions
    let levels: std::collections::HashSet<_> = actions.iter().filter_map(infer_action_level).collect();
    if levels.len() > 1 {
        let first_level = infer_action_level(&actions[0]);
        warn!(
            "Coordinator attempted multi-level actions: {:?}. Executing only first level.",
            levels
        );
        actions.retain(|a| infer_action_level(a) == first_level || infer_action_level(a).is_none());
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

    // Persist state after each iteration
    coord_state.updated_at = crate::id::now_millis();
    persist_coordinator_state(stores, coord_state);

    Ok(IterationOutcome::Continue(last_summary))
}

/// Run the Coordinator's long-lived loop with adaptive timer and FSM state dispatch.
///
/// Unlike Implementer (fixed max_iterations), Coordinator runs indefinitely:
/// - `Done` → check FSM state, sleep idle_interval (30s) or active_interval (5s)
/// - `Continue` → sleep active_interval (5s), then iterate
/// - `NeedHelp` or `Err` → exit the loop
///
/// Checks for cancellation before each iteration.
/// FSM state is persisted after each iteration for crash recovery.
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

    // Load or create FSM state
    let mut coord_state = match load_or_create_coordinator_state(stores) {
        Some(state) => {
            info!(
                "Coordinator {} resuming FSM state: {} (phase: {:?})",
                session.id, state.fsm_state, state.current_phase_id
            );
            state
        }
        None => {
            info!("Coordinator {} no active goal, waiting", session.id);
            // No active goal — run in legacy mode (idle until goal is set)
            // Fall through to loop, which will sleep idle_interval
            return run_coordinator_legacy(llm, session, stores, bridge, config, event_tx).await;
        }
    };

    loop {
        // Check cancellation
        if is_session_cancelled(stores, &session.id) {
            info!("Coordinator {} cancelled, exiting loop", session.id);
            return Ok(());
        }

        // Check if goal is complete
        if coord_state.fsm_state.is_terminal() {
            info!("Coordinator {} goal complete, exiting loop", session.id);
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
        info!(
            "Coordinator {} iteration {} (FSM: {})",
            session.id, iteration, coord_state.fsm_state
        );

        let ctx = IterationContext {
            llm,
            session,
            stores,
            bridge,
            config,
            iteration,
            previous_summary: previous_summary.clone(),
        };
        let outcome = run_coordinator_iteration(&ctx, &mut coord_state).await;

        let interval = match &outcome {
            Ok(IterationOutcome::Done(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
                info!(
                    "Coordinator {} idle (FSM: {}): {}",
                    session.id, coord_state.fsm_state, summary
                );
                previous_summary = Some(summary.clone());
                // Use active interval for FSM states that need quick transitions
                match coord_state.fsm_state {
                    CoordinatorFsmState::Planning
                    | CoordinatorFsmState::ActivatePhase
                    | CoordinatorFsmState::PhaseGate => config.active_interval_secs,
                    _ => config.idle_interval_secs,
                }
            }
            Ok(IterationOutcome::Continue(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
                info!(
                    "Coordinator {} continue (FSM: {}): {}",
                    session.id, coord_state.fsm_state, summary
                );
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

/// Legacy coordinator loop for when no goal is active.
/// Keeps the original behavior: call LLM, execute actions, sleep.
async fn run_coordinator_legacy(
    llm: &dyn LlmClient,
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &CoordinatorConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    let mut iteration: u32 = 0;
    let mut previous_summary: Option<String> = None;

    // Create a dummy coord_state for the legacy path
    let mut coord_state = CoordinatorState::new("legacy".to_string());

    loop {
        if is_session_cancelled(stores, &session.id) {
            info!("Coordinator {} cancelled, exiting loop", session.id);
            return Ok(());
        }

        // Check if a goal has been set since we started
        let has_goal = {
            let goals = stores.coordinator_goals.read().unwrap();
            goals.values().any(|g| g.active)
        };
        if has_goal && let Some(state) = load_or_create_coordinator_state(stores) {
            coord_state = state;
        }

        iteration = iteration.saturating_add(1);
        session.iteration = iteration;
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&session.id) {
                s.iteration = session.iteration;
            }
        }
        info!("Coordinator {} iteration {}", session.id, iteration);

        let ctx = IterationContext {
            llm,
            session,
            stores,
            bridge,
            config,
            iteration,
            previous_summary: previous_summary.clone(),
        };
        let outcome = run_coordinator_iteration(&ctx, &mut coord_state).await;

        let interval = match &outcome {
            Ok(IterationOutcome::Done(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
                previous_summary = Some(summary.clone());
                config.idle_interval_secs
            }
            Ok(IterationOutcome::Continue(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, summary));
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

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Nothing to do")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_need_help() {
        crate::prompts::init_defaults();
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

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref s) if s.contains("Unclear")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_continue_with_stub_actions() {
        crate::prompts::init_defaults();
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

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
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

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
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
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().coordinator;
        assert!(prompt.contains("Coordinator agent"));
        assert!(prompt.contains("create_plan"));
        assert!(prompt.contains("assign_agent"));
        assert!(prompt.contains("need_help"));
        assert!(prompt.contains("JSON array"));
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

    // --- infer_action_level tests ---

    #[test]
    fn test_infer_action_level_returns_none_for_done() {
        let action = AgentAction::Done {
            summary: "all done".into(),
        };
        assert!(infer_action_level(&action).is_none());
    }

    #[test]
    fn test_infer_action_level_returns_none_for_create_learning() {
        let action = AgentAction::CreateLearning {
            content: "learned something".into(),
            scope: "global".into(),
            source_id: "test".into(),
            applicable_roles: None,
            resource_tags: None,
        };
        assert!(infer_action_level(&action).is_none());
    }

    // --- build_generation_footer tests ---

    #[test]
    fn test_build_generation_footer_generation_needed() {
        // No plans exist → generation is needed at Plan level
        let dir = std::env::temp_dir().join(format!("loopr-coord-genftr-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let footer = build_generation_footer(&stores, "Build an auth system", 3);
        assert!(footer.is_some(), "should return generation footer when no plan exists");
        let text = footer.unwrap();
        // The Plan-level generation prompt should mention creating a plan
        assert!(
            text.contains("plan") || text.contains("Plan"),
            "footer should mention plan generation"
        );
    }

    #[test]
    fn test_build_generation_footer_validation_cap_reached() {
        use crate::domain::validation::{ValidationReport, ValidationVerdict};

        let dir = std::env::temp_dir().join(format!("loopr-coord-valcap-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // Create a Draft plan so determine_generation_level returns None
        // (Draft exists, so no generation needed)
        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create 3 failed validation reports for this plan (cap = 3)
        let store_lock = stores.store.as_ref().unwrap();
        {
            let mut store = store_lock.lock().unwrap();
            for _ in 0..3 {
                let report = ValidationReport::new(
                    "plans".into(),
                    plan_id.clone(),
                    ValidationVerdict::Fail,
                    vec![],
                    "Failed criteria".into(),
                    "test-model".into(),
                );
                store.create(report).unwrap();
            }
        }

        let footer = build_generation_footer(&stores, "Build auth", 3);
        assert!(footer.is_some(), "should return footer when validation cap is reached");
        let text = footer.unwrap();
        assert!(text.contains("need_help"), "should signal need_help when cap reached");
    }

    #[test]
    fn test_build_generation_footer_draft_needs_regen() {
        use crate::domain::validation::{ValidationReport, ValidationVerdict};

        let dir = std::env::temp_dir().join(format!("loopr-coord-regen-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // Create a Draft plan
        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create 1 failed validation report (below cap of 3)
        let store_lock = stores.store.as_ref().unwrap();
        {
            let mut store = store_lock.lock().unwrap();
            let report = ValidationReport::new(
                "plans".into(),
                plan_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                "Missing acceptance criteria".into(),
                "test-model".into(),
            );
            store.create(report).unwrap();
        }

        let footer = build_generation_footer(&stores, "Build auth", 3);
        assert!(
            footer.is_some(),
            "should return regen footer when draft has failures below cap"
        );
        let text = footer.unwrap();
        assert!(
            text.contains("Re-generation Required"),
            "should contain re-generation header"
        );
    }

    // --- check_phase_completion tests ---

    #[test]
    fn test_check_phase_completion_no_active_plan() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-noplan-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // No plans at all → should return empty
        let completed = check_phase_completion(&stores);
        assert!(completed.is_empty());
    }

    // --- multi-level action filter tests ---

    #[tokio::test]
    async fn test_coordinator_iteration_filters_multi_level_actions() {
        crate::prompts::init_defaults();
        let dir = std::env::temp_dir().join(format!("loopr-coord-multilevel-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // LLM returns mixed-level actions: a plan + a spec (different levels)
        let llm = MockLlm::new(vec![
            r#"[
                {"action": "create_plan", "title": "Auth", "description": "Add auth", "acceptance_criteria": "Tests pass"},
                {"action": "create_spec", "plan_id": "plan-1", "title": "Spec1", "description": "desc"}
            ]"#
            .to_string(),
        ]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
        .await
        .unwrap();

        // Should still produce a Continue (plan created), but spec should have been filtered
        assert!(matches!(outcome, IterationOutcome::Continue(_)));

        // Only one plan should exist (the spec with nonexistent plan_id would have been filtered)
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans.len(), 1, "only the plan-level action should have executed");
    }

    #[tokio::test]
    async fn test_coordinator_iteration_empty_after_filter() {
        crate::prompts::init_defaults();
        let dir = std::env::temp_dir().join(format!("loopr-coord-emptyfilter-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // LLM returns only a "done" action with no hierarchy level
        let llm = MockLlm::new(vec![
            r#"[{"action": "done", "summary": "Finished planning"}]"#.to_string(),
        ]);

        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let outcome = run_coordinator_iteration(
            &IterationContext {
                llm: &llm,
                session: &session,
                stores: &stores,
                bridge: &bridge,
                config: &CoordinatorConfig::default(),
                iteration: 1,
                previous_summary: None,
            },
            &mut CoordinatorState::new("test-goal".to_string()),
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Finished planning")),
            "done action should yield Done outcome"
        );
    }

    // --- format_action_summary additional coverage ---

    #[test]
    fn test_format_action_summary_document_validated_empty_issues() {
        // Exercises the empty-issues branch with a different verdict
        let result = ActionResult::DocumentValidated {
            verdict: "fail".into(),
            summary: "Criteria not met".into(),
            issues: vec![],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: fail"), "should contain the fail verdict");
        assert!(summary.contains("Criteria not met"), "should contain the summary");
        // Empty issues → no "(N issues)" suffix
        assert!(
            !summary.contains("issues"),
            "should not mention issues count when issues is empty"
        );
    }

    // --- FSM state management tests ---

    #[test]
    fn test_load_or_create_coordinator_state_no_goal() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-nogoal-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let result = load_or_create_coordinator_state(&stores);
        assert!(result.is_none());
    }

    #[test]
    fn test_load_or_create_coordinator_state_with_goal() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-goal-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Build app".to_string());
        let goal_id = goal.id.clone();
        stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

        let state = load_or_create_coordinator_state(&stores).unwrap();
        assert_eq!(state.goal_id, goal_id);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);
    }

    #[test]
    fn test_load_or_create_coordinator_state_resumes_existing() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-resume-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Build app".to_string());
        let goal_id = goal.id.clone();
        stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

        // Create an existing state in Executing
        let mut existing = CoordinatorState::new(goal_id.clone());
        existing.transition_to(CoordinatorFsmState::Executing);
        existing.current_phase_id = Some("phase-1".to_string());
        let existing_id = existing.id.clone();
        stores
            .coordinator_states
            .write()
            .unwrap()
            .insert(existing_id.clone(), existing);

        let state = load_or_create_coordinator_state(&stores).unwrap();
        assert_eq!(state.id, existing_id);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(state.current_phase_id.as_deref(), Some("phase-1"));
    }

    #[test]
    fn test_check_fsm_transition_planning_to_activate() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-plan2act-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // Create Plan → Spec → Phase hierarchy (all Active)
        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id.clone(), "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let coord_state = CoordinatorState::new("goal-1".to_string());
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::ActivatePhase));
    }

    #[test]
    fn test_check_fsm_transition_executing_to_phase_gate() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-exec2gate-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Create a Done work item in the phase
        let mut wi = WorkItem::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi.status = WorkItemStatus::Done;
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string());
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::PhaseGate));
    }

    #[test]
    fn test_check_fsm_transition_phase_gate_to_goal_complete() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-gate2done-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        // No more phases to activate
        let mut coord_state = CoordinatorState::new("goal-1".to_string());
        coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::GoalComplete));
    }

    #[test]
    fn test_persist_coordinator_state() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-persist-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut state = CoordinatorState::new("goal-1".to_string());
        state.transition_to(CoordinatorFsmState::Executing);
        let state_id = state.id.clone();

        persist_coordinator_state(&stores, &state);

        let stored = stores.coordinator_states.read().unwrap();
        let retrieved = stored.get(&state_id).unwrap();
        assert_eq!(retrieved.fsm_state, CoordinatorFsmState::Executing);
    }

    #[test]
    fn test_build_phase_status_no_phase() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-nophase-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let coord_state = CoordinatorState::new("goal-1".to_string());
        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("No active phase"));
    }

    #[test]
    fn test_build_phase_status_with_work_items() {
        let dir = std::env::temp_dir().join(format!("loopr-coord-fsm-phstatus-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Build Phase".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi1 = WorkItem::new(phase_id.clone(), "WI 1".into(), "desc".into());
        let mut wi2 = WorkItem::new(phase_id.clone(), "WI 2".into(), "desc".into());
        wi2.status = WorkItemStatus::Done;
        stores.work_items.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.work_items.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string());
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("Build Phase"));
        assert!(status.contains("2 total"));
    }
}
