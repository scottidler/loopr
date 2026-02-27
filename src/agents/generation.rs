//! Document generation prompts for the Coordinator's Plan → Spec → Phase → WorkItem pipeline.
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
use crate::domain::phase::Phase;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::spec::Spec;
use crate::domain::work_item::{WorkItem, WorkItemStatus};

/// Which level of the hierarchy to generate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerationLevel {
    Plan,
    Spec,
    Phase,
    WorkItem,
}

impl std::fmt::Display for GenerationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GenerationLevel::Plan => write!(f, "Plan"),
            GenerationLevel::Spec => write!(f, "Spec"),
            GenerationLevel::Phase => write!(f, "Phase"),
            GenerationLevel::WorkItem => write!(f, "WorkItem"),
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
pub fn build_plan_prompt(goal: &str, learnings: &[String], validation_failures: &[String]) -> GenerationPrompt {
    let mut msg = String::with_capacity(2048);

    msg.push_str("## Task: Generate a Plan\n\n");

    msg.push_str("### Current State\n");
    msg.push_str("No active Plan exists. Create one to address the user's goal.\n\n");

    msg.push_str("### User Intent\n");
    msg.push_str(goal);
    msg.push_str("\n\n");

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
    msg.push_str(
        "Create a Plan with:\n\
         - A clear, bounded title\n\
         - A description of what this achieves and why\n\
         - Measurable acceptance criteria (specific, testable conditions)\n\n\
         Respond with a JSON array containing a single `create_plan` action.\n",
    );

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
) -> GenerationPrompt {
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate a Spec\n\n");

    msg.push_str("### Active Plan\n");
    msg.push_str(&format!("- **ID:** {}\n", plan.id));
    msg.push_str(&format!("- **Title:** {}\n", plan.title));
    msg.push_str(&format!("- **Description:** {}\n", plan.description));
    msg.push_str(&format!("- **Acceptance Criteria:** {}\n\n", plan.acceptance_criteria));

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
    msg.push_str(
        "Create a Spec for this Plan with:\n\
         - A title describing the technical approach\n\
         - A description covering: technical approach to satisfy the Plan, \
           key design decisions with rationale, testability strategy, risks and dependencies\n\n\
         Respond with a JSON array containing a single `create_spec` action with `plan_id` set to the Plan's ID.\n",
    );

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
pub fn build_phase_prompt(spec: &Spec, learnings: &[String], validation_failures: &[String]) -> GenerationPrompt {
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate Implementation Phases\n\n");

    msg.push_str("### Active Spec\n");
    msg.push_str(&format!("- **ID:** {}\n", spec.id));
    msg.push_str(&format!("- **Plan ID:** {}\n", spec.plan_id));
    msg.push_str(&format!("- **Title:** {}\n", spec.title));
    msg.push_str(&format!("- **Description:** {}\n\n", spec.description));

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
    msg.push_str(
        "Create ordered implementation Phases for this Spec. Each Phase should have:\n\
         - A clear, actionable title\n\
         - A concrete deliverables description\n\
         - Dependencies on other Phases\n\
         - Be implementable in 1-5 WorkItems\n\n\
         Respond with a JSON array of `create_phase` actions with `spec_id` set to the Spec's ID, \
         ordered by implementation sequence (order: 1, 2, 3, ...).\n",
    );

    GenerationPrompt {
        level: GenerationLevel::Phase,
        user_message: msg,
    }
}

/// Build a WorkItem generation prompt.
///
/// Input context:
/// - Active Phase (title, order, ID, spec reference, description)
/// - Existing WorkItems in this Phase (to avoid duplicates)
/// - Relevant learnings (scoped to Phase + Spec + Plan + Global)
/// - Codebase context (researcher findings about affected modules)
pub fn build_work_item_prompt(
    phase: &Phase,
    existing_work_items: &[WorkItem],
    learnings: &[String],
    findings: &[String],
) -> GenerationPrompt {
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate WorkItems\n\n");

    msg.push_str("### Active Phase\n");
    msg.push_str(&format!("- **ID:** {}\n", phase.id));
    msg.push_str(&format!("- **Spec ID:** {}\n", phase.spec_id));
    msg.push_str(&format!("- **Title:** {}\n", phase.title));
    msg.push_str(&format!("- **Order:** {}\n", phase.order));
    msg.push_str(&format!("- **Description:** {}\n\n", phase.description));

    msg.push_str("### Existing WorkItems in this Phase\n");
    if existing_work_items.is_empty() {
        msg.push_str("None yet.\n\n");
    } else {
        for wi in existing_work_items {
            msg.push_str(&format!(
                "- [{}] {} ({}) — {}\n",
                wi.id, wi.title, wi.status, wi.description
            ));
        }
        msg.push('\n');
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
    msg.push_str(
        "Create WorkItems for this Phase. Each WorkItem should be:\n\
         - Small enough for an Implementer to complete in ~5-10 iterations\n\
         - Have a clear title and description with acceptance criteria\n\
         - Include resource_tags identifying affected files/modules\n\
         - Declare dependencies on other WorkItems\n\n\
         Respond with a JSON array of `create_work_item` actions with `phase_id` set to the Phase's ID.\n",
    );

    GenerationPrompt {
        level: GenerationLevel::WorkItem,
        user_message: msg,
    }
}

/// Determine which generation level the Coordinator should focus on, based on current state.
///
/// Returns `None` if no generation is needed (all levels have Active records or WorkItems exist).
///
/// Decision tree:
/// 1. No active Plan AND no Draft Plan? → Plan
/// 2. Active Plan, no active Specs AND no Draft Specs? → Spec
/// 3. Active Specs, no active Phases AND no Draft Phases? → Phase
/// 4. Active Phases, no WorkItems for them? → WorkItem
/// 5. Otherwise → None (generation not needed)
pub fn determine_generation_level(stores: &Stores) -> Option<GenerationLevel> {
    let plans = stores.plans.read().unwrap();

    // Check for active Plan
    let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active);
    let draft_plan = plans.values().find(|p| p.status == HierarchyStatus::Draft);

    if active_plan.is_none() && draft_plan.is_none() {
        return Some(GenerationLevel::Plan);
    }

    // If there's no active Plan (only Draft), don't advance — Coordinator should validate the Draft
    let active_plan = active_plan?;

    // Check for Specs under the active Plan
    let specs = stores.specs.read().unwrap();
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
    let phases = stores.phases.read().unwrap();
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

    // Check for WorkItems under active Phases
    let work_items = stores.work_items.read().unwrap();
    let active_phase_ids: Vec<_> = spec_phases
        .iter()
        .filter(|p| p.status == HierarchyStatus::Active)
        .map(|p| p.id.as_str())
        .collect();

    let has_work_items = work_items
        .values()
        .any(|w| active_phase_ids.contains(&w.phase_id.as_str()));

    if !has_work_items {
        return Some(GenerationLevel::WorkItem);
    }

    None
}

/// Find the active Plan from stores. Returns None if no active Plan.
pub fn find_active_plan(stores: &Stores) -> Option<Plan> {
    let plans = stores.plans.read().unwrap();
    plans.values().find(|p| p.status == HierarchyStatus::Active).cloned()
}

/// Find active Specs for a given Plan.
pub fn find_active_specs_for_plan(stores: &Stores, plan_id: &str) -> Vec<Spec> {
    let specs = stores.specs.read().unwrap();
    specs
        .values()
        .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
        .cloned()
        .collect()
}

/// Find active Phases for a given Spec.
pub fn find_active_phases_for_spec(stores: &Stores, spec_id: &str) -> Vec<Phase> {
    let phases = stores.phases.read().unwrap();
    let mut result: Vec<_> = phases
        .values()
        .filter(|p| p.spec_id == spec_id && p.status == HierarchyStatus::Active)
        .cloned()
        .collect();
    result.sort_by_key(|p| p.order);
    result
}

/// Find existing WorkItems for a given Phase.
pub fn find_work_items_for_phase(stores: &Stores, phase_id: &str) -> Vec<WorkItem> {
    let work_items = stores.work_items.read().unwrap();
    work_items
        .values()
        .filter(|w| w.phase_id == phase_id)
        .cloned()
        .collect()
}

/// Find the first active Phase that still needs WorkItems.
pub fn find_phase_needing_work_items(stores: &Stores) -> Option<Phase> {
    let plans = stores.plans.read().unwrap();
    let active_plan = plans.values().find(|p| p.status == HierarchyStatus::Active)?;
    let plan_id = active_plan.id.clone();
    drop(plans);

    let specs = stores.specs.read().unwrap();
    let active_spec_ids: Vec<String> = specs
        .values()
        .filter(|s| s.plan_id == plan_id && s.status == HierarchyStatus::Active)
        .map(|s| s.id.clone())
        .collect();
    drop(specs);

    let phases = stores.phases.read().unwrap();
    let mut active_phases: Vec<_> = phases
        .values()
        .filter(|p| active_spec_ids.contains(&p.spec_id) && p.status == HierarchyStatus::Active)
        .cloned()
        .collect();
    active_phases.sort_by_key(|p| p.order);
    drop(phases);

    let work_items = stores.work_items.read().unwrap();
    for phase in active_phases {
        let has_wi = work_items.values().any(|w| w.phase_id == phase.id);
        if !has_wi {
            return Some(phase);
        }
    }

    None
}

/// Check if all WorkItems in a Phase are Done.
pub fn is_phase_complete(stores: &Stores, phase_id: &str) -> bool {
    let work_items = stores.work_items.read().unwrap();
    let phase_wis: Vec<_> = work_items.values().filter(|w| w.phase_id == phase_id).collect();
    !phase_wis.is_empty() && phase_wis.iter().all(|w| w.status == WorkItemStatus::Done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::daemon::context::Stores;
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

    // --- GenerationLevel tests ---

    #[test]
    fn test_generation_level_display() {
        assert_eq!(GenerationLevel::Plan.to_string(), "Plan");
        assert_eq!(GenerationLevel::Spec.to_string(), "Spec");
        assert_eq!(GenerationLevel::Phase.to_string(), "Phase");
        assert_eq!(GenerationLevel::WorkItem.to_string(), "WorkItem");
    }

    // --- Plan prompt tests ---

    #[test]
    fn test_plan_prompt_includes_goal() {
        let prompt = build_plan_prompt("Build a REST API", &[], &[]);
        assert!(prompt.user_message.contains("Build a REST API"));
        assert_eq!(prompt.level, GenerationLevel::Plan);
    }

    #[test]
    fn test_plan_prompt_includes_learnings() {
        let learnings = vec!["Use async handlers".to_string(), "Prefer JSON responses".to_string()];
        let prompt = build_plan_prompt("Build API", &learnings, &[]);
        assert!(prompt.user_message.contains("Use async handlers"));
        assert!(prompt.user_message.contains("Prefer JSON responses"));
        assert!(prompt.user_message.contains("Relevant Learnings"));
    }

    #[test]
    fn test_plan_prompt_includes_accumulated_failures() {
        let failures = vec!["Missing acceptance criteria".to_string(), "Title too vague".to_string()];
        let prompt = build_plan_prompt("Build API", &[], &failures);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("1. Missing acceptance criteria"));
        assert!(prompt.user_message.contains("2. Title too vague"));
        assert!(prompt.user_message.contains("fix ALL of these"));
    }

    #[test]
    fn test_plan_prompt_no_optional_sections_when_empty() {
        let prompt = build_plan_prompt("Build API", &[], &[]);
        assert!(!prompt.user_message.contains("Relevant Learnings"));
        assert!(!prompt.user_message.contains("Validation Failures"));
    }

    #[test]
    fn test_plan_prompt_instructions_present() {
        let prompt = build_plan_prompt("Build API", &[], &[]);
        assert!(prompt.user_message.contains("create_plan"));
        assert!(prompt.user_message.contains("acceptance criteria"));
    }

    // --- Spec prompt tests ---

    #[test]
    fn test_spec_prompt_includes_plan_context() {
        let plan = Plan::new("Auth Plan".into(), "Implement auth".into(), "Tests pass".into());
        let prompt = build_spec_prompt(&plan, &[], &[], &[]);
        assert!(prompt.user_message.contains(&plan.id));
        assert!(prompt.user_message.contains("Auth Plan"));
        assert!(prompt.user_message.contains("Implement auth"));
        assert!(prompt.user_message.contains("Tests pass"));
        assert_eq!(prompt.level, GenerationLevel::Spec);
    }

    #[test]
    fn test_spec_prompt_includes_findings() {
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let findings = vec!["src/auth.rs has existing login logic".to_string()];
        let prompt = build_spec_prompt(&plan, &[], &findings, &[]);
        assert!(prompt.user_message.contains("Codebase Findings"));
        assert!(prompt.user_message.contains("src/auth.rs has existing login logic"));
    }

    #[test]
    fn test_spec_prompt_includes_accumulated_failures() {
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let failures = vec!["Missing testability strategy".to_string()];
        let prompt = build_spec_prompt(&plan, &[], &[], &failures);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("Missing testability strategy"));
    }

    #[test]
    fn test_spec_prompt_instructions_reference_plan_id() {
        let plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        let prompt = build_spec_prompt(&plan, &[], &[], &[]);
        assert!(prompt.user_message.contains("create_spec"));
        assert!(prompt.user_message.contains("plan_id"));
    }

    // --- Phase prompt tests ---

    #[test]
    fn test_phase_prompt_includes_spec_context() {
        let spec = Spec::new("plan-1".into(), "JWT Auth".into(), "Implement JWT".into());
        let prompt = build_phase_prompt(&spec, &[], &[]);
        assert!(prompt.user_message.contains(&spec.id));
        assert!(prompt.user_message.contains("plan-1"));
        assert!(prompt.user_message.contains("JWT Auth"));
        assert!(prompt.user_message.contains("Implement JWT"));
        assert_eq!(prompt.level, GenerationLevel::Phase);
    }

    #[test]
    fn test_phase_prompt_includes_accumulated_failures() {
        let spec = Spec::new("plan-1".into(), "Spec".into(), "desc".into());
        let failures = vec![
            "Phase 2 depends on Phase 1 but order is wrong".to_string(),
            "Missing deliverables in Phase 3".to_string(),
        ];
        let prompt = build_phase_prompt(&spec, &[], &failures);
        assert!(prompt.user_message.contains("Previous Validation Failures"));
        assert!(prompt.user_message.contains("Phase 2 depends on Phase 1"));
        assert!(prompt.user_message.contains("Missing deliverables"));
    }

    #[test]
    fn test_phase_prompt_instructions() {
        let spec = Spec::new("plan-1".into(), "Spec".into(), "desc".into());
        let prompt = build_phase_prompt(&spec, &[], &[]);
        assert!(prompt.user_message.contains("create_phase"));
        assert!(prompt.user_message.contains("spec_id"));
        assert!(prompt.user_message.contains("order"));
    }

    // --- WorkItem prompt tests ---

    #[test]
    fn test_work_item_prompt_includes_phase_context() {
        let phase = Phase::new("spec-1".into(), "Foundation".into(), "Set up base".into(), 1);
        let prompt = build_work_item_prompt(&phase, &[], &[], &[]);
        assert!(prompt.user_message.contains(&phase.id));
        assert!(prompt.user_message.contains("spec-1"));
        assert!(prompt.user_message.contains("Foundation"));
        assert!(prompt.user_message.contains("Order:** 1"));
        assert_eq!(prompt.level, GenerationLevel::WorkItem);
    }

    #[test]
    fn test_work_item_prompt_includes_existing_work_items() {
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let wi = WorkItem::new(phase.id.clone(), "Add login".into(), "Login endpoint".into());
        let prompt = build_work_item_prompt(&phase, &[wi], &[], &[]);
        assert!(prompt.user_message.contains("Add login"));
        assert!(prompt.user_message.contains("Login endpoint"));
        assert!(!prompt.user_message.contains("None yet"));
    }

    #[test]
    fn test_work_item_prompt_shows_none_when_no_existing() {
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let prompt = build_work_item_prompt(&phase, &[], &[], &[]);
        assert!(prompt.user_message.contains("None yet"));
    }

    #[test]
    fn test_work_item_prompt_includes_findings() {
        let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
        let findings = vec!["src/auth/ directory has 5 modules".to_string()];
        let prompt = build_work_item_prompt(&phase, &[], &[], &findings);
        assert!(prompt.user_message.contains("Codebase Context"));
        assert!(prompt.user_message.contains("src/auth/ directory has 5 modules"));
    }

    // --- determine_generation_level tests ---

    #[test]
    fn test_determine_level_plan_when_empty() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-empty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Plan));
    }

    #[test]
    fn test_determine_level_none_when_draft_plan_exists() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-draftplan-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        // Draft plan exists — Coordinator should validate it, not generate a new one
        assert_eq!(determine_generation_level(&stores), None);
    }

    #[test]
    fn test_determine_level_spec_when_active_plan_no_specs() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-needspec-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Spec));
    }

    #[test]
    fn test_determine_level_none_when_draft_spec_exists() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-draftspec-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("loopr-gen-needphase-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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
    fn test_determine_level_work_item_when_active_phase_no_wis() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-needwi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::WorkItem));
    }

    #[test]
    fn test_determine_level_none_when_all_levels_populated() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-full-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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

        let wi = WorkItem::new(phase_id, "WI 1".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        assert_eq!(determine_generation_level(&stores), None);
    }

    // --- find_* helper tests ---

    #[test]
    fn test_find_active_plan_none() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fap-none-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        assert!(find_active_plan(&stores).is_none());
    }

    #[test]
    fn test_find_active_plan_some() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fap-some-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Active".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan.clone());

        let found = find_active_plan(&stores).unwrap();
        assert_eq!(found.id, plan.id);
    }

    #[test]
    fn test_find_active_plan_skips_draft() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fap-skip-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert!(find_active_plan(&stores).is_none());
    }

    #[test]
    fn test_find_active_specs_for_plan() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fasp-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("loopr-gen-faps-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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
    fn test_find_work_items_for_phase() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fwip-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let wi1 = WorkItem::new("phase-1".into(), "WI 1".into(), "desc".into());
        let wi2 = WorkItem::new("phase-1".into(), "WI 2".into(), "desc".into());
        let wi3 = WorkItem::new("phase-2".into(), "WI 3".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.work_items.write().unwrap().insert(wi2.id.clone(), wi2);
        stores.work_items.write().unwrap().insert(wi3.id.clone(), wi3);

        let wis = find_work_items_for_phase(&stores, "phase-1");
        assert_eq!(wis.len(), 2);
    }

    #[test]
    fn test_find_phase_needing_work_items() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fpnwi-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        let wi = WorkItem::new(phase1_id, "WI".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        // Should find Phase 2 (no WIs)
        let phase = find_phase_needing_work_items(&stores).unwrap();
        assert_eq!(phase.title, "Phase 2");
    }

    #[test]
    fn test_find_phase_needing_work_items_none_when_all_have() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-fpnwi2-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
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

        let wi = WorkItem::new(phase_id, "WI".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        assert!(find_phase_needing_work_items(&stores).is_none());
    }

    // --- is_phase_complete tests ---

    #[test]
    fn test_is_phase_complete_true() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-ipc-true-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let mut wi = WorkItem::new("phase-1".into(), "WI".into(), "desc".into());
        wi.status = WorkItemStatus::Done;
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        assert!(is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_not_done() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-ipc-false-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        let wi = WorkItem::new("phase-1".into(), "WI".into(), "desc".into());
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_no_wis() {
        let dir = std::env::temp_dir().join(format!("loopr-gen-ipc-empty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }
}
