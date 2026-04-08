use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eyre::{Result, eyre};

use crate::agents::AgentAction;
use crate::agents::context::ContextBuilder;
use crate::agents::error::AgentErrorKind;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::generation::{self, build_work_prompt};
use crate::agents::implementer::{self, ChatMessage, IterationOutcome, LlmClient};
use crate::agents::lifeguard::{self, Lifeguard, Verdict};
use crate::agents::{Agent, AgentContext, AgentKind, AgentStatus};
use crate::config::CoordinatorConfig;
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};
use crate::domain::learning::LearningScope;
use crate::domain::lock::LockStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::role::Role;
use crate::domain::tick::TickStatus;
use crate::domain::work::WorkStatus;
use crate::ipc::protocol::DaemonEvent;

/// Infer the hierarchy level of a coordinator action for one-level-per-iteration guard (Gap #28).
fn infer_action_level(action: &AgentAction) -> Option<&'static str> {
    match action {
        AgentAction::CreateWork { .. } | AgentAction::AssignAgent { .. } => Some("work"),
        _ => None,
    }
}

/// Build a state summary string from stores for the Coordinator's context.
///
/// Uses lock-snapshot pattern: acquires each lock briefly, clones/summarizes, releases.
/// The summary is designed to fit within the Coordinator's state_summary token budget (3000 tokens).
pub fn build_state_summary(stores: &Stores, prefix: &str) -> String {
    build_state_summary_with_sla(stores, prefix, None, None)
}

pub fn build_state_summary_with_sla(
    stores: &Stores,
    prefix: &str,
    coord_state: Option<&CoordinatorState>,
    sla_config: Option<&crate::config::WorkSlaConfig>,
) -> String {
    tracing::debug!("{} build_state_summary_with_sla()", prefix);
    let mut summary = String::with_capacity(4096);

    // Sections ordered most-actionable-first so truncation drops static info (Plans/Specs)
    // rather than dynamic info (Works/Bundles/Agents) the coordinator needs most.

    // --- Works ---
    {
        let Ok(works) = stores.read_works() else {
            return summary;
        };
        let mut non_terminal: Vec<_> = works
            .values()
            .filter(|w| !matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|w| w.created_at);
        if !non_terminal.is_empty() {
            let now = crate::id::now_millis();
            summary.push_str("### Works\n");
            for w in &non_terminal {
                let sla_annotation = match (coord_state, sla_config) {
                    (Some(cs), Some(sla)) => {
                        let attempts = cs.attempts(&w.id);
                        let age_minutes = cs.work_age_minutes(&w.id, now).unwrap_or(0);
                        let attempt_breach = attempts >= sla.max_attempts;
                        let time_breach = age_minutes >= sla.max_wall_clock_minutes as i64;
                        if attempt_breach || time_breach {
                            format!(
                                " **SLA BREACHED** — attempts: {}/{}, age: {}min/{}min",
                                attempts, sla.max_attempts, age_minutes, sla.max_wall_clock_minutes
                            )
                        } else {
                            format!(
                                " attempts: {}/{}, age: {}min/{}min",
                                attempts, sla.max_attempts, age_minutes, sla.max_wall_clock_minutes
                            )
                        }
                    }
                    _ => String::new(),
                };
                summary.push_str(&format!(
                    "- [{}] {} ({}, phase: {}){}\n",
                    w.id,
                    w.title,
                    w.status(),
                    w.parent_id,
                    sla_annotation
                ));
            }
            summary.push('\n');
        }
    }

    // --- Reviewed Bundles (use accept_bundle) ---
    {
        let Ok(bundles) = stores.read_bundles() else {
            return summary;
        };
        let mut reviewed: Vec<_> = bundles
            .values()
            .filter(|b| matches!(b.status(), BundleStatus::Reviewed))
            .collect();
        reviewed.sort_by_key(|b| b.created_at);
        if !reviewed.is_empty() {
            summary.push_str("### Reviewed Bundles (use accept_bundle)\n");
            for b in &reviewed {
                summary.push_str(&format!("- [{}] {} (wi: {})\n", b.id, b.status(), b.work_id));
            }
            summary.push('\n');
        }
    }

    // C2: Recently Merged Bundles whose parent WI still needs advancing
    {
        let Ok(bundles) = stores.read_bundles() else {
            return summary;
        };
        let Ok(works) = stores.read_works() else {
            return summary;
        };
        let mut actionable_merged: Vec<_> = bundles
            .values()
            .filter(|b| b.status() == BundleStatus::Merged)
            .filter(|b| {
                works
                    .get(&b.work_id)
                    .map(|w| !matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
                    .unwrap_or(true)
            })
            .collect();
        actionable_merged.sort_by_key(|b| b.created_at);
        if !actionable_merged.is_empty() {
            summary.push_str("### Recently Merged Bundles (WI needs advancing)\n");
            for b in &actionable_merged {
                let wi_status = works
                    .get(&b.work_id)
                    .map(|w| w.status().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                summary.push_str(&format!(
                    "- [{}] Merged (wi: {} [{}], branch: {})\n",
                    b.id, b.work_id, wi_status, b.branch_name
                ));
            }
            summary.push('\n');
        }
    }

    // C3: Rejected Bundles whose parent Work is still InReview (needs rollback)
    {
        let Ok(bundles) = stores.read_bundles() else {
            return summary;
        };
        let Ok(works) = stores.read_works() else {
            return summary;
        };
        let mut rejected: Vec<_> = bundles
            .values()
            .filter(|b| b.status() == BundleStatus::Rejected)
            .filter(|b| {
                works
                    .get(&b.work_id)
                    .map(|w| w.status() == WorkStatus::InReview)
                    .unwrap_or(false)
            })
            .collect();
        rejected.sort_by_key(|b| b.created_at);
        if !rejected.is_empty() {
            summary.push_str("### Rejected Bundles (Work needs reset to Ready)\n");
            for b in &rejected {
                let reason = if b.verification.is_empty() {
                    "bundle was rejected by reviewer".to_string()
                } else {
                    b.verification.clone()
                };
                summary.push_str(&format!(
                    "- [{}] REJECTED (work: {} is InReview, reason: {}) \
                     ACTION: use override_work on {} with target_status Ready \
                     and reason 'bundle {} rejected'. \
                     The worker pool will auto-assign a new implementer.\n",
                    b.id, b.work_id, reason, b.work_id, b.id
                ));
            }
            summary.push('\n');
        }
    }

    // --- Active Agent Sessions ---
    {
        let Ok(sessions) = stores.read_agent_sessions() else {
            return summary;
        };
        let mut active: Vec<_> = sessions.values().filter(|s| !s.status().is_terminal()).collect();
        active.sort_by_key(|s| s.created_at);
        if !active.is_empty() {
            summary.push_str("### Active Agents\n");
            for s in &active {
                let target = s
                    .work_id
                    .as_deref()
                    .or(s.bundle_id.as_deref())
                    .or(s.target_id.as_deref())
                    .unwrap_or("global");
                summary.push_str(&format!(
                    "- [{}] {} {} (target: {})\n",
                    s.id,
                    s.agent_type,
                    s.status(),
                    target
                ));
            }
            summary.push('\n');
        }
    }

    // --- Ticks (non-terminal) ---
    {
        let Ok(ticks) = stores.read_ticks() else {
            return summary;
        };
        let mut active: Vec<_> = ticks
            .values()
            .filter(|t| !matches!(t.status(), TickStatus::Published | TickStatus::Failed))
            .collect();
        active.sort_by_key(|t| t.created_at);
        if !active.is_empty() {
            summary.push_str("### Ticks\n");
            for t in &active {
                summary.push_str(&format!("- [{}] {}\n", t.id, t.status()));
            }
            summary.push('\n');
        }
    }

    // --- Active Locks ---
    {
        let Ok(locks) = stores.read_locks() else {
            return summary;
        };
        let active: Vec<_> = locks.values().filter(|l| l.status() == LockStatus::Active).collect();
        if !active.is_empty() {
            summary.push_str("### Active Locks\n");
            for l in &active {
                summary.push_str(&format!("- [{}] {} (holder: {})\n", l.id, l.resource, l.holder_id));
            }
            summary.push('\n');
        }
    }

    // --- Phases ---
    {
        let Ok(phases) = stores.read_phases() else {
            return summary;
        };
        let mut non_terminal: Vec<_> = phases
            .values()
            .filter(|p| !matches!(p.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by(|a, b| a.order.cmp(&b.order).then(a.created_at.cmp(&b.created_at)));
        if !non_terminal.is_empty() {
            summary.push_str("### Phases\n");
            for p in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, spec: {}, order: {})\n",
                    p.id,
                    p.title,
                    p.status(),
                    p.parent_id,
                    p.order
                ));
            }
            summary.push('\n');
        }
    }

    // --- Specs ---
    {
        let Ok(specs) = stores.read_specs() else {
            return summary;
        };
        let mut non_terminal: Vec<_> = specs
            .values()
            .filter(|s| !matches!(s.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|s| s.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### Specs\n");
            for s in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, plan: {})\n",
                    s.id,
                    s.title,
                    s.status(),
                    s.parent_id
                ));
            }
            summary.push('\n');
        }
    }

    // --- Plans ---
    {
        let Ok(plans) = stores.read_plans() else {
            return summary;
        };
        let mut non_terminal: Vec<_> = plans
            .values()
            .filter(|p| !matches!(p.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned))
            .collect();
        non_terminal.sort_by_key(|p| p.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### Plans\n");
            for p in &non_terminal {
                summary.push_str(&format!("- [{}] {} ({})\n", p.id, p.title, p.status()));
            }
            summary.push('\n');
        }
    }

    if summary.is_empty() {
        summary.push_str("No active records. The project is starting from scratch.\n");
    }

    summary
}

/// The Coordinator agent — long-lived FSM-driven agent that manages the planning/execution lifecycle.
pub struct CoordinatorAgent<L: LlmClient> {
    pub ctx: AgentContext,
    llm: L,
    config: CoordinatorConfig,
    iteration: u32,
    previous_summary: Option<String>,
}

/// Fix #2: Resolve batch:N dependency references in a CreateWork action.
/// Returns Some(modified_action) if batch deps were resolved, None if no changes needed.
fn resolve_batch_dependencies(action: &AgentAction, batch_created_ids: &[String], prefix: &str) -> Option<AgentAction> {
    if let AgentAction::CreateWork {
        parent_id,
        title,
        description,
        files,
        acceptance_criteria,
        dependencies,
    } = action
    {
        let has_batch_refs = dependencies.iter().any(|d| d.starts_with("batch:"));
        if !has_batch_refs {
            return None;
        }

        let resolved_deps: Vec<String> = dependencies
            .iter()
            .map(|dep| {
                if let Some(idx_str) = dep.strip_prefix("batch:")
                    && let Ok(idx) = idx_str.parse::<usize>()
                {
                    if let Some(resolved_id) = batch_created_ids.get(idx) {
                        return resolved_id.clone();
                    }
                    tracing::warn!(
                        "{} batch:{} out of range (only {} items created so far)",
                        prefix,
                        idx,
                        batch_created_ids.len()
                    );
                }
                dep.clone()
            })
            .collect();

        Some(AgentAction::CreateWork {
            parent_id: parent_id.clone(),
            title: title.clone(),
            description: description.clone(),
            files: files.clone(),
            acceptance_criteria: acceptance_criteria.clone(),
            dependencies: resolved_deps,
        })
    } else {
        None
    }
}

/// Fix #6: Prune dependencies between batch-created works whose files don't overlap.
/// Safety net for when the LLM creates linear chains between independent works.
fn prune_independent_deps(stores: &Stores, batch_created_ids: &[String], prefix: &str) {
    let batch_set: HashSet<&str> = batch_created_ids.iter().map(|s| s.as_str()).collect();

    // Build files lookup for batch works
    let tag_map: HashMap<String, HashSet<String>> = {
        let Ok(works) = stores.read_works() else {
            tracing::error!("works lock poisoned");
            return;
        };
        batch_created_ids
            .iter()
            .filter_map(|id| works.get(id).map(|w| (id.clone(), w.files.iter().cloned().collect())))
            .collect()
    };

    // Prune deps between batch works with disjoint files
    let Ok(mut works) = stores.write_works() else {
        tracing::error!("works lock poisoned");
        return;
    };
    for wi_id in batch_created_ids {
        if let Some(wi) = works.get_mut(wi_id) {
            let my_tags = match tag_map.get(wi_id) {
                Some(tags) => tags,
                None => continue,
            };
            let before = wi.dependencies.len();
            wi.dependencies.retain(|dep_id| {
                // Only prune deps within this batch — keep deps on external works
                if !batch_set.contains(dep_id.as_str()) {
                    return true;
                }
                match tag_map.get(dep_id) {
                    Some(dep_tags) => !my_tags.is_disjoint(dep_tags),
                    None => true,
                }
            });
            let pruned = before - wi.dependencies.len();
            if pruned > 0 {
                tracing::info!("{} pruned {} independent dep(s) from '{}'", prefix, pruned, wi.title);
            }
        }
    }
}

/// Fix #12: Mark the Phase domain record as Complete before calling coord_state.complete_phase().
/// This ensures the Phase record status and CoordinatorState are updated together.
fn mark_phase_record_complete(stores: &Stores, coord_state: &CoordinatorState, prefix: &str) {
    if let Some(ref phase_id) = coord_state.current_phase_id {
        // L1: Clone-then-drop-then-persist to avoid deadlock and ensure TaskStore persistence
        let phase_to_persist = {
            let Ok(mut phases) = stores.write_phases() else {
                tracing::error!("phases lock poisoned");
                return;
            };
            if let Some(phase) = phases.get_mut(phase_id) {
                phase.force_status(HierarchyStatus::Complete);
                phase.updated_at = crate::id::now_millis();
                tracing::info!("{} Phase {} marked Complete (record status updated)", prefix, phase_id);
                Some(phase.clone())
            } else {
                None
            }
        };
        if let Some(phase) = phase_to_persist
            && let Some(ref store) = stores.store
            && let Ok(mut s) = store.lock().map_err(|_| eyre!("lock poisoned"))
            && let Err(e) = s.update(phase)
        {
            tracing::warn!("{} Failed to persist Phase complete status: {}", prefix, e);
        }
    }
}

/// Check if the Coordinator should mark any Phases as complete based on Work status.
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
                completed.push(format!("Phase '{}' (id: {}) has all Works Done", phase.title, phase.id));
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
        let goals = stores.read_coordinator_goals().ok()?;
        goals.values().find(|g| g.active).map(|g| g.id.clone())?
    };

    // Check if we already have a non-terminal state for this goal
    {
        let states = stores.read_coordinator_states().ok()?;
        if let Some(existing) = states
            .values()
            .find(|s| s.goal_id == goal_id && !s.fsm_state.is_terminal())
        {
            return Some(existing.clone());
        }
    }

    // Create new state
    let interview_mode = stores.config.agents.coordinator.interview_mode;
    let state = CoordinatorState::new(goal_id, interview_mode);
    let id = state.id.clone();
    stores
        .write_coordinator_states()
        .ok()?
        .insert(id.clone(), state.clone());
    if let Some(store_arc) = &stores.store
        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("lock poisoned"))
    {
        let _ = s.create(state.clone());
    }
    Some(state)
}

/// Persist the CoordinatorState to both in-memory and TaskStore.
fn persist_coordinator_state(stores: &Stores, state: &CoordinatorState) {
    let Ok(mut states) = stores.write_coordinator_states() else {
        tracing::error!("coordinator_states lock poisoned");
        return;
    };
    states.insert(state.id.clone(), state.clone());
    drop(states);
    if let Some(store_arc) = &stores.store
        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("lock poisoned"))
    {
        let _ = s.update(state.clone());
    }
}

/// Apply an FSM state transition. Returns `Some(IterationOutcome)` if the caller
/// should return early (GoalComplete), or `None` to continue the iteration.
fn apply_fsm_transition(
    new_state: CoordinatorFsmState,
    coord_state: &mut CoordinatorState,
    stores: &Stores,
    prefix: &str,
) -> Option<IterationOutcome> {
    let old_state = coord_state.fsm_state;
    tracing::info!(
        "{} {} -> {} (goal: {})",
        prefix,
        old_state,
        new_state,
        coord_state.goal_id
    );
    // Handle ActivatePhase: complete previous phase, find and set the next phase
    if new_state == CoordinatorFsmState::ActivatePhase {
        // If transitioning from PhaseGate, complete the previous phase
        if coord_state.current_phase_id.is_some() {
            mark_phase_record_complete(stores, coord_state, prefix);
            coord_state.complete_phase();
        }
        let next_phase = find_next_phase_to_activate(stores, coord_state);
        if let Some((phase_id, phase_title)) = next_phase {
            tracing::info!("{} activating phase: {} ({})", prefix, phase_title, phase_id);
            coord_state.current_phase_id = Some(phase_id);
            coord_state.phase_activated_at = Some(crate::id::now_millis());
            coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
            coord_state.updated_at = crate::id::now_millis();
        } else {
            coord_state.transition_to(CoordinatorFsmState::GoalComplete);
            // Deactivate the goal - the normal completion path goes through ActivatePhase
            // (no next phase found), not through the explicit GoalComplete branch below.
            if let Ok(mut goals) = stores.write_coordinator_goals()
                && let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id)
            {
                goal.deactivate();
            }
        }
    } else if new_state == CoordinatorFsmState::PhaseGate {
        // Transition to PhaseGate but DON'T complete_phase() yet —
        // keep current_phase_id so build_phase_status can show context to the LLM.
        // complete_phase() is called on the NEXT transition out of PhaseGate.
        coord_state.transition_to(CoordinatorFsmState::PhaseGate);
    } else if new_state == CoordinatorFsmState::GoalComplete {
        // Complete current phase if transitioning from PhaseGate
        if coord_state.current_phase_id.is_some() {
            mark_phase_record_complete(stores, coord_state, prefix);
            coord_state.complete_phase();
        }
        coord_state.transition_to(CoordinatorFsmState::GoalComplete);
        // Deactivate the goal
        if let Ok(mut goals) = stores.write_coordinator_goals()
            && let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id)
        {
            goal.deactivate();
        }
        persist_coordinator_state(stores, coord_state);
        return Some(IterationOutcome::Done(format!(
            "Goal complete: {} phases completed",
            coord_state.phases_completed.len()
        )));
    } else {
        coord_state.transition_to(new_state);
    }
    persist_coordinator_state(stores, coord_state);
    None
}

/// Deterministic sweep: transition all `Integrated` Works in the current phase to `Done`.
/// The integrator parks Work at `Integrated` after merge+validation; the coordinator
/// acknowledges completion. Runs every iteration during `Executing` state.
fn sweep_integrated_to_done(
    stores: &Stores,
    coord_state: &CoordinatorState,
    bridge: &crate::agents::bridge::AgentIpcBridge,
    prefix: &str,
) {
    tracing::debug!(
        "{} sweep_integrated_to_done(fsm={:?}, phase={:?})",
        prefix,
        coord_state.fsm_state,
        coord_state.current_phase_id,
    );
    if coord_state.fsm_state != CoordinatorFsmState::Executing {
        return;
    }

    // Determine the parent ID for filtering Works:
    // Full mode: current_phase_id (the active Phase)
    // Brief mode: active Plan ID (Works are parented directly to Plan)
    let parent_id = if let Some(id) = &coord_state.current_phase_id {
        id.clone()
    } else if let Some(plan) = generation::find_active_plan(stores) {
        if plan.tier == crate::domain::plan::Tier::Brief {
            plan.id.clone()
        } else {
            return; // Full mode with no phase - nothing to sweep
        }
    } else {
        return;
    };

    let integrated_ids: Vec<String> = {
        let Ok(works) = stores.read_works() else {
            tracing::error!("works lock poisoned");
            return;
        };
        works
            .values()
            .filter(|w| w.parent_id == parent_id && w.status() == WorkStatus::Integrated)
            .map(|w| w.id.clone())
            .collect()
    };
    for wi_id in &integrated_ids {
        let resp = bridge.request(
            "work.transition",
            serde_json::json!({
                "id": wi_id,
                "target_status": "Done",
                "role": "coordinator",
            }),
        );
        if resp.is_error() {
            let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
            tracing::error!("{} failed to transition WI {} Integrated->Done: {}", prefix, wi_id, msg);
            continue;
        } else {
            tracing::info!("{} Work {} transitioned Integrated -> Done", prefix, wi_id);
        }
    }
}

/// Phase-level validation_commands were removed in domain-model-cleanup Phase 3.
/// This function now always returns empty - validation commands live only in IntegratorConfig.
fn phase_missing_test_tool(_stores: &Stores, _coord_state: &CoordinatorState) -> String {
    String::new()
}

/// Determine the FSM state footer -- state-specific instructions for the LLM.
///
/// `goal` and `config` are retained in the signature for future use
/// (e.g., re-decomposition prompts during ActivatePhase). The Planning branch no
/// longer uses them because decomposition happens before the Coordinator starts.
fn build_fsm_footer(
    stores: &Stores,
    coord_state: &CoordinatorState,
    goal: &str,
    config: &CoordinatorConfig,
    prefix: &str,
) -> String {
    // Retained for ActivatePhase re-decomposition (future wiring)
    let _ = (goal, config, prefix);
    match coord_state.fsm_state {
        CoordinatorFsmState::Interviewing => {
            // In Interviewing state, the Coordinator generates interview questions
            // or proposes a Plan. This is handled by the interview IPC handlers;
            // the FSM footer just signals the state.
            "## Interviewing\n\n\
             You are in the Interviewing state. Generate interview questions to clarify the user's goal, \
             or propose a Plan if you have enough context.\n\n\
             Use InterviewQuestion to ask the user questions, or ProposePlan to propose a Plan draft.\n\n\
             Respond with a JSON array of actions."
                .to_string()
        }
        CoordinatorFsmState::Decomposing => "Background decomposition is in progress. \
             Respond with: [{\"action\": \"done\", \"summary\": \"Waiting for decomposition to complete\"}]"
            .to_string(),
        CoordinatorFsmState::Planning => {
            // Decomposition is handled by the Decomposer before the Coordinator starts.
            // The Planning state is a transient pass-through: the hierarchy already exists.
            "All planning artifacts have been decomposed by the Decomposer. \
             Respond with: [{\"action\": \"done\", \"summary\": \"Planning complete, ready to activate first phase\"}]"
                .to_string()
        }
        CoordinatorFsmState::ActivatePhase => {
            // Find the next phase to activate and generate Works for it
            let phase_info = find_next_phase_to_activate(stores, coord_state);
            match phase_info {
                Some((phase_id, phase_title)) => {
                    let phase = {
                        let Ok(phases) = stores.read_phases() else {
                            return "phases lock poisoned".to_string();
                        };
                        phases.get(&phase_id).cloned()
                    };
                    if let Some(phase) = phase {
                        let existing = generation::find_works_for_parent(stores, &phase.id);
                        if existing.is_empty() {
                            let phase_content = crate::domain::markdown::read_doc_content_or_empty(
                                &stores.config.project.repo_path,
                                &phase.id,
                            );
                            let prompt = build_work_prompt(
                                &phase,
                                &phase_content,
                                &existing,
                                &HashMap::new(),
                                &[],
                                &[],
                                None,
                                None,
                            );
                            format!(
                                "## Activating Phase: {} (id: {})\n\n\
                                 Generate Works for this phase. Each Work should have clear \
                                 acceptance criteria and declare dependencies on other Works in this phase \
                                 using their IDs.\n\n{}",
                                phase_title, phase_id, prompt.user_message
                            )
                        } else {
                            format!(
                                "Phase '{}' already has {} Works. \
                                 Respond with: [{{\"action\": \"done\", \"summary\": \"Phase {} Works ready\"}}]",
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
            // Build executing context — monitor works, assign agents, triage bundles
            let phase_status = build_phase_status(stores, coord_state);

            // Phase-level tool guard: warn if validation tools are required but missing
            let tool_warning = phase_missing_test_tool(stores, coord_state);

            format!(
                "## Executing Phase\n\n{}\n\n{}\
                 Monitor Work statuses. Assign implementers to Ready Works whose dependencies are all Done. \
                 Triage proposed Bundles. Accept reviewed Bundles. \
                 If a Work is Blocked or has failed, consider retrying.\n\n\
                 Respond with a JSON array of actions.",
                phase_status, tool_warning
            )
        }
        CoordinatorFsmState::PhaseGate => {
            let phase_status = build_phase_status(stores, coord_state);
            format!(
                "## Phase Gate Check\n\n{}\n\n\
                 All Works in this phase should be in a terminal state (Done, Abandoned, or NeedHelp). \
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

/// Find the next phase to activate, respecting sequential execution order.
///
/// Execution model: Specs execute sequentially (by `order`). Within each
/// spec, phases execute sequentially (by `order`). Within an active phase,
/// work items execute in parallel (constrained only by dependencies and
/// shared files). This function finds the next phase to activate - it does
/// not manage work-level parallelism.
///
/// Returns the first Draft phase in the earliest spec that still has
/// non-terminal phases. Returns None if a phase is already in progress
/// or all specs are exhausted.
///
/// Critical invariant: never look at phases from Spec N+1 while Spec N
/// has non-terminal phases.
fn find_next_phase_to_activate(stores: &Stores, _coord_state: &CoordinatorState) -> Option<(String, String)> {
    let plan = generation::find_active_plan(stores)?;

    let Ok(all_specs) = stores.read_specs() else {
        return None;
    };
    let mut specs: Vec<_> = all_specs
        .values()
        .filter(|s| s.parent_id == plan.id)
        .filter(|s| !matches!(s.status(), HierarchyStatus::Abandoned))
        .collect();
    specs.sort_by_key(|s| s.order);

    let Ok(all_phases) = stores.read_phases() else {
        return None;
    };

    for spec in &specs {
        let mut phases: Vec<_> = all_phases.values().filter(|p| p.parent_id == spec.id).collect();
        phases.sort_by_key(|p| p.order);

        let has_non_terminal = phases
            .iter()
            .any(|p| !matches!(p.status(), HierarchyStatus::Complete | HierarchyStatus::Abandoned));

        if has_non_terminal {
            // Return the first Draft phase in this spec
            for phase in &phases {
                if phase.status() == HierarchyStatus::Draft {
                    return Some((phase.id.clone(), phase.title.clone()));
                }
            }
            // No Draft phases but some are still Active/in-progress - wait
            return None;
        }
        // All phases in this spec are terminal - continue to next spec
    }

    // All specs exhausted
    None
}

/// Build a status summary for the current phase's works.
fn build_phase_status(stores: &Stores, coord_state: &CoordinatorState) -> String {
    // Determine the parent ID and title for the status display.
    // Full mode: current_phase_id -> Phase title
    // Brief mode: active Plan ID -> Plan title
    let (parent_id, parent_title) = if let Some(ref phase_id) = coord_state.current_phase_id {
        let title = stores
            .read_phases()
            .ok()
            .and_then(|phases| phases.get(phase_id).map(|p| p.title.clone()))
            .unwrap_or_default();
        (phase_id.clone(), title)
    } else if let Some(plan) = generation::find_active_plan(stores) {
        if plan.tier == crate::domain::plan::Tier::Brief {
            let title = plan.title.clone();
            (plan.id.clone(), title)
        } else {
            return "No active phase.".to_string();
        }
    } else {
        return "No active phase.".to_string();
    };
    let phase_id = &parent_id;

    let phase_title = parent_title;

    let Ok(works) = stores.read_works() else {
        return "works lock poisoned".to_string();
    };
    let phase_wis: Vec<_> = works.values().filter(|w| &w.parent_id == phase_id).collect();

    // Split into actionable vs terminal so the LLM can clearly distinguish
    // which works need attention and which are finished.
    let mut actionable = Vec::new();
    let mut terminal = Vec::new();
    for wi in &phase_wis {
        if matches!(wi.status(), WorkStatus::Done | WorkStatus::Abandoned) {
            terminal.push(*wi);
        } else {
            actionable.push(*wi);
        }
    }

    let mut summary = format!(
        "Phase: {} (id: {})\nWorks: {} total ({} actionable, {} terminal)\n\n",
        phase_title,
        phase_id,
        phase_wis.len(),
        actionable.len(),
        terminal.len(),
    );

    // Actionable works: these are the ONLY works eligible for assignment
    if actionable.is_empty() {
        summary
            .push_str("### Actionable Works (eligible for assignment)\nNone - all works are in a terminal state.\n\n");
    } else {
        summary.push_str("### Actionable Works (eligible for assignment)\n");
        for wi in &actionable {
            let attempts = coord_state.attempts(&wi.id);
            let attempt_note = if attempts > 0 { format!(" [{} attempts]", attempts) } else { String::new() };
            summary.push_str(&format!(
                "- [{}] {} (status: {}){}\n",
                wi.id,
                wi.title,
                wi.status(),
                attempt_note
            ));
            // Show dependency info inline
            if !wi.dependencies.is_empty() {
                let dep_status: Vec<String> = wi
                    .dependencies
                    .iter()
                    .map(|dep_id| {
                        let status = works
                            .get(dep_id)
                            .map(|d| format!("{}", d.status()))
                            .unwrap_or_else(|| "unknown".to_string());
                        format!("{}={}", dep_id, status)
                    })
                    .collect();
                let all_met = wi.dependencies.iter().all(|dep_id| {
                    works
                        .get(dep_id)
                        .map(|d| d.status() == WorkStatus::Done)
                        .unwrap_or(false)
                });
                summary.push_str(&format!(
                    "    deps: [{}] ({})\n",
                    dep_status.join(", "),
                    if all_met { "READY" } else { "BLOCKED" }
                ));
            }
        }
        summary.push('\n');
    }

    // Terminal works: DO NOT assign agents to these
    if !terminal.is_empty() {
        summary.push_str("### Terminal Works (COMPLETED - do NOT assign agents to these)\n");
        for wi in &terminal {
            summary.push_str(&format!("- [{}] {} ({})\n", wi.id, wi.title, wi.status()));
        }
        summary.push('\n');
    }

    // Fix #9: Collect WI IDs (owned) before dropping works lock
    let wi_ids: std::collections::HashSet<String> = phase_wis.iter().map(|w| w.id.clone()).collect();
    drop(works);

    // Surface phase-specific failure Learnings
    {
        let Ok(learnings) = stores.read_learnings() else {
            return summary;
        };
        let phase_failures: Vec<_> = learnings
            .values()
            .filter(|l| l.scope == LearningScope::Phase && wi_ids.contains(&l.source_id))
            .collect();

        if !phase_failures.is_empty() {
            summary.push_str("Recent failure learnings:\n");
            for learning in phase_failures.iter().take(5) {
                summary.push_str(&format!("  - {}\n", learning.content));
            }
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
    tracing::debug!(
        "check_fsm_transition(current={:?}, goal_id={}, plan_approved={})",
        coord_state.fsm_state,
        coord_state.goal_id,
        coord_state.plan_approved,
    );
    match coord_state.fsm_state {
        CoordinatorFsmState::Interviewing => {
            // Transition to Planning when plan_approved is set
            if coord_state.plan_approved {
                Some(CoordinatorFsmState::Planning)
            } else {
                None
            }
        }
        CoordinatorFsmState::Decomposing => {
            // Advance to Planning once the background decomposition task has persisted phases.
            let plan = generation::find_active_plan(stores)?;
            let specs = generation::find_active_specs_for_plan(stores, &plan.id);
            let has_phases = specs
                .iter()
                .any(|s| !generation::find_active_phases_for_spec(stores, &s.id).is_empty());
            if has_phases { Some(CoordinatorFsmState::Planning) } else { None }
        }
        CoordinatorFsmState::Planning => {
            // Decomposition is complete before the Coordinator starts.
            // Transition immediately based on what the Decomposer produced.
            let plan = generation::find_active_plan(stores)?;

            // Brief mode: Works are parented to Plan directly
            if plan.tier == crate::domain::plan::Tier::Brief {
                let wis = generation::find_works_for_parent(stores, &plan.id);
                return if !wis.is_empty() { Some(CoordinatorFsmState::Executing) } else { None };
            }

            // Full mode: Decomposer already created Specs and Phases - go to ActivatePhase
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
            // Transition when: Works for current phase exist and are Ready
            if let Some(ref phase_id) = coord_state.current_phase_id {
                let wis = generation::find_works_for_parent(stores, phase_id);
                if !wis.is_empty() {
                    return Some(CoordinatorFsmState::Executing);
                }
            }
            // If we just set current_phase_id, wait for Works to be created
            None
        }
        CoordinatorFsmState::Executing => {
            // Brief mode: Works are parented directly to Plan, no phases
            if let Some(plan) =
                generation::find_active_plan(stores).filter(|p| p.tier == crate::domain::plan::Tier::Brief)
            {
                // Goal timeout
                let goal_elapsed_ms = crate::id::now_millis() - coord_state.goal_started_at;
                let goal_timeout_ms = config.goal_timeout_secs as i64 * 1000;
                if goal_elapsed_ms > goal_timeout_ms {
                    return Some(CoordinatorFsmState::GoalComplete);
                }

                let wis = generation::find_works_for_parent(stores, &plan.id);
                if wis.is_empty() {
                    return None; // Wait for Works to be generated
                }
                if wis
                    .iter()
                    .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
                {
                    return Some(CoordinatorFsmState::GoalComplete);
                }
                return None;
            }

            // Full mode: phase timeout fires first (local - advance this phase)
            if let Some(activated_at) = coord_state.phase_activated_at {
                let elapsed_ms = crate::id::now_millis() - activated_at;
                let timeout_ms = config.phase_timeout_secs as i64 * 1000;
                if elapsed_ms > timeout_ms {
                    return Some(CoordinatorFsmState::PhaseGate);
                }
            }

            // Full mode: goal timeout (global - kill the entire goal)
            let goal_elapsed_ms = crate::id::now_millis() - coord_state.goal_started_at;
            let goal_timeout_ms = config.goal_timeout_secs as i64 * 1000;
            if goal_elapsed_ms > goal_timeout_ms {
                return Some(CoordinatorFsmState::GoalComplete);
            }

            // Full mode: all WIs in current phase are terminal
            if let Some(ref phase_id) = coord_state.current_phase_id {
                let wis = generation::find_works_for_parent(stores, phase_id);
                if wis.is_empty() {
                    return Some(CoordinatorFsmState::PhaseGate);
                }
                if wis
                    .iter()
                    .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
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

mod run;

impl<L: LlmClient + 'static> Agent for CoordinatorAgent<L> {
    async fn run(&mut self) -> Result<()> {
        self.ctx.debug(&format!("run(session_id={})", self.ctx.session.id));

        // Restart loop — Coordinator restarts on transient failures (up to max_restarts).
        // Non-retryable errors (NeedHelp) propagate immediately.
        let max_restarts = 3u32;
        let restart_delay = self.config.idle_interval_secs * 2;
        let mut attempt = 0u32;

        loop {
            // Load or create FSM state
            let result: Result<()> = match load_or_create_coordinator_state(&self.ctx.stores) {
                Some(state) => {
                    self.ctx.info(&format!(
                        "Resuming FSM state: {} (phase: {:?})",
                        state.fsm_state, state.current_phase_id
                    ));
                    self.run_fsm_loop(state).await
                }
                None => {
                    // No active goal yet — sleep and retry instead of calling the LLM.
                    // This avoids a race where the coordinator starts before set-goal
                    // runs, causing the LLM to hallucinate a plan from "No goal set."
                    self.ctx.info("No active goal, waiting for goal to be set");
                    tokio::time::sleep(Duration::from_secs(self.config.idle_interval_secs)).await;
                    if self.ctx.is_cancelled() {
                        return Ok(());
                    }
                    continue;
                }
            };

            match result {
                Ok(()) => return Ok(()),
                Err(ref e) if e.to_string().contains("needs help") => {
                    // NeedHelp is a deliberate exit — don't retry
                    return result;
                }
                Err(e) => {
                    if attempt >= max_restarts {
                        return Err(e);
                    }
                    attempt += 1;
                    self.ctx.warn(&format!(
                        "Coordinator failed (attempt {}/{}), restarting in {}s: {}",
                        attempt, max_restarts, restart_delay, e
                    ));
                    tokio::time::sleep(Duration::from_secs(restart_delay)).await;
                    // Check cancellation during sleep
                    if self.ctx.is_cancelled() {
                        return Ok(());
                    }
                    // Reset iteration for the new run
                    self.iteration = 0;
                    self.previous_summary = None;
                }
            }
        }
    }

    fn agent_type(&self) -> AgentKind {
        AgentKind::Coordinator
    }
}

/// Find the error_kind from the most recent failed session for a given work ID.
fn last_error_kind_for_work(stores: &Stores, work_id: &str) -> Option<AgentErrorKind> {
    let sessions = stores.agent_sessions.read().ok()?;
    sessions
        .values()
        .filter(|s| {
            s.work_id.as_deref() == Some(work_id) && s.status() == AgentStatus::Failed && s.error_kind.is_some()
        })
        .max_by_key(|s| s.updated_at)
        .and_then(|s| s.error_kind)
}

fn format_action_summary(result: &ActionResult) -> String {
    match result {
        ActionResult::ToolRun(tr) => format!("ran {} (exit {})", tr.tool, tr.exit_code),
        ActionResult::FileWritten(p) => format!("wrote {}", p),
        ActionResult::FileEdited(p) => format!("edited {}", p),
        ActionResult::FileRead(content) => format!("read file ({} bytes)", content.len()),
        ActionResult::Committed(m) => format!("committed: {}", m),
        ActionResult::BundleProposed(d) => format!("proposed bundle: {}", d),
        ActionResult::Transitioned(d) => format!("transitioned: {}", d),
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::ToolRegistered(n) => format!("registered tool: {}", n),
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
        ActionResult::CoverageEvaluated { verdict, summary, gaps } => {
            format!("coverage: {} ({}, {} gaps)", verdict, summary, gaps.len())
        }
        ActionResult::DependencyNotMet { work_id, message } => {
            format!("dep not met for {}: {}", work_id, message)
        } // M10-12: DuplicateDetected, PhaseCompleted, GoalCompleted removed — dead variants
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
