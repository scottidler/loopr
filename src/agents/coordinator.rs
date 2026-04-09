use std::collections::{HashMap, HashSet};
use std::time::Duration;

use eyre::{Result, eyre};

use crate::agents::AgentAction;
use crate::agents::context::ContextBuilder;
use crate::agents::error::AgentErrorKind;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::generation;
use crate::agents::implementer::{self, ChatMessage, IterationOutcome, LlmClient};
use crate::agents::lifeguard::{self, Lifeguard, Verdict};
use crate::agents::{Agent, AgentContext, AgentKind, AgentStatus};
use crate::config::CoordinatorConfig;
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};
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
        non_terminal.sort_by_key(|p| p.created_at);
        if !non_terminal.is_empty() {
            summary.push_str("### Phases\n");
            for p in &non_terminal {
                summary.push_str(&format!(
                    "- [{}] {} ({}, spec: {})\n",
                    p.id,
                    p.title,
                    p.status(),
                    p.parent_id,
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

/// Safety net for when the LLM omits dependencies between works that share a file.
///
/// Algorithm (cycle-safe):
/// 1. Build file -> Vec<work_id> mapping for works in the batch.
/// 2. For each file claimed by 2+ works, build the existing dep subgraph.
/// 3. For pairs with no directed path between them, inject a dep:
///    - prefer the LLM-declared direction if one already partially exists;
///    - use batch-creation order (ascending id index) as a tie-breaker for fully disconnected pairs.
/// 4. After each injection, verify no cycle is introduced (topological sort).
///    If a cycle would result, skip the edge and log a warning.
fn inject_overlap_deps(stores: &Stores, batch_created_ids: &[String], prefix: &str) {
    // Build file -> [work_id] mapping (only for works in this batch)
    let file_to_works: HashMap<String, Vec<String>> = {
        let Ok(works) = stores.read_works() else {
            tracing::error!("inject_overlap_deps: works lock poisoned");
            return;
        };
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for id in batch_created_ids {
            if let Some(w) = works.get(id) {
                for f in &w.files {
                    map.entry(f.clone()).or_default().push(id.clone());
                }
            }
        }
        map
    };

    // Collect pairs that share at least one file: (earlier_in_batch, later_in_batch)
    // We'll use batch-index as tie-breaker direction.
    let batch_index: HashMap<&str, usize> = batch_created_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();

    // Collect the set of pairs that need a dependency (bidirectional check)
    let mut pairs_needing_dep: Vec<(String, String)> = Vec::new();
    for ids in file_to_works.values() {
        if ids.len() < 2 {
            continue;
        }
        // For each pair, ensure a dependency exists in one direction
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let a = &ids[i];
                let b = &ids[j];
                pairs_needing_dep.push((a.clone(), b.clone()));
            }
        }
    }
    pairs_needing_dep.sort();
    pairs_needing_dep.dedup();

    if pairs_needing_dep.is_empty() {
        return;
    }

    // Read the current dep graph for batch works
    let mut dep_graph: HashMap<String, HashSet<String>> = {
        let Ok(works) = stores.read_works() else {
            return;
        };
        batch_created_ids
            .iter()
            .filter_map(|id| {
                works.get(id).map(|w| {
                    let batch_deps: HashSet<String> = w
                        .dependencies
                        .iter()
                        .filter(|d| batch_index.contains_key(d.as_str()))
                        .cloned()
                        .collect();
                    (id.clone(), batch_deps)
                })
            })
            .collect()
    };

    /// Returns true if there is a directed path from `from` to `to` in the dep graph.
    fn has_path(graph: &HashMap<String, HashSet<String>>, from: &str, to: &str) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![from.to_string()];
        while let Some(node) = stack.pop() {
            if node == to {
                return true;
            }
            if visited.contains(&node) {
                continue;
            }
            visited.insert(node.clone());
            if let Some(deps) = graph.get(&node) {
                stack.extend(deps.iter().cloned());
            }
        }
        false
    }

    /// Returns true if adding edge (from -> to) would create a cycle.
    fn would_create_cycle(graph: &HashMap<String, HashSet<String>>, from: &str, to: &str) -> bool {
        // A cycle would exist if `from` is already reachable from `to`
        has_path(graph, to, from)
    }

    let mut injected = Vec::new();

    for (a, b) in &pairs_needing_dep {
        // Check if a path already exists in either direction
        let a_to_b = has_path(&dep_graph, a, b);
        let b_to_a = has_path(&dep_graph, b, a);

        if a_to_b || b_to_a {
            // A relationship already exists; no injection needed
            continue;
        }

        // No path exists: use batch index to determine direction
        let (predecessor, successor) = match (batch_index.get(a.as_str()), batch_index.get(b.as_str())) {
            (Some(&ia), Some(&ib)) if ia <= ib => (a.clone(), b.clone()),
            (Some(_), Some(_)) => (b.clone(), a.clone()),
            _ => (a.clone(), b.clone()),
        };

        // Safety: check that adding predecessor -> successor doesn't create a cycle
        if would_create_cycle(&dep_graph, &predecessor, &successor) {
            tracing::warn!(
                "{} inject_overlap_deps: skipping edge {} -> {} (would create cycle)",
                prefix,
                predecessor,
                successor
            );
            continue;
        }

        // Inject: successor now depends on predecessor
        dep_graph
            .entry(successor.clone())
            .or_default()
            .insert(predecessor.clone());
        injected.push((predecessor, successor));
    }

    if injected.is_empty() {
        return;
    }

    // Persist the injected deps
    let Ok(mut works) = stores.write_works() else {
        tracing::error!("inject_overlap_deps: works lock poisoned on write");
        return;
    };
    for (predecessor, successor) in &injected {
        if let Some(w) = works.get_mut(successor)
            && !w.dependencies.contains(predecessor)
        {
            w.dependencies.push(predecessor.clone());
            tracing::info!(
                "{} inject_overlap_deps: injected dep {} -> {} (shared file)",
                prefix,
                predecessor,
                successor
            );
        }
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

// mark_phase_record_complete removed - reconciliation handles Phase completion.

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

/// Scan parsed coordinator actions for coherence: if an `override_work` with
/// `target_status: "Abandoned"` mentions creating replacements in the reason
/// field but no `create_work` action is present, log a warning.
/// Returns a list of warning strings (empty if coherent).
pub(crate) fn validate_action_coherence(actions: &[AgentAction], prefix: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let has_create = actions.iter().any(|a| matches!(a, AgentAction::CreateWork { .. }));

    for action in actions {
        if let AgentAction::OverrideWork {
            work_id,
            target_status,
            reason,
        } = action
            && target_status == "Abandoned"
        {
            let lower = reason.to_lowercase();
            let mentions_create = lower.contains("creating")
                || lower.contains("create_work")
                || lower.contains("replacement")
                || lower.contains("replacing")
                || lower.contains("fix work");
            if mentions_create && !has_create {
                let msg = format!(
                    "{} override_work on {} mentions creating replacements \
                     but no create_work action in payload",
                    prefix, work_id,
                );
                tracing::warn!("{}", msg);
                warnings.push(msg);
            }
        }
    }
    warnings
}

/// Check the abandon-ratio quality gate for a goal at GoalComplete.
/// If the terminal-only abandon ratio exceeds the configured threshold, returns
/// `Some(IterationOutcome::NeedHelp(...))`. Otherwise returns `None` (gate passed).
/// This is extracted so both GoalComplete code paths use identical gate logic.
fn check_abandon_gate(stores: &Stores, coord_state: &CoordinatorState, prefix: &str) -> Option<IterationOutcome> {
    let ratio = generation::goal_abandon_ratio_terminal(stores, &coord_state.goal_id);
    let max_ratio = stores.config.agents.coordinator.max_abandon_ratio;
    if ratio > max_ratio {
        let (done_count, _total_all, terminal_count, abandoned_count) =
            generation::goal_work_counts(stores, &coord_state.goal_id);
        let reason = format!(
            "Quality gate: {abandoned_count}/{terminal_count} terminal works abandoned \
             ({:.0}% > {:.0}% threshold). {done_count}/{terminal_count} completed.",
            ratio * 100.0,
            max_ratio * 100.0,
        );
        tracing::warn!("{} {}", prefix, reason);
        Some(IterationOutcome::NeedHelp(reason))
    } else {
        None
    }
}

/// Apply an FSM state transition. Returns `Some(IterationOutcome)` if the caller
/// should return early (GoalComplete or quality gate failure), or `None` to continue.
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
    if new_state == CoordinatorFsmState::GoalComplete {
        coord_state.transition_to(CoordinatorFsmState::GoalComplete);
        // Check quality gate before declaring success.
        if let Some(outcome) = check_abandon_gate(stores, coord_state, prefix) {
            persist_coordinator_state(stores, coord_state);
            return Some(outcome);
        }
        // Gate passed: deactivate the goal.
        if let Ok(mut goals) = stores.write_coordinator_goals()
            && let Some(goal) = goals.values_mut().find(|g| g.id == coord_state.goal_id)
        {
            goal.deactivate();
        }
        persist_coordinator_state(stores, coord_state);
        return Some(IterationOutcome::Done("Goal complete".to_string()));
    }
    coord_state.transition_to(new_state);
    persist_coordinator_state(stores, coord_state);
    None
}

/// Deterministic sweep: transition all `Integrated` Works to `Done`.
/// The integrator parks Work at `Integrated` after merge+validation; the coordinator
/// acknowledges completion. Runs every iteration during `Executing` state.
fn sweep_integrated_to_done(
    stores: &Stores,
    coord_state: &CoordinatorState,
    bridge: &crate::agents::bridge::AgentIpcBridge,
    prefix: &str,
) {
    tracing::debug!("{} sweep_integrated_to_done(fsm={:?})", prefix, coord_state.fsm_state,);
    if coord_state.fsm_state != CoordinatorFsmState::Executing {
        return;
    }

    // Sweep ALL Integrated Works regardless of parent - reconciliation handles ordering.
    let integrated_ids: Vec<String> = {
        let Ok(works) = stores.read_works() else {
            tracing::error!("works lock poisoned");
            return;
        };
        works
            .values()
            .filter(|w| w.status() == WorkStatus::Integrated)
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
        CoordinatorFsmState::Planning => "All planning artifacts have been decomposed by the Decomposer. \
             Respond with: [{\"action\": \"done\", \"summary\": \"Planning complete, ready to execute\"}]"
            .to_string(),
        CoordinatorFsmState::Executing => {
            let execution_status = build_execution_status(stores, coord_state);
            format!(
                "## Executing\n\n{}\n\
                 Monitor Work statuses. Assign implementers to Ready Works. \
                 Triage proposed Bundles. Accept reviewed Bundles. \
                 If a Work is Blocked or has failed, consider retrying.\n\n\
                 Respond with a JSON array of actions.",
                execution_status,
            )
        }
        CoordinatorFsmState::GoalComplete => "Goal is complete. \
             Respond with: [{\"action\": \"done\", \"summary\": \"Goal complete\"}]"
            .to_string(),
    }
}

/// Build execution status showing all Active Phases and their Works.
/// Replaces build_phase_status - shows multi-phase view instead of single cursor.
fn build_execution_status(stores: &Stores, coord_state: &CoordinatorState) -> String {
    let Some(plan) = generation::find_active_plan(stores) else {
        return "No active plan.".to_string();
    };

    let mut summary = String::with_capacity(2048);

    // Brief mode: show works directly under Plan
    if plan.tier == crate::domain::plan::Tier::Brief {
        let Ok(works) = stores.read_works() else {
            return "works lock poisoned".to_string();
        };
        let plan_works: Vec<_> = works.values().filter(|w| w.parent_id == plan.id).collect();
        summary.push_str(&format!("Plan: {} (Brief mode)\n\n", plan.title));
        append_work_status(&mut summary, &plan_works, coord_state, &works);
        return summary;
    }

    // Full mode: show all Active Phases and their Works
    let Ok(specs) = stores.read_specs() else {
        return "specs lock poisoned".to_string();
    };
    let Ok(phases) = stores.read_phases() else {
        return "phases lock poisoned".to_string();
    };
    let Ok(works) = stores.read_works() else {
        return "works lock poisoned".to_string();
    };

    let mut active_specs: Vec<_> = specs
        .values()
        .filter(|s| s.parent_id == plan.id && s.status() == HierarchyStatus::Active)
        .collect();
    active_specs.sort_by_key(|s| s.created_at);

    for spec in &active_specs {
        let mut spec_phases: Vec<_> = phases
            .values()
            .filter(|p| p.parent_id == spec.id && p.status() == HierarchyStatus::Active)
            .collect();
        spec_phases.sort_by_key(|p| p.created_at);

        for phase in &spec_phases {
            summary.push_str(&format!(
                "### Phase: {} (id: {}, spec: {})\n",
                phase.title, phase.id, spec.title
            ));
            let phase_works: Vec<_> = works.values().filter(|w| w.parent_id == phase.id).collect();
            append_work_status(&mut summary, &phase_works, coord_state, &works);
        }
    }

    if summary.is_empty() {
        summary.push_str("No active phases. Reconciliation will promote Pending phases when deps are met.\n");
    }
    summary
}

/// Append work status for a set of works - shared between Brief and Full modes.
fn append_work_status(
    summary: &mut String,
    phase_works: &[&crate::domain::work::Work],
    coord_state: &CoordinatorState,
    all_works: &std::collections::HashMap<String, crate::domain::work::Work>,
) {
    let mut actionable = Vec::new();
    let mut terminal = Vec::new();
    for wi in phase_works {
        if matches!(wi.status(), WorkStatus::Done | WorkStatus::Abandoned) {
            terminal.push(*wi);
        } else {
            actionable.push(*wi);
        }
    }

    summary.push_str(&format!(
        "Works: {} total ({} actionable, {} terminal)\n",
        phase_works.len(),
        actionable.len(),
        terminal.len(),
    ));

    if !actionable.is_empty() {
        summary.push_str("Actionable:\n");
        for wi in &actionable {
            let attempts = coord_state.attempts(&wi.id);
            let attempt_note = if attempts > 0 { format!(" [{} attempts]", attempts) } else { String::new() };
            summary.push_str(&format!(
                "- [{}] {} ({}){}\n",
                wi.id,
                wi.title,
                wi.status(),
                attempt_note
            ));
            if !wi.dependencies.is_empty() {
                let dep_info: Vec<String> = wi
                    .dependencies
                    .iter()
                    .map(|dep_id| {
                        let status = all_works
                            .get(dep_id)
                            .map(|d| format!("{}", d.status()))
                            .unwrap_or_else(|| "unknown".to_string());
                        format!("{}={}", dep_id, status)
                    })
                    .collect();
                summary.push_str(&format!("    deps: [{}]\n", dep_info.join(", ")));
            }
        }
    }

    if !terminal.is_empty() {
        summary.push_str("Terminal (do NOT assign):\n");
        for wi in &terminal {
            summary.push_str(&format!("- [{}] {} ({})\n", wi.id, wi.title, wi.status()));
        }
    }
    summary.push('\n');
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
            if coord_state.plan_approved {
                Some(CoordinatorFsmState::Planning)
            } else {
                None
            }
        }
        CoordinatorFsmState::Decomposing => {
            // Advance to Planning once background decomposition has persisted hierarchy.
            let plan = generation::find_active_plan(stores)?;
            let Ok(specs) = stores.read_specs() else { return None };
            let has_children = specs.values().any(|s| s.parent_id == plan.id);
            if has_children {
                Some(CoordinatorFsmState::Planning)
            } else {
                // Brief mode: check for works parented to plan
                let wis = generation::find_works_for_parent(stores, &plan.id);
                if !wis.is_empty() { Some(CoordinatorFsmState::Planning) } else { None }
            }
        }
        CoordinatorFsmState::Planning => {
            // Transition to Executing when hierarchy exists.
            let plan = generation::find_active_plan(stores)?;
            if plan.tier == crate::domain::plan::Tier::Brief {
                let wis = generation::find_works_for_parent(stores, &plan.id);
                return if !wis.is_empty() { Some(CoordinatorFsmState::Executing) } else { None };
            }
            // Full mode: need Specs to exist
            let Ok(specs) = stores.read_specs() else { return None };
            let has_specs = specs.values().any(|s| s.parent_id == plan.id);
            if has_specs { Some(CoordinatorFsmState::Executing) } else { None }
        }
        CoordinatorFsmState::Executing => {
            // Goal timeout
            let goal_elapsed_ms = crate::id::now_millis() - coord_state.goal_started_at;
            let goal_timeout_ms = config.goal_timeout_secs as i64 * 1000;
            if goal_elapsed_ms > goal_timeout_ms {
                return Some(CoordinatorFsmState::GoalComplete);
            }
            // Goal complete is detected by reconcile() and handled in run_iteration.
            None
        }
        CoordinatorFsmState::GoalComplete => None,
    }
}

pub(crate) mod reconcile;
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
