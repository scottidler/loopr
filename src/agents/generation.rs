//! Document generation prompts for the Coordinator's Plan → Spec → Phase → Work pipeline.
//!
//! Each generation level has a prompt builder that assembles context-aware messages including:
//! - Current state (active parent records)
//! - User intent / goal
//! - Relevant learnings
//! - ALL accumulated validation failures (prevents oscillation between failure modes)
//!
//! The Coordinator uses these prompts in a generate → validate → iterate loop,
//! capped by `max_validation_attempts`.

use crate::daemon::context::Stores;
use crate::domain::coverage::{CoverageReport, CoverageVerdict};
use crate::domain::phase::Phase;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::spec::Spec;
use crate::domain::validation::ValidationReport;
use crate::domain::work::{Work, WorkStatus};
use taskstore::Filter;
use taskstore::FilterOp;
use taskstore::record::IndexValue;

/// Which level of the hierarchy to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationLevel {
    Plan,
    Spec,
    Phase,
    Work,
}

impl std::fmt::Display for GenerationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationLevel::Plan => write!(f, "Plan"),
            GenerationLevel::Spec => write!(f, "Spec"),
            GenerationLevel::Phase => write!(f, "Phase"),
            GenerationLevel::Work => write!(f, "Work"),
        }
    }
}

/// Context assembled for a generation prompt.
#[derive(Debug)]
pub struct GenerationPrompt {
    /// The generation level.
    pub level: GenerationLevel,
    /// The assembled user message to send to the LLM.
    pub user_message: String,
}

/// Build a Plan generation prompt.
///
/// Input context:
/// - User-provided goal/objective
/// - Relevant learnings (global scope)
/// - Accumulated validation failures from previous attempts
pub fn build_plan_prompt(
    goal: &str,
    learnings: &[String],
    validation_failures: &[String],
    guidance_section: Option<&str>,
) -> GenerationPrompt {
    log::debug!(
        "build_plan_prompt(goal_len={}, learnings={}, failures={})",
        goal.len(),
        learnings.len(),
        validation_failures.len()
    );
    let mut msg = String::with_capacity(2048);

    msg.push_str("## Task: Generate a Plan\n\n");

    msg.push_str("### Current State\n");
    msg.push_str("No active Plan exists. Create one to address the user's goal.\n\n");

    msg.push_str("### User Intent\n");
    msg.push_str(goal);
    msg.push_str("\n\n");

    if let Some(guidance) = guidance_section {
        msg.push_str(guidance);
    }

    if !learnings.is_empty() {
        msg.push_str("### Relevant Learnings\n");
        for l in learnings {
            msg.push_str("- ");
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push('\n');
    }

    if !validation_failures.is_empty() {
        msg.push_str("### Previous Validation Failures (ALL accumulated — fix ALL of these)\n");
        for (i, failure) in validation_failures.iter().enumerate() {
            msg.push_str(&format!("{}. {}\n", i + 1, failure));
        }
        msg.push('\n');
    }

    msg.push_str("### Instructions\n");
    msg.push_str(&crate::prompts::store().generation_plan);

    GenerationPrompt {
        level: GenerationLevel::Plan,
        user_message: msg,
    }
}

/// Build a Spec generation prompt.
///
/// Input context:
/// - Active Plan (title, ID, description, acceptance criteria)
/// - Relevant learnings (scoped to Plan + Global)
/// - Codebase findings (researcher results, if available)
/// - Accumulated validation failures
pub fn build_spec_prompt(
    plan: &Plan,
    learnings: &[String],
    findings: &[String],
    validation_failures: &[String],
    guidance_section: Option<&str>,
) -> GenerationPrompt {
    log::debug!(
        "build_spec_prompt(plan_id={}, learnings={}, findings={}, failures={})",
        plan.id,
        learnings.len(),
        findings.len(),
        validation_failures.len()
    );
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate a Spec\n\n");

    msg.push_str("### Active Plan\n");
    msg.push_str(&format!("- **ID:** {}\n", plan.id));
    msg.push_str(&format!("- **Title:** {}\n", plan.title));
    msg.push_str(&format!("- **Description:** {}\n", plan.description));
    msg.push_str(&format!("- **Acceptance Criteria:** {}\n\n", plan.acceptance_criteria));

    if let Some(guidance) = guidance_section {
        msg.push_str(guidance);
    }

    if !learnings.is_empty() {
        msg.push_str("### Relevant Learnings\n");
        for l in learnings {
            msg.push_str("- ");
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push('\n');
    }

    if !findings.is_empty() {
        msg.push_str("### Codebase Findings\n");
        for f in findings {
            msg.push_str("- ");
            msg.push_str(f);
            msg.push('\n');
        }
        msg.push('\n');
    }

    if !validation_failures.is_empty() {
        msg.push_str("### Previous Validation Failures (ALL accumulated — fix ALL of these)\n");
        for (i, failure) in validation_failures.iter().enumerate() {
            msg.push_str(&format!("{}. {}\n", i + 1, failure));
        }
        msg.push('\n');
    }

    msg.push_str("### Instructions\n");
    msg.push_str(&crate::prompts::store().generation_spec);

    GenerationPrompt {
        level: GenerationLevel::Spec,
        user_message: msg,
    }
}

/// Build a Phase generation prompt.
///
/// Input context:
/// - Active Spec (title, ID, plan reference, description)
/// - Relevant learnings (scoped to Spec + Plan + Global)
/// - Accumulated validation failures
pub fn build_phase_prompt(
    spec: &Spec,
    learnings: &[String],
    validation_failures: &[String],
    guidance_section: Option<&str>,
) -> GenerationPrompt {
    log::debug!(
        "build_phase_prompt(spec_id={}, learnings={}, failures={})",
        spec.id,
        learnings.len(),
        validation_failures.len()
    );
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate Implementation Phases\n\n");

    msg.push_str("### Active Spec\n");
    msg.push_str(&format!("- **ID:** {}\n", spec.id));
    msg.push_str(&format!("- **Plan ID:** {}\n", spec.parent_id));
    msg.push_str(&format!("- **Title:** {}\n", spec.title));
    msg.push_str(&format!("- **Description:** {}\n\n", spec.description));

    if let Some(guidance) = guidance_section {
        msg.push_str(guidance);
    }

    if !learnings.is_empty() {
        msg.push_str("### Relevant Learnings\n");
        for l in learnings {
            msg.push_str("- ");
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push('\n');
    }

    if !validation_failures.is_empty() {
        msg.push_str("### Previous Validation Failures (ALL accumulated — fix ALL of these)\n");
        for (i, failure) in validation_failures.iter().enumerate() {
            msg.push_str(&format!("{}. {}\n", i + 1, failure));
        }
        msg.push('\n');
    }

    msg.push_str("### Instructions\n");
    msg.push_str(&crate::prompts::store().generation_phase);

    GenerationPrompt {
        level: GenerationLevel::Phase,
        user_message: msg,
    }
}

/// Build a Work generation prompt.
///
/// Input context:
/// - Active Phase (title, order, ID, spec reference, description)
/// - Existing Works in this Phase (to avoid duplicates)
/// - Relevant learnings (scoped to Phase + Spec + Plan + Global)
/// - Codebase context (researcher findings about affected modules)
pub fn build_work_prompt(
    phase: &Phase,
    existing_works: &[Work],
    learnings: &[String],
    findings: &[String],
    plan_description: Option<&str>,
    guidance_section: Option<&str>,
) -> GenerationPrompt {
    log::debug!(
        "build_work_prompt(phase_id={}, existing_works={}, learnings={}, findings={})",
        phase.id,
        existing_works.len(),
        learnings.len(),
        findings.len()
    );
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate Works\n\n");

    // Include the original plan description so the LLM can see the full
    // user-agreed structure when generating work items.
    if let Some(plan_desc) = plan_description.filter(|d| !d.is_empty()) {
        msg.push_str("### Original Plan (user-agreed - be faithful to this structure)\n");
        msg.push_str(plan_desc);
        msg.push_str("\n\n");
    }

    msg.push_str("### Active Phase\n");
    msg.push_str(&format!("- **ID:** {}\n", phase.id));
    msg.push_str(&format!("- **Spec ID:** {}\n", phase.parent_id));
    msg.push_str(&format!("- **Title:** {}\n", phase.title));
    msg.push_str(&format!("- **Order:** {}\n", phase.order));
    msg.push_str(&format!("- **Description:** {}\n\n", phase.description));

    if let Some(guidance) = guidance_section {
        msg.push_str(guidance);
    }

    msg.push_str("### Existing Works in this Phase\n");
    if existing_works.is_empty() {
        msg.push_str("None yet.\n\n");
    } else {
        for wi in existing_works {
            let deps = if wi.dependencies.is_empty() {
                "no deps".to_string()
            } else {
                format!("deps: {}", wi.dependencies.join(", "))
            };
            msg.push_str(&format!(
                "- ID: {} | Title: \"{}\" | Status: {} | {} — {}\n",
                wi.id,
                wi.title,
                wi.status(),
                deps,
                wi.description
            ));
        }
        msg.push_str("\nWhen declaring `dependencies`, use the exact IDs above.\n\n");
    }

    if !learnings.is_empty() {
        msg.push_str("### Relevant Learnings\n");
        for l in learnings {
            msg.push_str("- ");
            msg.push_str(l);
            msg.push('\n');
        }
        msg.push('\n');
    }

    if !findings.is_empty() {
        msg.push_str("### Codebase Context\n");
        for f in findings {
            msg.push_str("- ");
            msg.push_str(f);
            msg.push('\n');
        }
        msg.push('\n');
    }

    msg.push_str("### Instructions\n");
    msg.push_str(&crate::prompts::store().generation_work);

    GenerationPrompt {
        level: GenerationLevel::Work,
        user_message: msg,
    }
}

/// Determine which generation level the Coordinator should focus on, based on current state.
///
/// Returns `None` if no generation is needed (all levels have Active records or Works exist).
///
/// Decision tree:
/// 1. No active Plan AND no Draft Plan? → Plan
/// 2. Active Plan, no active Specs AND no Draft Specs? → Spec
/// 3. Active Specs, no active Phases AND no Draft Phases? → Phase
/// 4. Active Phases, no Works for them? → Work
/// 5. Otherwise → None (generation not needed)
pub fn determine_generation_level(stores: &Stores) -> Option<GenerationLevel> {
    let plans = stores.read_plans().ok()?;

    // Check for active Plan
    let active_plan = plans.values().find(|p| p.status() == HierarchyStatus::Active);
    let draft_plan = plans.values().find(|p| p.status() == HierarchyStatus::Draft);

    if active_plan.is_none() && draft_plan.is_none() {
        return Some(GenerationLevel::Plan);
    }

    // If there's no active Plan (only Draft), don't advance — Coordinator should validate the Draft
    let active_plan = active_plan?;

    // Check for Specs under the active Plan
    let specs = stores.read_specs().ok()?;
    let plan_specs: Vec<_> = specs.values().filter(|s| s.parent_id == active_plan.id).collect();
    let has_active_spec = plan_specs.iter().any(|s| s.status() == HierarchyStatus::Active);
    let has_draft_spec = plan_specs.iter().any(|s| s.status() == HierarchyStatus::Draft);

    if !has_active_spec && !has_draft_spec {
        return Some(GenerationLevel::Spec);
    }

    if !has_active_spec {
        return None; // Draft Spec exists, wait for validation
    }

    // Check for Phases under active Specs
    let phases = stores.read_phases().ok()?;
    let active_spec_ids: Vec<_> = plan_specs
        .iter()
        .filter(|s| s.status() == HierarchyStatus::Active)
        .map(|s| s.id.as_str())
        .collect();

    let spec_phases: Vec<_> = phases
        .values()
        .filter(|p| active_spec_ids.contains(&p.parent_id.as_str()))
        .collect();
    let has_active_phase = spec_phases.iter().any(|p| p.status() == HierarchyStatus::Active);
    let has_draft_phase = spec_phases.iter().any(|p| p.status() == HierarchyStatus::Draft);

    if !has_active_phase && !has_draft_phase {
        return Some(GenerationLevel::Phase);
    }

    if !has_active_phase {
        return None; // Draft Phase exists, wait for validation
    }

    // Check for Works under active Phases
    let works = stores.read_works().ok()?;
    let active_phase_ids: Vec<_> = spec_phases
        .iter()
        .filter(|p| p.status() == HierarchyStatus::Active)
        .map(|p| p.id.as_str())
        .collect();

    let has_works = works.values().any(|w| active_phase_ids.contains(&w.parent_id.as_str()));

    if !has_works {
        return Some(GenerationLevel::Work);
    }

    None
}

/// Find the active Plan from stores. Returns None if no active Plan.
pub fn find_active_plan(stores: &Stores) -> Option<Plan> {
    let plans = stores.read_plans().ok()?;
    plans.values().find(|p| p.status() == HierarchyStatus::Active).cloned()
}

/// Find active Specs for a given Plan.
pub fn find_active_specs_for_plan(stores: &Stores, plan_id: &str) -> Vec<Spec> {
    let Ok(specs) = stores.read_specs() else { return vec![] };
    specs
        .values()
        .filter(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Active)
        .cloned()
        .collect()
}

/// Find active Phases for a given Spec.
pub fn find_active_phases_for_spec(stores: &Stores, spec_id: &str) -> Vec<Phase> {
    let Ok(phases) = stores.read_phases() else {
        return vec![];
    };
    let mut result: Vec<_> = phases
        .values()
        .filter(|p| p.parent_id == spec_id && p.status() == HierarchyStatus::Active)
        .cloned()
        .collect();
    result.sort_by_key(|p| p.order);
    result
}

/// Find existing Works for a given Phase.
pub fn find_works_for_phase(stores: &Stores, phase_id: &str) -> Vec<Work> {
    let Ok(works) = stores.read_works() else { return vec![] };
    works.values().filter(|w| w.parent_id == phase_id).cloned().collect()
}

/// Find the first active Phase that still needs Works.
pub fn find_phase_needing_works(stores: &Stores) -> Option<Phase> {
    let plans = stores.read_plans().ok()?;
    let active_plan = plans.values().find(|p| p.status() == HierarchyStatus::Active)?;
    let plan_id = active_plan.id.clone();
    drop(plans);

    let specs = stores.read_specs().ok()?;
    let active_spec_ids: Vec<String> = specs
        .values()
        .filter(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Active)
        .map(|s| s.id.clone())
        .collect();
    drop(specs);

    let phases = stores.read_phases().ok()?;
    let mut active_phases: Vec<_> = phases
        .values()
        .filter(|p| active_spec_ids.contains(&p.parent_id) && p.status() == HierarchyStatus::Active)
        .cloned()
        .collect();
    active_phases.sort_by_key(|p| p.order);
    drop(phases);

    let works = stores.read_works().ok()?;
    for phase in active_phases {
        let has_wi = works.values().any(|w| w.parent_id == phase.id);
        if !has_wi {
            return Some(phase);
        }
    }

    None
}

/// Check if all Works in a Phase are in a terminal state (Done or Abandoned).
/// This matches the FSM's check_fsm_transition() predicate exactly.
pub fn is_phase_complete(stores: &Stores, phase_id: &str) -> bool {
    let Ok(works) = stores.read_works() else { return false };
    let phase_wis: Vec<_> = works.values().filter(|w| w.parent_id == phase_id).collect();
    !phase_wis.is_empty()
        && phase_wis
            .iter()
            .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
}

/// Query failed validation reports for a specific document from TaskStore.
///
/// Returns all Fail-verdict reports for the given target, sorted by creation time.
/// These are used as accumulated failures in the re-generation prompt.
pub fn find_failed_validations(stores: &Stores, collection: &str, target_id: &str) -> Vec<ValidationReport> {
    let store = match &stores.store {
        Some(s) => s,
        None => return vec![],
    };
    let filters = vec![
        Filter {
            field: "target_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(target_id.to_string()),
        },
        Filter {
            field: "target_collection".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(collection.to_string()),
        },
        Filter {
            field: "verdict".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String("fail".to_string()),
        },
    ];
    let Ok(store_guard) = store.lock().map_err(|_| eyre::eyre!("taskstore lock poisoned")) else {
        return vec![];
    };
    let mut reports: Vec<ValidationReport> = store_guard.list(&filters).unwrap_or_default();
    reports.sort_by_key(|r| r.created_at);
    reports
}

/// Extract accumulated failure messages from validation reports.
///
/// Collects all issue messages from Fail reports, deduplicating identical messages.
pub fn collect_failure_messages(reports: &[ValidationReport]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut messages = Vec::new();
    for report in reports {
        // Include the summary as a top-level failure message
        if !report.summary.is_empty() && seen.insert(report.summary.clone()) {
            messages.push(report.summary.clone());
        }
        // Include individual issue messages
        for issue in &report.issues {
            if seen.insert(issue.message.clone()) {
                messages.push(issue.message.clone());
            }
        }
    }
    messages
}

/// Information about a Draft document that needs re-generation after failed validation.
#[derive(Debug)]
pub struct RegenerationInfo {
    /// The generation level (Plan, Spec, Phase).
    pub level: GenerationLevel,
    /// The collection name ("plans", "specs", "phases").
    pub collection: String,
    /// The target document ID.
    pub target_id: String,
    /// Accumulated failure messages from all validation attempts.
    pub accumulated_failures: Vec<String>,
    /// Number of validation attempts so far.
    pub attempt_count: usize,
}

/// Check if there's a Draft document that has failed validation and needs re-generation.
///
/// Returns `Some(RegenerationInfo)` if:
/// 1. A Draft document exists at some level
/// 2. It has at least one Fail validation report
/// 3. The number of attempts is less than `max_validation_attempts`
///
/// Returns `None` if no Draft needs re-generation, or if max attempts reached.
pub fn find_draft_needing_regeneration(stores: &Stores, max_validation_attempts: u32) -> Option<RegenerationInfo> {
    // Check for Draft Plan
    {
        let plans = stores.read_plans().ok()?;
        if let Some(draft_plan) = plans.values().find(|p| p.status() == HierarchyStatus::Draft) {
            let target_id = draft_plan.id.clone();
            drop(plans);
            let failed = find_failed_validations(stores, "plans", &target_id);
            if !failed.is_empty() && (failed.len() as u32) < max_validation_attempts {
                let accumulated_failures = collect_failure_messages(&failed);
                return Some(RegenerationInfo {
                    level: GenerationLevel::Plan,
                    collection: "plans".to_string(),
                    target_id,
                    accumulated_failures,
                    attempt_count: failed.len(),
                });
            }
            // If attempts >= max_validation_attempts, return None (Coordinator should NeedHelp)
            return None;
        }
    }

    // Check for Draft Specs under an active Plan
    {
        let plans = stores.read_plans().ok()?;
        let active_plan = plans.values().find(|p| p.status() == HierarchyStatus::Active);
        if let Some(plan) = active_plan {
            let plan_id = plan.id.clone();
            drop(plans);
            let specs = stores.read_specs().ok()?;
            if let Some(draft_spec) = specs
                .values()
                .find(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Draft)
            {
                let target_id = draft_spec.id.clone();
                drop(specs);
                let failed = find_failed_validations(stores, "specs", &target_id);
                if !failed.is_empty() && (failed.len() as u32) < max_validation_attempts {
                    let accumulated_failures = collect_failure_messages(&failed);
                    return Some(RegenerationInfo {
                        level: GenerationLevel::Spec,
                        collection: "specs".to_string(),
                        target_id,
                        accumulated_failures,
                        attempt_count: failed.len(),
                    });
                }
                return None;
            }
        }
    }

    // Check for Draft Phases under active Specs
    {
        let plans = stores.read_plans().ok()?;
        let active_plan = plans.values().find(|p| p.status() == HierarchyStatus::Active);
        if let Some(plan) = active_plan {
            let plan_id = plan.id.clone();
            drop(plans);

            let specs = stores.read_specs().ok()?;
            let active_spec_ids: Vec<String> = specs
                .values()
                .filter(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Active)
                .map(|s| s.id.clone())
                .collect();
            drop(specs);

            let phases = stores.read_phases().ok()?;
            if let Some(draft_phase) = phases
                .values()
                .find(|p| active_spec_ids.contains(&p.parent_id) && p.status() == HierarchyStatus::Draft)
            {
                let target_id = draft_phase.id.clone();
                drop(phases);
                let failed = find_failed_validations(stores, "phases", &target_id);
                if !failed.is_empty() && (failed.len() as u32) < max_validation_attempts {
                    let accumulated_failures = collect_failure_messages(&failed);
                    return Some(RegenerationInfo {
                        level: GenerationLevel::Phase,
                        collection: "phases".to_string(),
                        target_id,
                        accumulated_failures,
                        attempt_count: failed.len(),
                    });
                }
                return None;
            }
        }
    }

    None
}

/// Check if a Draft document has exceeded max_validation_attempts.
///
/// Returns true if a Draft exists at any level AND has Fail reports >= max_validation_attempts.
pub fn is_validation_cap_reached(stores: &Stores, max_validation_attempts: u32) -> bool {
    // Check Draft Plan
    {
        let Ok(plans) = stores.read_plans() else { return false };
        if let Some(draft_plan) = plans.values().find(|p| p.status() == HierarchyStatus::Draft) {
            let target_id = draft_plan.id.clone();
            drop(plans);
            let failed = find_failed_validations(stores, "plans", &target_id);
            if !failed.is_empty() && (failed.len() as u32) >= max_validation_attempts {
                return true;
            }
        }
    }

    // Check Draft Specs
    {
        let Ok(plans) = stores.read_plans() else { return false };
        if let Some(plan) = plans.values().find(|p| p.status() == HierarchyStatus::Active) {
            let plan_id = plan.id.clone();
            drop(plans);
            let Ok(specs) = stores.read_specs() else { return false };
            if let Some(spec) = specs
                .values()
                .find(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Draft)
            {
                let target_id = spec.id.clone();
                drop(specs);
                let failed = find_failed_validations(stores, "specs", &target_id);
                if !failed.is_empty() && (failed.len() as u32) >= max_validation_attempts {
                    return true;
                }
            }
        }
    }

    // Check Draft Phases
    {
        let Ok(plans) = stores.read_plans() else { return false };
        if let Some(plan) = plans.values().find(|p| p.status() == HierarchyStatus::Active) {
            let plan_id = plan.id.clone();
            drop(plans);
            let Ok(specs) = stores.read_specs() else { return false };
            let active_spec_ids: Vec<String> = specs
                .values()
                .filter(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Active)
                .map(|s| s.id.clone())
                .collect();
            drop(specs);
            let Ok(phases) = stores.read_phases() else { return false };
            if let Some(phase) = phases
                .values()
                .find(|p| active_spec_ids.contains(&p.parent_id) && p.status() == HierarchyStatus::Draft)
            {
                let target_id = phase.id.clone();
                drop(phases);
                let failed = find_failed_validations(stores, "phases", &target_id);
                if !failed.is_empty() && (failed.len() as u32) >= max_validation_attempts {
                    return true;
                }
            }
        }
    }

    false
}

/// Information about a parent that needs coverage evaluation.
pub struct CoverageCheckNeeded {
    /// The collection type of the parent ("plan", "spec", "phase")
    pub parent_collection: String,
    /// The parent record ID
    pub parent_id: String,
    /// Human-readable description for the LLM prompt
    pub description: String,
}

/// Information about an incomplete coverage result that needs re-decomposition.
pub struct IncompleteDecomposition {
    /// The collection type of the parent
    pub parent_collection: String,
    /// The parent record ID
    pub parent_id: String,
    /// Current attempt count for this parent
    pub attempt_count: u32,
    /// Gap descriptions from the coverage report
    pub gap_descriptions: Vec<String>,
}

/// Find the latest CoverageReport for a given parent, if any.
pub fn find_latest_coverage_report(
    stores: &Stores,
    parent_collection: &str,
    parent_id: &str,
) -> Option<CoverageReport> {
    let Ok(reports) = stores.read_coverage_reports() else {
        return None;
    };
    reports
        .values()
        .filter(|r| r.parent_collection == parent_collection && r.parent_id == parent_id)
        .max_by_key(|r| r.created_at)
        .cloned()
}

/// Check if any hierarchy level needs coverage evaluation.
///
/// Returns Some if all children at a level exist (active) but no Complete coverage report
/// exists for that parent. This tells the Coordinator to evaluate coverage before proceeding.
pub fn find_pending_coverage_check(stores: &Stores) -> Option<CoverageCheckNeeded> {
    // Check Plan -> Specs coverage
    let plan = find_active_plan(stores)?;
    let specs = find_active_specs_for_plan(stores, &plan.id);
    if !specs.is_empty() {
        match find_latest_coverage_report(stores, "plan", &plan.id) {
            None => {
                return Some(CoverageCheckNeeded {
                    parent_collection: "plan".to_string(),
                    parent_id: plan.id.clone(),
                    description: format!(
                        "Plan '{}' has {} Specs but no coverage evaluation",
                        plan.title,
                        specs.len()
                    ),
                });
            }
            Some(report) if report.verdict == CoverageVerdict::Incomplete => {
                // Coverage was checked and found incomplete - handled by find_incomplete_decomposition
            }
            Some(_) => {
                // Coverage is Complete at Plan->Specs level, check Spec->Phases
            }
        }
    } else {
        return None; // No specs yet, generation still needed
    }

    // Check Spec -> Phases coverage (for each active spec)
    for spec in &specs {
        let phases = find_active_phases_for_spec(stores, &spec.id);
        if phases.is_empty() {
            continue; // Phases not yet generated for this spec
        }
        match find_latest_coverage_report(stores, "spec", &spec.id) {
            None => {
                return Some(CoverageCheckNeeded {
                    parent_collection: "spec".to_string(),
                    parent_id: spec.id.clone(),
                    description: format!(
                        "Spec '{}' has {} Phases but no coverage evaluation",
                        spec.title,
                        phases.len()
                    ),
                });
            }
            Some(report) if report.verdict == CoverageVerdict::Incomplete => {}
            Some(_) => {} // Complete, continue
        }
    }

    None
}

/// Find a parent with Incomplete coverage that needs re-decomposition.
///
/// Returns Some if a coverage report with Incomplete verdict exists and the attempt count
/// is below max_decomposition_attempts. The caller should re-decompose the children.
pub fn find_incomplete_decomposition(
    stores: &Stores,
    coord_state: &crate::domain::coordinator_state::CoordinatorState,
    max_attempts: u32,
) -> Option<IncompleteDecomposition> {
    // Check Plan -> Specs
    if let Some(plan) = find_active_plan(stores)
        && let Some(report) = find_latest_coverage_report(stores, "plan", &plan.id)
        && report.verdict == CoverageVerdict::Incomplete
    {
        let attempts = coord_state.decomposition_attempts(&plan.id);
        if attempts < max_attempts {
            let gaps: Vec<String> = report
                .gaps
                .iter()
                .map(|g| format!("[{}] {}: {}", g.severity, g.parent_criterion, g.description))
                .collect();
            return Some(IncompleteDecomposition {
                parent_collection: "plan".to_string(),
                parent_id: plan.id.clone(),
                attempt_count: attempts,
                gap_descriptions: gaps,
            });
        }
    }

    // Check Spec -> Phases
    if let Some(plan) = find_active_plan(stores) {
        for spec in find_active_specs_for_plan(stores, &plan.id) {
            if let Some(report) = find_latest_coverage_report(stores, "spec", &spec.id)
                && report.verdict == CoverageVerdict::Incomplete
            {
                let attempts = coord_state.decomposition_attempts(&spec.id);
                if attempts < max_attempts {
                    let gaps: Vec<String> = report
                        .gaps
                        .iter()
                        .map(|g| format!("[{}] {}: {}", g.severity, g.parent_criterion, g.description))
                        .collect();
                    return Some(IncompleteDecomposition {
                        parent_collection: "spec".to_string(),
                        parent_id: spec.id.clone(),
                        attempt_count: attempts,
                        gap_descriptions: gaps,
                    });
                }
            }
        }
    }

    None
}

/// Check if decomposition attempts are exhausted for any parent (needs bubble-up).
pub fn is_decomposition_cap_reached(
    stores: &Stores,
    coord_state: &crate::domain::coordinator_state::CoordinatorState,
    max_attempts: u32,
) -> Option<(String, String)> {
    // Check Plan -> Specs
    if let Some(plan) = find_active_plan(stores)
        && let Some(report) = find_latest_coverage_report(stores, "plan", &plan.id)
        && report.verdict == CoverageVerdict::Incomplete
        && coord_state.decomposition_attempts(&plan.id) >= max_attempts
    {
        return Some(("plan".to_string(), plan.id.clone()));
    }

    // Check Spec -> Phases
    if let Some(plan) = find_active_plan(stores) {
        for spec in find_active_specs_for_plan(stores, &plan.id) {
            if let Some(report) = find_latest_coverage_report(stores, "spec", &spec.id)
                && report.verdict == CoverageVerdict::Incomplete
                && coord_state.decomposition_attempts(&spec.id) >= max_attempts
            {
                return Some(("spec".to_string(), spec.id.clone()));
            }
        }
    }

    None
}

/// Extract coverage gap descriptions for a parent from the latest coverage report.
/// Returns formatted gap strings suitable for inclusion in LLM prompts.
pub fn get_coverage_gaps(stores: &Stores, collection: &str, parent_id: &str) -> Vec<String> {
    find_latest_coverage_report(stores, collection, parent_id)
        .map(|report| {
            report
                .gaps
                .iter()
                .map(|g| format!("[{}] {}: {}", g.severity, g.parent_criterion, g.description))
                .collect()
        })
        .unwrap_or_default()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
