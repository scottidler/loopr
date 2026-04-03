use std::collections::{HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, eyre};

use crate::agents::AgentAction;
use crate::agents::agent_logger::AgentLogger;
use crate::agents::context::ContextBuilder;
use crate::agents::error::AgentErrorKind;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::generation::{
    self, GenerationLevel, build_phase_prompt, build_plan_prompt, build_spec_prompt, build_work_prompt,
};
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
        AgentAction::CreatePlan { .. } => Some("plan"),
        AgentAction::CreateSpec { .. } => Some("spec"),
        AgentAction::CreatePhase { .. } => Some("phase"),
        AgentAction::CreateWork { .. } | AgentAction::AssignAgent { .. } => Some("work"),
        _ => None,
    }
}

/// Build a state summary string from stores for the Coordinator's context.
///
/// Uses lock-snapshot pattern: acquires each lock briefly, clones/summarizes, releases.
/// The summary is designed to fit within the Coordinator's state_summary token budget (3000 tokens).
pub fn build_state_summary(stores: &Stores, agent_log: &AgentLogger) -> String {
    build_state_summary_with_sla(stores, agent_log, None, None)
}

pub fn build_state_summary_with_sla(
    stores: &Stores,
    agent_log: &AgentLogger,
    coord_state: Option<&CoordinatorState>,
    sla_config: Option<&crate::config::WorkSlaConfig>,
) -> String {
    agent_log.debug("build_state_summary_with_sla()");
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
            .filter(|w| !matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
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
                    w.id, w.title, w.status, w.phase_id, sla_annotation
                ));
            }
            summary.push('\n');
        }
    }

    // --- Proposed Bundles (use triage_bundle) ---
    {
        let Ok(bundles) = stores.read_bundles() else {
            return summary;
        };
        let mut proposed: Vec<_> = bundles
            .values()
            .filter(|b| matches!(b.status, BundleStatus::Proposed))
            .collect();
        proposed.sort_by_key(|b| b.created_at);
        if !proposed.is_empty() {
            summary.push_str("### Proposed Bundles (use triage_bundle)\n");
            for b in &proposed {
                summary.push_str(&format!("- [{}] {} (wi: {})\n", b.id, b.status, b.work_id));
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
            .filter(|b| matches!(b.status, BundleStatus::Reviewed))
            .collect();
        reviewed.sort_by_key(|b| b.created_at);
        if !reviewed.is_empty() {
            summary.push_str("### Reviewed Bundles (use accept_bundle)\n");
            for b in &reviewed {
                summary.push_str(&format!("- [{}] {} (wi: {})\n", b.id, b.status, b.work_id));
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
            .filter(|b| b.status == BundleStatus::Merged)
            .filter(|b| {
                works
                    .get(&b.work_id)
                    .map(|w| !matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
                    .unwrap_or(true)
            })
            .collect();
        actionable_merged.sort_by_key(|b| b.created_at);
        if !actionable_merged.is_empty() {
            summary.push_str("### Recently Merged Bundles (WI needs advancing)\n");
            for b in &actionable_merged {
                let wi_status = works
                    .get(&b.work_id)
                    .map(|w| w.status.to_string())
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
            .filter(|b| b.status == BundleStatus::Rejected)
            .filter(|b| {
                works
                    .get(&b.work_id)
                    .map(|w| w.status == WorkStatus::InReview)
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
        let mut active: Vec<_> = sessions.values().filter(|s| !s.status.is_terminal()).collect();
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
                    s.id, s.agent_type, s.status, target
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

    // --- Specs ---
    {
        let Ok(specs) = stores.read_specs() else {
            return summary;
        };
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

    // --- Plans ---
    {
        let Ok(plans) = stores.read_plans() else {
            return summary;
        };
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

    if summary.is_empty() {
        summary.push_str("No active records. The project is starting from scratch.\n");
    }

    summary
}

/// The Coordinator agent — long-lived FSM-driven agent that manages the planning/execution lifecycle.
pub struct CoordinatorAgent {
    pub ctx: AgentContext,
    llm: Box<dyn LlmClient>,
    config: CoordinatorConfig,
    iteration: u32,
    previous_summary: Option<String>,
}

/// Query learnings from stores filtered by scopes relevant to the given generation level.
/// Returns content strings suitable for passing to prompt builders.
///
/// Scope hierarchy per level:
/// - Plan: Plan + Global
/// - Spec: Spec + Plan + Global
/// - Phase: Phase + Spec + Plan + Global
/// - Work: Work + Phase + Spec + Plan + Global
fn query_learnings_for_level(stores: &Stores, level: GenerationLevel) -> Vec<String> {
    let Ok(learnings) = stores.read_learnings() else {
        return Vec::new();
    };
    let scopes: &[LearningScope] = match level {
        GenerationLevel::Plan => &[LearningScope::Plan, LearningScope::Global],
        GenerationLevel::Spec => &[LearningScope::Spec, LearningScope::Plan, LearningScope::Global],
        GenerationLevel::Phase => &[
            LearningScope::Phase,
            LearningScope::Spec,
            LearningScope::Plan,
            LearningScope::Global,
        ],
        GenerationLevel::Work => &[
            LearningScope::Work,
            LearningScope::Phase,
            LearningScope::Spec,
            LearningScope::Plan,
            LearningScope::Global,
        ],
    };
    learnings
        .values()
        .filter(|l| scopes.contains(&l.scope))
        .map(|l| l.content.clone())
        .collect()
}

/// Build a generation-specific footer for the Coordinator's context message.
///
/// Handles three cases:
/// 1. **New generation** — no document at this level → generate from scratch.
/// 2. **Re-generation** — Draft exists with failed validation → regenerate with accumulated failures.
/// 3. **Validation cap reached** — Draft has exceeded max_validation_attempts → NeedHelp signal.
///
/// Returns None when no generation/re-generation is needed (normal iteration).
#[allow(clippy::too_many_arguments)]
fn build_generation_footer(
    stores: &Stores,
    goal: &str,
    max_validation_attempts: u32,
    guidance_section: Option<&str>,
    agent_log: &AgentLogger,
    coord_state: Option<&crate::domain::coordinator_state::CoordinatorState>,
    max_decomposition_attempts: Option<u32>,
    max_bubble_up_depth: Option<u32>,
) -> Option<String> {
    // Case 0: Check if decomposition attempts are exhausted (needs bubble-up or NeedHelp)
    if let (Some(cs), Some(max_da)) = (coord_state, max_decomposition_attempts)
        && let Some((collection, parent_id)) = generation::is_decomposition_cap_reached(stores, cs, max_da)
    {
        let max_bud = max_bubble_up_depth.unwrap_or(2);

        // Can't revise above Plan, and can't exceed bubble-up depth
        if collection == "plan" || cs.bubble_up_count >= max_bud {
            agent_log.info(&format!(
                "bubble-up exhausted for {} {} (count={}, max={}), signaling need_help",
                collection, parent_id, cs.bubble_up_count, max_bud
            ));
            let bubble_count = cs.bubble_up_count;
            return Some(format!(
                "Coverage evaluation for {collection} ({parent_id}) has failed {max_da} times. \
                 Bubble-up depth exhausted ({bubble_count}/{max_bud}). Human review needed.\n\
                 [{{\"action\": \"need_help\", \"reason\": \"Decomposition failed for {collection} {parent_id} after {max_da} attempts and {bubble_count} bubble-ups\"}}]",
            ));
        }

        // Bubble up: emit ReviseParent prompt
        let gaps = generation::get_coverage_gaps(stores, &collection, &parent_id);
        let diagnostic = if gaps.is_empty() {
            format!("Decomposition of {} {} failed {} times", collection, parent_id, max_da)
        } else {
            format!(
                "Decomposition of {} {} failed {} times. Coverage gaps:\n{}",
                collection,
                parent_id,
                max_da,
                gaps.join("\n")
            )
        };
        let diagnostic_escaped = diagnostic.replace('"', "\\\"");

        agent_log.info(&format!(
            "bubbling up: ReviseParent {} {} (bubble_up_count={}/{})",
            collection,
            parent_id,
            cs.bubble_up_count + 1,
            max_bud
        ));

        return Some(format!(
            "## Bubble-Up Required\n\n\
             Coverage evaluation for {collection} ({parent_id}) has failed {max_da} times.\n\
             The children cannot fix this - the parent needs revision.\n\n\
             ### Diagnostic:\n{diagnostic}\n\n\
             Respond with:\n\
             [{{\"action\": \"revise_parent\", \"collection\": \"{collection}\", \"id\": \"{parent_id}\", \
             \"reason\": \"decomposition failed {max_da} times\", \
             \"diagnostic\": \"{diagnostic_escaped}\"}}]"
        ));
    }

    // Case 0b: Check if a parent has Incomplete coverage - re-decompose with gap feedback
    if let (Some(cs), Some(max_da)) = (coord_state, max_decomposition_attempts)
        && let Some(incomplete) = generation::find_incomplete_decomposition(stores, cs, max_da)
    {
        agent_log.info(&format!(
            "re-decomposition needed: {} {} (attempt {}/{})",
            incomplete.parent_collection,
            incomplete.parent_id,
            incomplete.attempt_count + 1,
            max_da
        ));
        let gaps_text = incomplete.gap_descriptions.join("\n");
        return Some(format!(
            "## Re-decomposition Required (attempt {}/{})\n\n\
             Coverage evaluation found gaps in {} ({}).\n\
             First, abandon the existing children, then create NEW children that address ALL gaps:\n\n\
             ### Coverage Gaps:\n{}\n\n\
             Respond with a JSON array of actions to abandon old children and create new ones.",
            incomplete.attempt_count + 1,
            max_da,
            incomplete.parent_collection,
            incomplete.parent_id,
            gaps_text,
        ));
    }

    // Case 1: Check if generation is needed at any level (no document exists)
    if let Some(level) = generation::determine_generation_level(stores) {
        let learnings = query_learnings_for_level(stores, level);
        let prompt = match level {
            GenerationLevel::Plan => build_plan_prompt(goal, &learnings, &[], guidance_section),
            GenerationLevel::Spec => {
                let plan = generation::find_active_plan(stores)?;
                build_spec_prompt(&plan, &learnings, &[], &[], guidance_section)
            }
            GenerationLevel::Phase => {
                let plan = generation::find_active_plan(stores)?;
                let specs = generation::find_active_specs_for_plan(stores, &plan.id);
                let spec = specs.into_iter().next()?;
                build_phase_prompt(&spec, &learnings, &[], guidance_section)
            }
            GenerationLevel::Work => {
                let phase = generation::find_phase_needing_works(stores)?;
                let existing = generation::find_works_for_phase(stores, &phase.id);
                // Thread the original plan description through so the LLM can
                // see the full plan context when generating work items.
                let plan_description = generation::find_active_plan(stores).map(|p| p.description.clone());
                build_work_prompt(
                    &phase,
                    &existing,
                    &learnings,
                    &[],
                    plan_description.as_deref(),
                    guidance_section,
                )
            }
        };

        agent_log.info(&format!(
            "generation needed at level: {} (prompt level: {})",
            level, prompt.level
        ));
        return Some(prompt.user_message);
    }

    // Case 3: Check if validation cap is reached → signal NeedHelp
    // (Only relevant when validator is enabled — no validation failures exist when disabled)
    if stores.validator.is_some() && generation::is_validation_cap_reached(stores, max_validation_attempts) {
        agent_log.info(&format!(
            "validation cap reached ({} attempts), signaling need_help",
            max_validation_attempts
        ));
        return Some(format!(
            "A Draft document has failed validation {} times (the maximum). \
             You cannot fix it further. Respond with:\n\
             [{{\"action\": \"need_help\", \"reason\": \"Document failed validation {} times, needs human review\"}}]",
            max_validation_attempts, max_validation_attempts
        ));
    }

    // Case 2: Check if a Draft exists with failed validation → re-generate with accumulated failures
    // (Only relevant when validator is enabled — no validation failures exist when disabled)
    if let Some(regen) = stores
        .validator
        .is_some()
        .then(|| generation::find_draft_needing_regeneration(stores, max_validation_attempts))
        .flatten()
    {
        agent_log.info(&format!(
            "re-generation needed: {} {} (attempt {}/{})",
            regen.collection,
            regen.target_id,
            regen.attempt_count + 1,
            max_validation_attempts
        ));
        let learnings = query_learnings_for_level(stores, regen.level);
        let prompt = match regen.level {
            GenerationLevel::Plan => build_plan_prompt(goal, &learnings, &regen.accumulated_failures, guidance_section),
            GenerationLevel::Spec => {
                let plan = generation::find_active_plan(stores)?;
                build_spec_prompt(&plan, &learnings, &[], &regen.accumulated_failures, guidance_section)
            }
            GenerationLevel::Phase => {
                let plan = generation::find_active_plan(stores)?;
                let specs = generation::find_active_specs_for_plan(stores, &plan.id);
                let spec = specs.into_iter().next()?;
                build_phase_prompt(&spec, &learnings, &regen.accumulated_failures, guidance_section)
            }
            // Works don't go through Draft→Active validation cycle
            GenerationLevel::Work => return None,
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

    // Fix #8: Check if a Draft document exists that needs validation or activation.
    // When determine_generation_level() returned None (a Draft exists but has no failed
    // validations yet), the Coordinator needs to validate it (if validator enabled)
    // or activate it directly (if validator disabled).
    if let Some(draft_info) = find_pending_draft_for_validation(stores) {
        if stores.validator.is_some() {
            // Validator enabled — ask coordinator to validate
            agent_log.info(&format!("Draft {} '{}' needs validation", draft_info.0, draft_info.1));
            return Some(format!(
                "A {} is in Draft status and needs validation before proceeding.\n\
                 Use ValidateDocument to validate it.\n\
                 Draft ID: {}\nTitle: {}",
                draft_info.0, draft_info.1, draft_info.2
            ));
        } else {
            // Validator disabled — tell coordinator to activate directly
            agent_log.info(&format!(
                "Draft {} '{}' — validator disabled, activate directly",
                draft_info.0, draft_info.1
            ));
            return Some(format!(
                "A {} is in Draft status. Validation is disabled — activate it directly.\n\
                 Use Transition to move it from Draft to Active.\n\
                 ID: {}\nTitle: {}",
                draft_info.0, draft_info.1, draft_info.2
            ));
        }
    }

    // Case 4: Check if coverage evaluation is needed (all children exist but not yet evaluated)
    if stores.evaluator.is_some()
        && stores.config.strategy.coverage_enabled
        && let Some(check) = generation::find_pending_coverage_check(stores)
    {
        agent_log.info(&format!("coverage evaluation needed: {}", check.description));
        return Some(format!(
            "Children exist but coverage has not been evaluated.\n\
             Use EvaluateCoverage to check that children fully cover the parent's requirements.\n\
             Parent collection: {}\nParent ID: {}\n\n{}",
            check.parent_collection, check.parent_id, check.description
        ));
    }

    None
}

/// Fix #2: Resolve batch:N dependency references in a CreateWork action.
/// Returns Some(modified_action) if batch deps were resolved, None if no changes needed.
fn resolve_batch_dependencies(
    action: &AgentAction,
    batch_created_ids: &[String],
    agent_log: &AgentLogger,
) -> Option<AgentAction> {
    if let AgentAction::CreateWork {
        phase_id,
        title,
        description,
        resource_tags,
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
                    agent_log.warn(&format!(
                        "batch:{} out of range (only {} items created so far)",
                        idx,
                        batch_created_ids.len()
                    ));
                }
                dep.clone()
            })
            .collect();

        Some(AgentAction::CreateWork {
            phase_id: phase_id.clone(),
            title: title.clone(),
            description: description.clone(),
            resource_tags: resource_tags.clone(),
            acceptance_criteria: acceptance_criteria.clone(),
            dependencies: resolved_deps,
        })
    } else {
        None
    }
}

/// Fix #6: Prune dependencies between batch-created works whose resource_tags don't overlap.
/// Safety net for when the LLM creates linear chains between independent works.
fn prune_independent_deps(stores: &Stores, batch_created_ids: &[String], agent_log: &AgentLogger) {
    let batch_set: HashSet<&str> = batch_created_ids.iter().map(|s| s.as_str()).collect();

    // Build resource_tags lookup for batch works
    let tag_map: HashMap<String, HashSet<String>> = {
        let Ok(works) = stores.read_works() else {
            log::error!("works lock poisoned");
            return;
        };
        batch_created_ids
            .iter()
            .filter_map(|id| {
                works
                    .get(id)
                    .map(|w| (id.clone(), w.resource_tags.iter().cloned().collect()))
            })
            .collect()
    };

    // Prune deps between batch works with disjoint resource_tags
    let Ok(mut works) = stores.write_works() else {
        log::error!("works lock poisoned");
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
                agent_log.info(&format!("pruned {} independent dep(s) from '{}'", pruned, wi.title));
            }
        }
    }
}

/// Fix #12: Mark the Phase domain record as Complete before calling coord_state.complete_phase().
/// This ensures the Phase record status and CoordinatorState are updated together.
fn mark_phase_record_complete(stores: &Stores, coord_state: &CoordinatorState, agent_log: &AgentLogger) {
    if let Some(ref phase_id) = coord_state.current_phase_id {
        // L1: Clone-then-drop-then-persist to avoid deadlock and ensure TaskStore persistence
        let phase_to_persist = {
            let Ok(mut phases) = stores.write_phases() else {
                log::error!("phases lock poisoned");
                return;
            };
            if let Some(phase) = phases.get_mut(phase_id) {
                phase.status = HierarchyStatus::Complete;
                phase.updated_at = crate::id::now_millis();
                agent_log.info(&format!("Phase {} marked Complete (record status updated)", phase_id));
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
            agent_log.warn(&format!("Failed to persist Phase complete status: {}", e));
        }
    }
}

/// L2: Find a pending Draft document scoped to the current active hierarchy chain.
/// Returns (level_name, id, title) if found.
fn find_pending_draft_for_validation(stores: &Stores) -> Option<(&'static str, String, String)> {
    // Find active Plan (if any)
    let active_plan_id = {
        let plans = stores.read_plans().ok()?;
        plans
            .values()
            .find(|p| p.status == HierarchyStatus::Active)
            .map(|p| p.id.clone())
    };

    // Check for Draft Plan (only if no Active Plan exists)
    if active_plan_id.is_none() {
        let plans = stores.read_plans().ok()?;
        if let Some(draft) = plans.values().find(|p| p.status == HierarchyStatus::Draft) {
            return Some(("Plan", draft.id.clone(), draft.title.clone()));
        }
        return None;
    }

    // Find active Spec for the active Plan
    let active_spec_id = {
        let specs = stores.read_specs().ok()?;
        specs
            .values()
            .find(|s| s.status == HierarchyStatus::Active && Some(&s.plan_id) == active_plan_id.as_ref())
            .map(|s| s.id.clone())
    };

    // Check for Draft Spec (only children of the active Plan)
    if active_spec_id.is_none() {
        let specs = stores.read_specs().ok()?;
        if let Some(draft) = specs
            .values()
            .find(|s| s.status == HierarchyStatus::Draft && Some(&s.plan_id) == active_plan_id.as_ref())
        {
            return Some(("Spec", draft.id.clone(), draft.title.clone()));
        }
        return None;
    }

    // Check for Draft Phase (only children of the active Spec)
    let phases = stores.read_phases().ok()?;
    if let Some(draft) = phases
        .values()
        .find(|p| p.status == HierarchyStatus::Draft && Some(&p.spec_id) == active_spec_id.as_ref())
    {
        return Some(("Phase", draft.id.clone(), draft.title.clone()));
    }

    None
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
        log::error!("coordinator_states lock poisoned");
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
    log: &AgentLogger,
) -> Option<IterationOutcome> {
    let old_state = coord_state.fsm_state;
    log::info!(
        "[coordinator] {} -> {} (goal: {})",
        old_state,
        new_state,
        coord_state.goal_id
    );
    // Handle ActivatePhase: complete previous phase, find and set the next phase
    if new_state == CoordinatorFsmState::ActivatePhase {
        // If transitioning from PhaseGate, complete the previous phase
        if coord_state.current_phase_id.is_some() {
            mark_phase_record_complete(stores, coord_state, log);
            coord_state.complete_phase();
        }
        let next_phase = find_next_phase_to_activate(stores, coord_state);
        if let Some((phase_id, phase_title)) = next_phase {
            log.info(&format!("activating phase: {} ({})", phase_title, phase_id));
            coord_state.current_phase_id = Some(phase_id);
            coord_state.phase_activated_at = Some(crate::id::now_millis());
            coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
            coord_state.updated_at = crate::id::now_millis();
        } else {
            coord_state.transition_to(CoordinatorFsmState::GoalComplete);
        }
    } else if new_state == CoordinatorFsmState::PhaseGate {
        // Transition to PhaseGate but DON'T complete_phase() yet —
        // keep current_phase_id so build_phase_status can show context to the LLM.
        // complete_phase() is called on the NEXT transition out of PhaseGate.
        coord_state.transition_to(CoordinatorFsmState::PhaseGate);
    } else if new_state == CoordinatorFsmState::GoalComplete {
        // Complete current phase if transitioning from PhaseGate
        if coord_state.current_phase_id.is_some() {
            mark_phase_record_complete(stores, coord_state, log);
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
    log: &AgentLogger,
) {
    log::debug!(
        "sweep_integrated_to_done(fsm={:?}, phase={:?})",
        coord_state.fsm_state,
        coord_state.current_phase_id,
    );
    if coord_state.fsm_state != CoordinatorFsmState::Executing {
        return;
    }
    let phase_id = match &coord_state.current_phase_id {
        Some(id) => id,
        None => return,
    };
    let integrated_ids: Vec<String> = {
        let Ok(works) = stores.read_works() else {
            log::error!("works lock poisoned");
            return;
        };
        works
            .values()
            .filter(|w| w.phase_id == *phase_id && w.status == WorkStatus::Integrated)
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
            log.error(&format!("failed to transition WI {} Integrated->Done: {}", wi_id, msg));
            continue;
        } else {
            log.info(&format!("Work {} transitioned Integrated → Done", wi_id));
        }
    }
}

/// Check if the current phase requires validation tools but none are registered.
/// Returns a warning string for the coordinator prompt, or empty string if tools are available.
fn phase_missing_test_tool(stores: &Stores, coord_state: &CoordinatorState) -> String {
    let phase_id = match &coord_state.current_phase_id {
        Some(id) => id,
        None => return String::new(),
    };
    let phase = {
        let Ok(phases) = stores.read_phases() else {
            return String::new();
        };
        match phases.get(phase_id) {
            Some(p) => p.clone(),
            None => return String::new(),
        }
    };
    if phase.validation_commands.is_empty() {
        return String::new();
    }
    // Phase has validation_commands - check if a test tool exists
    let has_test_tool = stores
        .read_tool_runner()
        .ok()
        .is_some_and(|runner| runner.get_tool("test").is_some());
    if has_test_tool {
        return String::new();
    }
    // Surface the declared validation commands as a hint
    let cmds_list = phase
        .validation_commands
        .iter()
        .map(|c| format!("  - `{}`", c))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "**WARNING: This phase has validation-commands but no 'test' tool \
         is registered.** The declared validation commands for this phase are:\n\
         {}\n\
         You MUST use `register_tool` to register a test command based on \
         these commands BEFORE dispatching implementers. Extract the \
         executable from the commands above and register it. \
         Do NOT spawn researchers for tool discovery when validation \
         commands are already declared.\n\n",
        cmds_list
    )
}

/// Determine the FSM state footer — state-specific instructions for the LLM.
fn build_fsm_footer(
    stores: &Stores,
    coord_state: &CoordinatorState,
    goal: &str,
    config: &CoordinatorConfig,
    agent_log: &AgentLogger,
) -> String {
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
        CoordinatorFsmState::Planning => {
            // Use existing generation footer logic for Plan→Spec→Phase hierarchy
            if let Some(gen_footer) = build_generation_footer(
                stores,
                goal,
                config.max_validation_attempts,
                None,
                agent_log,
                Some(coord_state),
                Some(stores.config.strategy.max_decomposition_attempts),
                Some(stores.config.strategy.max_bubble_up_depth),
            ) {
                gen_footer
            } else {
                // All hierarchy levels exist — ready to transition to ActivatePhase
                "All planning artifacts (Plan, Spec, Phases) are created and active. \
                 Respond with: [{\"action\": \"done\", \"summary\": \"Planning complete, ready to activate first phase\"}]"
                    .to_string()
            }
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
                        let existing = generation::find_works_for_phase(stores, &phase.id);
                        if existing.is_empty() {
                            let prompt = build_work_prompt(&phase, &existing, &[], &[], None, None);
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
    let phases = stores.read_phases().ok()?;
    let mut ordered: Vec<_> = phases
        .values()
        .filter(|p| !coord_state.phases_completed.contains(&p.id))
        .filter(|p| !matches!(p.status, HierarchyStatus::Complete | HierarchyStatus::Abandoned))
        .collect();
    ordered.sort_by_key(|p| p.order);
    ordered.first().map(|p| (p.id.clone(), p.title.clone()))
}

/// Build a status summary for the current phase's works.
fn build_phase_status(stores: &Stores, coord_state: &CoordinatorState) -> String {
    let phase_id = match &coord_state.current_phase_id {
        Some(id) => id,
        None => return "No active phase.".to_string(),
    };

    let phase_title = {
        let Ok(phases) = stores.read_phases() else {
            return "phases lock poisoned".to_string();
        };
        phases.get(phase_id).map(|p| p.title.clone()).unwrap_or_default()
    };

    let Ok(works) = stores.read_works() else {
        return "works lock poisoned".to_string();
    };
    let phase_wis: Vec<_> = works.values().filter(|w| &w.phase_id == phase_id).collect();

    // Split into actionable vs terminal so the LLM can clearly distinguish
    // which works need attention and which are finished.
    let mut actionable = Vec::new();
    let mut terminal = Vec::new();
    for wi in &phase_wis {
        if matches!(wi.status, WorkStatus::Done | WorkStatus::Abandoned) {
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
                wi.id, wi.title, wi.status, attempt_note
            ));
            // Show dependency info inline
            if !wi.dependencies.is_empty() {
                let dep_status: Vec<String> = wi
                    .dependencies
                    .iter()
                    .map(|dep_id| {
                        let status = works
                            .get(dep_id)
                            .map(|d| format!("{}", d.status))
                            .unwrap_or_else(|| "unknown".to_string());
                        format!("{}={}", dep_id, status)
                    })
                    .collect();
                let all_met = wi
                    .dependencies
                    .iter()
                    .all(|dep_id| works.get(dep_id).map(|d| d.status == WorkStatus::Done).unwrap_or(false));
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
            summary.push_str(&format!("- [{}] {} ({})\n", wi.id, wi.title, wi.status));
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
    log::debug!(
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
            // Transition when: Works for current phase exist and are Ready
            if let Some(ref phase_id) = coord_state.current_phase_id {
                let wis = generation::find_works_for_phase(stores, phase_id);
                if !wis.is_empty() {
                    return Some(CoordinatorFsmState::Executing);
                }
            }
            // If we just set current_phase_id, wait for Works to be created
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
                let wis = generation::find_works_for_phase(stores, phase_id);
                if wis.is_empty() {
                    // Phase with 0 Works — transition to PhaseGate so the gate
                    // can decide whether to retry WI generation or advance.
                    // Design doc: "require at least 1 Done WI to consider a Phase complete"
                    return Some(CoordinatorFsmState::PhaseGate);
                }
                if wis
                    .iter()
                    .all(|w| matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
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

#[async_trait]
impl Agent for CoordinatorAgent {
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
        .filter(|s| s.work_id.as_deref() == Some(work_id) && s.status == AgentStatus::Failed && s.error_kind.is_some())
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
