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
    msg.push_str(&format!("- **Plan ID:** {}\n", spec.plan_id));
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

    msg.push_str("### Active Phase\n");
    msg.push_str(&format!("- **ID:** {}\n", phase.id));
    msg.push_str(&format!("- **Spec ID:** {}\n", phase.spec_id));
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
                wi.id, wi.title, wi.status, deps, wi.description
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
    let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active);
    let draft_plan = plans.values().find(|p| p.status == HierarchyStatus::Draft);

    if active_plan.is_none() && draft_plan.is_none() {
        return Some(GenerationLevel::Plan);
    }

    // If there's no active Plan (only Draft), don't advance — Coordinator should validate the Draft
    let active_plan = active_plan?;

    // Check for Specs under the active Plan
    let specs = stores.read_specs().ok()?;
    let plan_specs: Vec<_> = specs.values().filter(|s| s.plan_id == active_plan.id).collect();
    let has_active_spec = plan_specs.iter().any(|s| s.status == HierarchyStatus::Active);
    let has_draft_spec = plan_specs.iter().any(|s| s.status == HierarchyStatus::Draft);

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
        .filter(|s| s.status == HierarchyStatus::Active)
        .map(|s| s.id.as_str())
        .collect();

    let spec_phases: Vec<_> = phases
        .values()
        .filter(|p| active_spec_ids.contains(&p.spec_id.as_str()))
        .collect();
    let has_active_phase = spec_phases.iter().any(|p| p.status == HierarchyStatus::Active);
    let has_draft_phase = spec_phases.iter().any(|p| p.status == HierarchyStatus::Draft);

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
        .filter(|p| p.status == HierarchyStatus::Active)
        .map(|p| p.id.as_str())
        .collect();

    let has_works = works.values().any(|w| active_phase_ids.contains(&w.phase_id.as_str()));

    if !has_works {
        return Some(GenerationLevel::Work);
    }

    None
}

/// Find the active Plan from stores. Returns None if no active Plan.
pub fn find_active_plan(stores: &Stores) -> Option<Plan> {
    let plans = stores.read_plans().ok()?;
    plans.values().find(|p| p.status == HierarchyStatus::Active).cloned()
}

/// Find active Specs for a given Plan.
pub fn find_active_specs_for_plan(stores: &Stores, plan_id: &str) -> Vec<Spec> {
    let Ok(specs) = stores.read_specs() else { return vec![] };
    specs
        .values()
        .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
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
        .filter(|p| p.spec_id == spec_id && p.status == HierarchyStatus::Active)
        .cloned()
        .collect();
    result.sort_by_key(|p| p.order);
    result
}

/// Find existing Works for a given Phase.
pub fn find_works_for_phase(stores: &Stores, phase_id: &str) -> Vec<Work> {
    let Ok(works) = stores.read_works() else { return vec![] };
    works.values().filter(|w| w.phase_id == phase_id).cloned().collect()
}

/// Find the first active Phase that still needs Works.
pub fn find_phase_needing_works(stores: &Stores) -> Option<Phase> {
    let plans = stores.read_plans().ok()?;
    let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active)?;
    let plan_id = active_plan.id.clone();
    drop(plans);

    let specs = stores.read_specs().ok()?;
    let active_spec_ids: Vec<String> = specs
        .values()
        .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
        .map(|s| s.id.clone())
        .collect();
    drop(specs);

    let phases = stores.read_phases().ok()?;
    let mut active_phases: Vec<_> = phases
        .values()
        .filter(|p| active_spec_ids.contains(&p.spec_id) && p.status == HierarchyStatus::Active)
        .cloned()
        .collect();
    active_phases.sort_by_key(|p| p.order);
    drop(phases);

    let works = stores.read_works().ok()?;
    for phase in active_phases {
        let has_wi = works.values().any(|w| w.phase_id == phase.id);
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
    let phase_wis: Vec<_> = works.values().filter(|w| w.phase_id == phase_id).collect();
    !phase_wis.is_empty()
        && phase_wis
            .iter()
            .all(|w| matches!(w.status, WorkStatus::Done | WorkStatus::Abandoned))
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
        if let Some(draft_plan) = plans.values().find(|p| p.status == HierarchyStatus::Draft) {
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
        let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active);
        if let Some(plan) = active_plan {
            let plan_id = plan.id.clone();
            drop(plans);
            let specs = stores.read_specs().ok()?;
            if let Some(draft_spec) = specs
                .values()
                .find(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Draft)
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
        let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active);
        if let Some(plan) = active_plan {
            let plan_id = plan.id.clone();
            drop(plans);

            let specs = stores.read_specs().ok()?;
            let active_spec_ids: Vec<String> = specs
                .values()
                .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
                .map(|s| s.id.clone())
                .collect();
            drop(specs);

            let phases = stores.read_phases().ok()?;
            if let Some(draft_phase) = phases
                .values()
                .find(|p| active_spec_ids.contains(&p.spec_id) && p.status == HierarchyStatus::Draft)
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
        if let Some(draft_plan) = plans.values().find(|p| p.status == HierarchyStatus::Draft) {
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
        if let Some(plan) = plans.values().find(|p| p.status == HierarchyStatus::Active) {
            let plan_id = plan.id.clone();
            drop(plans);
            let Ok(specs) = stores.read_specs() else { return false };
            if let Some(spec) = specs
                .values()
                .find(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Draft)
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
        if let Some(plan) = plans.values().find(|p| p.status == HierarchyStatus::Active) {
            let plan_id = plan.id.clone();
            drop(plans);
            let Ok(specs) = stores.read_specs() else { return false };
            let active_spec_ids: Vec<String> = specs
                .values()
                .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
                .map(|s| s.id.clone())
                .collect();
            drop(specs);
            let Ok(phases) = stores.read_phases() else { return false };
            if let Some(phase) = phases
                .values()
                .find(|p| active_spec_ids.contains(&p.spec_id) && p.status == HierarchyStatus::Draft)
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::daemon::context::Stores;
    use crate::test_util::TestDir;
    use std::sync::{Arc, Mutex as StdMutex};
    use taskstore::Store;

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

    fn init() {
        crate::prompts::init_defaults();
    }

    // --- GenerationLevel tests ---

    #[test]
    fn test_generation_level_display() {
        assert_eq!(GenerationLevel::Plan.to_string(), "Plan");
        assert_eq!(GenerationLevel::Spec.to_string(), "Spec");
        assert_eq!(GenerationLevel::Phase.to_string(), "Phase");
        assert_eq!(GenerationLevel::Work.to_string(), "Work");
    }

    // --- Plan prompt tests ---

    #[test]
    fn test_plan_prompt_includes_goal() {
        init();
        let prompt = build_plan_prompt("Build a REST API", &[], &[], None);
        assert!(prompt.user_message.contains("Build a REST API"));
        assert_eq!(prompt.level, GenerationLevel::Plan);
    }

    #[test]
    fn test_plan_prompt_includes_learnings() {
        init();
        let learnings = vec!["Use async handlers".to_string(), "Prefer JSON responses".to_string()];
        let prompt = build_plan_prompt("Build API", &learnings, &[], None);
        assert!(prompt.user_message.contains("Use async handlers"));
        assert!(prompt.user_message.contains("Prefer JSON responses"));
        assert!(prompt.user_message.contains("Relevant Learnings"));
    }

    #[test]
    fn test_plan_prompt_includes_accumulated_failures() {
        init();
        let failures = vec!["Missing acceptance criteria".to_string(), "Title too vague".to_string()];
        let prompt = build_plan_prompt("Build API", &[], &failures, None);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("1. Missing acceptance criteria"));
        assert!(prompt.user_message.contains("2. Title too vague"));
        assert!(prompt.user_message.contains("fix ALL of these"));
    }

    #[test]
    fn test_plan_prompt_no_optional_sections_when_empty() {
        init();
        let prompt = build_plan_prompt("Build API", &[], &[], None);
        assert!(!prompt.user_message.contains("Relevant Learnings"));
        assert!(!prompt.user_message.contains("Validation Failures"));
    }

    #[test]
    fn test_plan_prompt_instructions_present() {
        init();
        let prompt = build_plan_prompt("Build API", &[], &[], None);
        assert!(prompt.user_message.contains("create_plan"));
        assert!(prompt.user_message.contains("acceptance criteria"));
    }

    // --- Spec prompt tests ---

    #[test]
    fn test_spec_prompt_includes_plan_context() {
        init();
        let plan = Plan::new("Auth Plan".into(), "Implement auth".into(), "Tests pass".into());
        let prompt = build_spec_prompt(&plan, &[], &[], &[], None);
        assert!(prompt.user_message.contains(&plan.id));
        assert!(prompt.user_message.contains("Auth Plan"));
        assert!(prompt.user_message.contains("Implement auth"));
        assert!(prompt.user_message.contains("Tests pass"));
        assert_eq!(prompt.level, GenerationLevel::Spec);
    }

    #[test]
    fn test_spec_prompt_includes_findings() {
        init();
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let findings = vec!["src/auth.rs has existing login logic".to_string()];
        let prompt = build_spec_prompt(&plan, &[], &findings, &[], None);
        assert!(prompt.user_message.contains("Codebase Findings"));
        assert!(prompt.user_message.contains("src/auth.rs has existing login logic"));
    }

    #[test]
    fn test_spec_prompt_includes_accumulated_failures() {
        init();
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let failures = vec!["Missing testability strategy".to_string()];
        let prompt = build_spec_prompt(&plan, &[], &[], &failures, None);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("Missing testability strategy"));
    }

    #[test]
    fn test_spec_prompt_instructions_reference_plan_id() {
        init();
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let prompt = build_spec_prompt(&plan, &[], &[], &[], None);
        assert!(prompt.user_message.contains("create_spec"));
        assert!(prompt.user_message.contains("plan_id"));
    }

    // --- Phase prompt tests ---

    #[test]
    fn test_phase_prompt_includes_spec_context() {
        init();
        let spec = Spec::new("plan-1".into(), "JWT Auth".into(), "Implement JWT".into());
        let prompt = build_phase_prompt(&spec, &[], &[], None);
        assert!(prompt.user_message.contains(&spec.id));
        assert!(prompt.user_message.contains("plan-1"));
        assert!(prompt.user_message.contains("JWT Auth"));
        assert!(prompt.user_message.contains("Implement JWT"));
        assert_eq!(prompt.level, GenerationLevel::Phase);
    }

    #[test]
    fn test_phase_prompt_includes_accumulated_failures() {
        init();
        let spec = Spec::new("plan-1".into(), "Spec".into(), "desc".into());
        let failures = vec![
            "Phase 2 depends on Phase 1 but order is wrong".to_string(),
            "Missing deliverables in Phase 3".to_string(),
        ];
        let prompt = build_phase_prompt(&spec, &[], &failures, None);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("Phase 2 depends on Phase 1"));
        assert!(prompt.user_message.contains("Missing deliverables"));
    }

    #[test]
    fn test_phase_prompt_instructions() {
        init();
        let spec = Spec::new("plan-1".into(), "Spec".into(), "desc".into());
        let prompt = build_phase_prompt(&spec, &[], &[], None);
        assert!(prompt.user_message.contains("create_phase"));
        assert!(prompt.user_message.contains("spec_id"));
        assert!(prompt.user_message.contains("order"));
    }

    // --- Work prompt tests ---

    #[test]
    fn test_work_prompt_includes_phase_context() {
        init();
        let phase = Phase::new("spec-1".into(), "Foundation".into(), "Set up base".into(), 1);
        let prompt = build_work_prompt(&phase, &[], &[], &[], None);
        assert!(prompt.user_message.contains(&phase.id));
        assert!(prompt.user_message.contains("spec-1"));
        assert!(prompt.user_message.contains("Foundation"));
        assert!(prompt.user_message.contains("Order:** 1"));
        assert_eq!(prompt.level, GenerationLevel::Work);
    }

    #[test]
    fn test_work_prompt_includes_existing_works() {
        init();
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let wi = Work::new(phase.id.clone(), "Add login".into(), "Login endpoint".into());
        let prompt = build_work_prompt(&phase, &[wi], &[], &[], None);
        assert!(prompt.user_message.contains("Add login"));
        assert!(prompt.user_message.contains("Login endpoint"));
        assert!(!prompt.user_message.contains("None yet"));
    }

    #[test]
    fn test_work_prompt_shows_none_when_no_existing() {
        init();
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let prompt = build_work_prompt(&phase, &[], &[], &[], None);
        assert!(prompt.user_message.contains("None yet"));
    }

    #[test]
    fn test_work_prompt_includes_dependency_instructions() {
        init();
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let prompt = build_work_prompt(&phase, &[], &[], &[], None);
        // Prompt should include batch dependency instructions
        assert!(
            prompt.user_message.contains("batch:0"),
            "prompt should explain batch dependency syntax"
        );
        assert!(
            prompt.user_message.contains("dependencies"),
            "prompt should mention dependencies"
        );
    }

    #[test]
    fn test_work_prompt_shows_dep_info_for_existing() {
        init();
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let mut wi = Work::new(phase.id.clone(), "WI 1".into(), "desc".into());
        wi.dependencies = vec!["dep-1".to_string()];
        let prompt = build_work_prompt(&phase, &[wi], &[], &[], None);
        assert!(prompt.user_message.contains("deps: dep-1"));
        assert!(prompt.user_message.contains("use the exact IDs above"));
    }

    #[test]
    fn test_work_prompt_includes_findings() {
        init();
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let findings = vec!["src/auth/ directory has 5 modules".to_string()];
        let prompt = build_work_prompt(&phase, &[], &[], &findings, None);
        assert!(prompt.user_message.contains("Codebase Context"));
        assert!(prompt.user_message.contains("src/auth/ directory has 5 modules"));
    }

    // --- determine_generation_level tests ---

    #[test]
    fn test_determine_level_plan_when_empty() {
        let dir = TestDir::new("loopr-gen-empty");
        let stores = test_stores(&dir);
        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Plan));
    }

    #[test]
    fn test_determine_level_none_when_draft_plan_exists() {
        let dir = TestDir::new("loopr-gen-draftplan");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        // Draft plan exists — Coordinator should validate it, not generate a new one
        assert_eq!(determine_generation_level(&stores), None);
    }

    #[test]
    fn test_determine_level_spec_when_active_plan_no_specs() {
        let dir = TestDir::new("loopr-gen-needspec");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Spec));
    }

    #[test]
    fn test_determine_level_none_when_draft_spec_exists() {
        let dir = TestDir::new("loopr-gen-draftspec");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        assert_eq!(determine_generation_level(&stores), None);
    }

    #[test]
    fn test_determine_level_phase_when_active_spec_no_phases() {
        let dir = TestDir::new("loopr-gen-needphase");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Phase));
    }

    #[test]
    fn test_determine_level_work_when_active_phase_no_wis() {
        let dir = TestDir::new("loopr-gen-needwi");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Work));
    }

    #[test]
    fn test_determine_level_none_when_all_levels_populated() {
        let dir = TestDir::new("loopr-gen-full");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi = Work::new(phase_id, "WI 1".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert_eq!(determine_generation_level(&stores), None);
    }

    // --- find_* helper tests ---

    #[test]
    fn test_find_active_plan_none() {
        let dir = TestDir::new("loopr-gen-fap-none");
        let stores = test_stores(&dir);
        assert!(find_active_plan(&stores).is_none());
    }

    #[test]
    fn test_find_active_plan_some() {
        let dir = TestDir::new("loopr-gen-fap-some");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Active".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan.clone());

        let found = find_active_plan(&stores).unwrap();
        assert_eq!(found.id, plan.id);
    }

    #[test]
    fn test_find_active_plan_skips_draft() {
        let dir = TestDir::new("loopr-gen-fap-skip");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert!(find_active_plan(&stores).is_none());
    }

    #[test]
    fn test_find_active_specs_for_plan() {
        let dir = TestDir::new("loopr-gen-fasp");
        let stores = test_stores(&dir);

        let mut spec1 = Spec::new("plan-1".into(), "Active Spec".into(), "desc".into());
        spec1.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec1.id.clone(), spec1);

        let spec2 = Spec::new("plan-1".into(), "Draft Spec".into(), "desc".into());
        stores.specs.write().unwrap().insert(spec2.id.clone(), spec2);

        let mut spec3 = Spec::new("plan-2".into(), "Other Plan Spec".into(), "desc".into());
        spec3.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec3.id.clone(), spec3);

        let active = find_active_specs_for_plan(&stores, "plan-1");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Active Spec");
    }

    #[test]
    fn test_find_active_phases_for_spec_sorted() {
        let dir = TestDir::new("loopr-gen-faps");
        let stores = test_stores(&dir);

        let mut p2 = Phase::new("spec-1".into(), "Phase 2".into(), "desc".into(), 2);
        p2.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(p2.id.clone(), p2);

        let mut p1 = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        p1.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(p1.id.clone(), p1);

        let phases = find_active_phases_for_spec(&stores, "spec-1");
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].order, 1);
        assert_eq!(phases[1].order, 2);
    }

    #[test]
    fn test_find_works_for_phase() {
        let dir = TestDir::new("loopr-gen-fwip");
        let stores = test_stores(&dir);

        let wi1 = Work::new("phase-1".into(), "WI 1".into(), "desc".into());
        let wi2 = Work::new("phase-1".into(), "WI 2".into(), "desc".into());
        let wi3 = Work::new("phase-2".into(), "WI 3".into(), "desc".into());
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);
        stores.works.write().unwrap().insert(wi3.id.clone(), wi3);

        let wis = find_works_for_phase(&stores, "phase-1");
        assert_eq!(wis.len(), 2);
    }

    #[test]
    fn test_find_phase_needing_works() {
        let dir = TestDir::new("loopr-gen-fpnwi");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase1.status = HierarchyStatus::Active;
        let phase1_id = phase1.id.clone();
        stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

        let mut phase2 = Phase::new(spec_id, "Phase 2".into(), "desc".into(), 2);
        phase2.status = HierarchyStatus::Active;
        let phase2_id = phase2.id.clone();
        stores.phases.write().unwrap().insert(phase2_id, phase2);

        // Add WI to Phase 1 only
        let wi = Work::new(phase1_id, "WI".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Should find Phase 2 (no WIs)
        let phase = find_phase_needing_works(&stores).unwrap();
        assert_eq!(phase.title, "Phase 2");
    }

    #[test]
    fn test_find_phase_needing_works_none_when_all_have() {
        let dir = TestDir::new("loopr-gen-fpnwi2");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi = Work::new(phase_id, "WI".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert!(find_phase_needing_works(&stores).is_none());
    }

    // --- is_phase_complete tests ---

    #[test]
    fn test_is_phase_complete_true() {
        let dir = TestDir::new("loopr-gen-ipc-true");
        let stores = test_stores(&dir);

        let mut wi = Work::new("phase-1".into(), "WI".into(), "desc".into());
        wi.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert!(is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_not_done() {
        let dir = TestDir::new("loopr-gen-ipc-false");
        let stores = test_stores(&dir);

        let wi = Work::new("phase-1".into(), "WI".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_no_wis() {
        let dir = TestDir::new("loopr-gen-ipc-empty");
        let stores = test_stores(&dir);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    // Fix #6: is_phase_complete now accepts Abandoned as terminal
    #[test]
    fn test_is_phase_complete_true_with_abandoned() {
        let dir = TestDir::new("loopr-gen-ipc-aband");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-1".into(), "WI Done".into(), "desc".into());
        wi1.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

        let mut wi2 = Work::new("phase-1".into(), "WI Abandoned".into(), "desc".into());
        wi2.status = WorkStatus::Abandoned;
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        assert!(is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_mixed_nonterminal() {
        let dir = TestDir::new("loopr-gen-ipc-mixed");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-1".into(), "WI Done".into(), "desc".into());
        wi1.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

        let wi2 = Work::new("phase-1".into(), "WI InProgress".into(), "desc".into());
        // Default status is Draft which is not terminal
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    // --- collect_failure_messages tests ---

    #[test]
    fn test_collect_failure_messages_empty() {
        let messages = collect_failure_messages(&[]);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_collect_failure_messages_deduplicates() {
        use crate::domain::validation::{IssueSeverity, ValidationIssue, ValidationVerdict};
        let r1 = ValidationReport::new(
            "plans".into(),
            "p1".into(),
            ValidationVerdict::Fail,
            vec![ValidationIssue {
                severity: IssueSeverity::Error,
                category: "completeness".into(),
                message: "Missing criteria".into(),
                suggestion: None,
            }],
            "Incomplete".into(),
            "model".into(),
        );
        let r2 = ValidationReport::new(
            "plans".into(),
            "p1".into(),
            ValidationVerdict::Fail,
            vec![
                ValidationIssue {
                    severity: IssueSeverity::Error,
                    category: "completeness".into(),
                    message: "Missing criteria".into(), // duplicate
                    suggestion: None,
                },
                ValidationIssue {
                    severity: IssueSeverity::Warning,
                    category: "scope".into(),
                    message: "Too broad".into(),
                    suggestion: None,
                },
            ],
            "Still incomplete".into(),
            "model".into(),
        );
        let messages = collect_failure_messages(&[r1, r2]);
        // Should have: "Incomplete", "Missing criteria", "Still incomplete", "Too broad" — no duplicate "Missing criteria"
        assert_eq!(messages.len(), 4);
        assert!(messages.contains(&"Missing criteria".to_string()));
        assert!(messages.contains(&"Too broad".to_string()));
        assert!(messages.contains(&"Incomplete".to_string()));
        assert!(messages.contains(&"Still incomplete".to_string()));
    }

    // --- find_failed_validations tests ---

    #[test]
    fn test_find_failed_validations_empty_store() {
        let dir = TestDir::new("loopr-gen-ffv-empty");
        let stores = test_stores(&dir);
        let reports = find_failed_validations(&stores, "plans", "plan-1");
        assert!(reports.is_empty());
    }

    #[test]
    fn test_find_failed_validations_returns_only_fails() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ffv-fails");
        let stores = test_stores(&dir);

        let fail_report = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Fail,
            vec![],
            "bad".into(),
            "m".into(),
        );
        let pass_report = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Pass,
            vec![],
            "good".into(),
            "m".into(),
        );
        {
            let store = stores.store.as_ref().unwrap();
            store.lock().unwrap().create(fail_report.clone()).unwrap();
            store.lock().unwrap().create(pass_report).unwrap();
        }

        let reports = find_failed_validations(&stores, "plans", "plan-1");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].id, fail_report.id);
    }

    // --- find_draft_needing_regeneration tests ---

    #[test]
    fn test_find_draft_needing_regeneration_no_drafts() {
        let dir = TestDir::new("loopr-gen-fdnr-none");
        let stores = test_stores(&dir);
        assert!(find_draft_needing_regeneration(&stores, 3).is_none());
    }

    #[test]
    fn test_find_draft_needing_regeneration_draft_no_failures() {
        let dir = TestDir::new("loopr-gen-fdnr-nofail");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        // Draft exists but no failed validations → no regeneration needed
        assert!(find_draft_needing_regeneration(&stores, 3).is_none());
    }

    #[test]
    fn test_find_draft_needing_regeneration_plan_with_failures() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-fdnr-plan");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Add a failed validation report
        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "bad plan".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let regen = find_draft_needing_regeneration(&stores, 3).unwrap();
        assert_eq!(regen.level, GenerationLevel::Plan);
        assert_eq!(regen.target_id, plan_id);
        assert_eq!(regen.attempt_count, 1);
        assert!(!regen.accumulated_failures.is_empty());
    }

    #[test]
    fn test_find_draft_needing_regeneration_cap_reached() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-fdnr-cap");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Add 3 failed validation reports (= max_validation_attempts)
        for i in 0..3 {
            let report = ValidationReport::new(
                "plans".into(),
                plan_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("failure {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        // Cap reached → should return None (not regeneration)
        assert!(find_draft_needing_regeneration(&stores, 3).is_none());
    }

    // --- is_validation_cap_reached tests ---

    #[test]
    fn test_is_validation_cap_reached_false_no_drafts() {
        let dir = TestDir::new("loopr-gen-ivcr-none");
        let stores = test_stores(&dir);
        assert!(!is_validation_cap_reached(&stores, 3));
    }

    #[test]
    fn test_is_validation_cap_reached_false_under_cap() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ivcr-under");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let report = ValidationReport::new(
            "plans".into(),
            plan_id,
            ValidationVerdict::Fail,
            vec![],
            "bad".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        assert!(!is_validation_cap_reached(&stores, 3)); // 1 < 3
    }

    #[test]
    fn test_is_validation_cap_reached_true_at_cap() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ivcr-at");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        for i in 0..3 {
            let report = ValidationReport::new(
                "plans".into(),
                plan_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("failure {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        assert!(is_validation_cap_reached(&stores, 3)); // 3 >= 3
    }

    // --- New coverage tests ---

    #[test]
    fn test_determine_level_multiple_active_plans() {
        // When multiple active plans exist, determine_generation_level should still find one
        // and proceed to check specs (returns Spec since no specs exist).
        let dir = TestDir::new("loopr-gen-multi-plan");
        let stores = test_stores(&dir);

        let mut plan1 = Plan::new("Plan A".into(), "desc".into(), "crit".into());
        plan1.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan1.id.clone(), plan1);

        let mut plan2 = Plan::new("Plan B".into(), "desc".into(), "crit".into());
        plan2.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan2.id.clone(), plan2);

        // With active plans but no specs, should want Spec
        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Spec));
    }

    #[test]
    fn test_determine_level_multiple_active_specs() {
        // Multiple active specs under one active plan; no phases → should want Phase.
        let dir = TestDir::new("loopr-gen-multi-spec");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec1 = Spec::new(plan_id.clone(), "Spec A".into(), "desc".into());
        spec1.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec1.id.clone(), spec1);

        let mut spec2 = Spec::new(plan_id, "Spec B".into(), "desc".into());
        spec2.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec2.id.clone(), spec2);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Phase));
    }

    #[test]
    fn test_determine_level_draft_spec_with_active_plan() {
        // Active plan + draft spec (no active spec) → None (wait for validation).
        let dir = TestDir::new("loopr-gen-draft-spec-ap");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Draft spec (default status is Draft)
        let spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        assert_eq!(determine_generation_level(&stores), None);
    }

    #[test]
    fn test_find_draft_regen_spec_with_failures() {
        // Draft spec under active plan with failed validation → should return Spec regen info.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-regen-spec");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let draft_spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        let spec_id = draft_spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), draft_spec);

        let report = ValidationReport::new(
            "specs".into(),
            spec_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "spec is incomplete".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let regen = find_draft_needing_regeneration(&stores, 3).unwrap();
        assert_eq!(regen.level, GenerationLevel::Spec);
        assert_eq!(regen.target_id, spec_id);
        assert_eq!(regen.collection, "specs");
        assert_eq!(regen.attempt_count, 1);
    }

    #[test]
    fn test_find_draft_regen_phase_with_failures() {
        // Draft phase under active spec/plan with failed validation → Phase regen info.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-regen-phase");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let draft_phase = Phase::new(spec_id, "Draft Phase".into(), "desc".into(), 1);
        let phase_id = draft_phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), draft_phase);

        let report = ValidationReport::new(
            "phases".into(),
            phase_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "phase ordering is wrong".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let regen = find_draft_needing_regeneration(&stores, 3).unwrap();
        assert_eq!(regen.level, GenerationLevel::Phase);
        assert_eq!(regen.target_id, phase_id);
        assert_eq!(regen.collection, "phases");
        assert_eq!(regen.attempt_count, 1);
    }

    #[test]
    fn test_find_draft_regen_multiple_drafts_returns_first() {
        // When a draft plan exists, it is checked first even if there are draft specs deeper.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-regen-multi");
        let stores = test_stores(&dir);

        // Draft plan with failures
        let draft_plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = draft_plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), draft_plan);

        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "plan fail".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft plan takes priority — returns Plan level
        let regen = find_draft_needing_regeneration(&stores, 3).unwrap();
        assert_eq!(regen.level, GenerationLevel::Plan);
        assert_eq!(regen.target_id, plan_id);
    }

    #[test]
    fn test_is_validation_cap_over_cap() {
        // When failures exceed cap, is_validation_cap_reached should still return true.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ivcr-over");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Add 5 failures, cap is 3
        for i in 0..5 {
            let report = ValidationReport::new(
                "plans".into(),
                plan_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("failure {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        assert!(is_validation_cap_reached(&stores, 3)); // 5 >= 3
    }

    #[test]
    fn test_find_failed_validations_multiple_for_same_target() {
        // Multiple fail reports for the same target should all be returned, sorted by created_at.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ffv-multi");
        let stores = test_stores(&dir);

        for i in 0..4 {
            let report = ValidationReport::new(
                "plans".into(),
                "target-1".into(),
                ValidationVerdict::Fail,
                vec![],
                format!("failure {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        let reports = find_failed_validations(&stores, "plans", "target-1");
        assert_eq!(reports.len(), 4);
        // Verify sorted by created_at (ascending)
        for window in reports.windows(2) {
            assert!(window[0].created_at <= window[1].created_at);
        }
    }

    #[test]
    fn test_find_failed_validations_wrong_collection_excluded() {
        // Fail reports for a different collection should not be returned.
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-ffv-coll");
        let stores = test_stores(&dir);

        let report = ValidationReport::new(
            "specs".into(),
            "target-1".into(),
            ValidationVerdict::Fail,
            vec![],
            "wrong collection".into(),
            "m".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Query for "plans" collection — should find nothing
        let reports = find_failed_validations(&stores, "plans", "target-1");
        assert!(reports.is_empty());
    }

    #[test]
    fn test_find_works_for_phase_ordering() {
        // find_works_for_phase returns all WIs for the phase regardless of status.
        let dir = TestDir::new("loopr-gen-fwip-ord");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-x".into(), "WI A".into(), "desc a".into());
        wi1.status = WorkStatus::Done;
        let mut wi2 = Work::new("phase-x".into(), "WI B".into(), "desc b".into());
        wi2.status = WorkStatus::InProgress;
        let wi3 = Work::new("phase-x".into(), "WI C".into(), "desc c".into());
        // wi3 stays Draft (default)
        let wi_other = Work::new("phase-y".into(), "WI Other".into(), "not this phase".into());

        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);
        stores.works.write().unwrap().insert(wi3.id.clone(), wi3);
        stores.works.write().unwrap().insert(wi_other.id.clone(), wi_other);

        let wis = find_works_for_phase(&stores, "phase-x");
        assert_eq!(wis.len(), 3);
        // All should belong to phase-x
        assert!(wis.iter().all(|w| w.phase_id == "phase-x"));
    }

    // --- Prompt building with learnings/findings (covering branches at lines 119-180, 237-254) ---

    #[test]
    fn test_build_spec_prompt_with_learnings_and_findings() {
        crate::prompts::init_defaults();
        let mut plan = Plan::new("Auth".into(), "JWT auth".into(), "Must secure".into());
        plan.status = HierarchyStatus::Active;

        let learnings = vec!["Use bcrypt".to_string(), "Rate limit".to_string()];
        let findings = vec!["Found auth.rs".to_string()];
        let failures = vec!["Missing edge case".to_string()];

        let prompt = build_spec_prompt(&plan, &learnings, &findings, &failures, None);
        assert_eq!(prompt.level, GenerationLevel::Spec);
        assert!(prompt.user_message.contains("### Relevant Learnings"));
        assert!(prompt.user_message.contains("Use bcrypt"));
        assert!(prompt.user_message.contains("Rate limit"));
        assert!(prompt.user_message.contains("### Codebase Findings"));
        assert!(prompt.user_message.contains("Found auth.rs"));
        assert!(prompt.user_message.contains("### Previous Validation Failures"));
        assert!(prompt.user_message.contains("Missing edge case"));
    }

    #[test]
    fn test_build_phase_prompt_with_learnings() {
        crate::prompts::init_defaults();
        let mut spec = Spec::new("plan-1".into(), "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;

        let learnings = vec!["Always test edge cases".to_string()];
        let failures: Vec<String> = vec![];

        let prompt = build_phase_prompt(&spec, &learnings, &failures, None);
        assert_eq!(prompt.level, GenerationLevel::Phase);
        assert!(prompt.user_message.contains("### Relevant Learnings"));
        assert!(prompt.user_message.contains("Always test edge cases"));
        assert!(!prompt.user_message.contains("### Previous Validation Failures"));
    }

    #[test]
    fn test_build_work_prompt_with_learnings_and_findings() {
        crate::prompts::init_defaults();
        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let learnings = vec!["Use generics".to_string()];
        let findings = vec!["Module at src/lib.rs".to_string()];

        let prompt = build_work_prompt(&phase, &[], &learnings, &findings, None);
        assert_eq!(prompt.level, GenerationLevel::Work);
        assert!(prompt.user_message.contains("### Relevant Learnings"));
        assert!(prompt.user_message.contains("Use generics"));
        assert!(prompt.user_message.contains("### Codebase Context"));
        assert!(prompt.user_message.contains("Module at src/lib.rs"));
    }

    // --- Spec/Phase validation cap checks (covering lines 617-658) ---

    #[test]
    fn test_is_validation_cap_reached_at_spec_level() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-vcap-spec");
        let stores = test_stores(&dir);

        // Active plan
        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Draft spec
        let spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // 3 failed reports on spec (cap = 3)
        for i in 0..3 {
            let report = ValidationReport::new(
                "specs".into(),
                spec_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("fail {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        assert!(is_validation_cap_reached(&stores, 3));
    }

    #[test]
    fn test_is_validation_cap_reached_at_phase_level() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-vcap-phase");
        let stores = test_stores(&dir);

        // Active plan
        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Active spec
        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Draft phase
        let phase = Phase::new(spec_id, "Draft Phase".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // 3 failed reports on phase (cap = 3)
        for i in 0..3 {
            let report = ValidationReport::new(
                "phases".into(),
                phase_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("phase fail {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        assert!(is_validation_cap_reached(&stores, 3));
    }

    #[test]
    fn test_find_draft_regen_returns_none_for_phase_at_cap() {
        use crate::domain::validation::ValidationVerdict;
        let dir = TestDir::new("loopr-gen-regen-phase-cap");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let phase = Phase::new(spec_id, "Draft Phase".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // 3 failures = at cap → return None
        for i in 0..3 {
            let report = ValidationReport::new(
                "phases".into(),
                phase_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                format!("fail {}", i),
                "m".into(),
            );
            stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();
        }

        assert!(find_draft_needing_regeneration(&stores, 3).is_none());
    }
}
