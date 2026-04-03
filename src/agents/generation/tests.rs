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
    let prompt = build_work_prompt(&phase, &[], &[], &[], None, None);
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
    let prompt = build_work_prompt(&phase, &[wi], &[], &[], None, None);
    assert!(prompt.user_message.contains("Add login"));
    assert!(prompt.user_message.contains("Login endpoint"));
    assert!(!prompt.user_message.contains("None yet"));
}

#[test]
fn test_work_prompt_shows_none_when_no_existing() {
    init();
    let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
    let prompt = build_work_prompt(&phase, &[], &[], &[], None, None);
    assert!(prompt.user_message.contains("None yet"));
}

#[test]
fn test_work_prompt_includes_dependency_instructions() {
    init();
    let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
    let prompt = build_work_prompt(&phase, &[], &[], &[], None, None);
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
    let prompt = build_work_prompt(&phase, &[wi], &[], &[], None, None);
    assert!(prompt.user_message.contains("deps: dep-1"));
    assert!(prompt.user_message.contains("use the exact IDs above"));
}

#[test]
fn test_work_prompt_includes_findings() {
    init();
    let phase = Phase::new("spec-1".into(), "Phase".into(), "desc".into(), 1);
    let findings = vec!["src/auth/ directory has 5 modules".to_string()];
    let prompt = build_work_prompt(&phase, &[], &[], &findings, None, None);
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
    plan.force_status(HierarchyStatus::Active);
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Spec));
}

#[test]
fn test_determine_level_none_when_draft_spec_exists() {
    let dir = TestDir::new("loopr-gen-draftspec");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Phase));
}

#[test]
fn test_determine_level_work_when_active_phase_no_wis() {
    let dir = TestDir::new("loopr-gen-needwi");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
    phase.force_status(HierarchyStatus::Active);
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Work));
}

#[test]
fn test_determine_level_none_when_all_levels_populated() {
    let dir = TestDir::new("loopr-gen-full");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
    phase.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
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
    spec1.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec1.id.clone(), spec1);

    let spec2 = Spec::new("plan-1".into(), "Draft Spec".into(), "desc".into());
    stores.specs.write().unwrap().insert(spec2.id.clone(), spec2);

    let mut spec3 = Spec::new("plan-2".into(), "Other Plan Spec".into(), "desc".into());
    spec3.force_status(HierarchyStatus::Active);
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
    p2.force_status(HierarchyStatus::Active);
    stores.phases.write().unwrap().insert(p2.id.clone(), p2);

    let mut p1 = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
    p1.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
    phase1.force_status(HierarchyStatus::Active);
    let phase1_id = phase1.id.clone();
    stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

    let mut phase2 = Phase::new(spec_id, "Phase 2".into(), "desc".into(), 2);
    phase2.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id, "Phase 1".into(), "desc".into(), 1);
    phase.force_status(HierarchyStatus::Active);
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
    wi.force_status(WorkStatus::Done);
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
    wi1.force_status(WorkStatus::Done);
    stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

    let mut wi2 = Work::new("phase-1".into(), "WI Abandoned".into(), "desc".into());
    wi2.force_status(WorkStatus::Abandoned);
    stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

    assert!(is_phase_complete(&stores, "phase-1"));
}

#[test]
fn test_is_phase_complete_false_mixed_nonterminal() {
    let dir = TestDir::new("loopr-gen-ipc-mixed");
    let stores = test_stores(&dir);

    let mut wi1 = Work::new("phase-1".into(), "WI Done".into(), "desc".into());
    wi1.force_status(WorkStatus::Done);
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
    plan1.force_status(HierarchyStatus::Active);
    stores.plans.write().unwrap().insert(plan1.id.clone(), plan1);

    let mut plan2 = Plan::new("Plan B".into(), "desc".into(), "crit".into());
    plan2.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec1 = Spec::new(plan_id.clone(), "Spec A".into(), "desc".into());
    spec1.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec1.id.clone(), spec1);

    let mut spec2 = Spec::new(plan_id, "Spec B".into(), "desc".into());
    spec2.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec2.id.clone(), spec2);

    assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Phase));
}

#[test]
fn test_determine_level_draft_spec_with_active_plan() {
    // Active plan + draft spec (no active spec) → None (wait for validation).
    let dir = TestDir::new("loopr-gen-draft-spec-ap");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
    plan.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
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
    wi1.force_status(WorkStatus::Done);
    let mut wi2 = Work::new("phase-x".into(), "WI B".into(), "desc b".into());
    wi2.force_status(WorkStatus::InProgress);
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
    plan.force_status(HierarchyStatus::Active);

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
    spec.force_status(HierarchyStatus::Active);

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

    let prompt = build_work_prompt(&phase, &[], &learnings, &findings, None, None);
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
    plan.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    // Active spec
    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
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
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id, "Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
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

// --- Coverage evaluation helper tests ---

use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::coverage::{CoverageGap, CoverageReport, CoverageReportParams, CoverageVerdict, GapSeverity};

fn make_coverage_report(parent_collection: &str, parent_id: &str, verdict: CoverageVerdict) -> CoverageReport {
    CoverageReport::new(CoverageReportParams {
        parent_collection: parent_collection.to_string(),
        parent_id: parent_id.to_string(),
        children_collection: "specs".to_string(),
        children_ids: vec![],
        verdict,
        gaps: if verdict == CoverageVerdict::Incomplete {
            vec![CoverageGap {
                parent_criterion: "test criterion".to_string(),
                description: "test gap".to_string(),
                severity: GapSeverity::Critical,
            }]
        } else {
            vec![]
        },
        out_of_scope: vec![],
        summary: "test summary".to_string(),
        model_used: "test".to_string(),
    })
}

#[test]
fn test_find_pending_coverage_check_no_plan() {
    let dir = TestDir::new("loopr-cov-noplan");
    let stores = test_stores(&dir);
    assert!(find_pending_coverage_check(&stores).is_none());
}

#[test]
fn test_find_pending_coverage_check_plan_with_specs_no_report() {
    let dir = TestDir::new("loopr-cov-noreport");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let check = find_pending_coverage_check(&stores);
    assert!(check.is_some(), "should need coverage check when no report exists");
    let check = check.unwrap();
    assert_eq!(check.parent_collection, "plan");
    assert_eq!(check.parent_id, plan_id);
}

#[test]
fn test_find_pending_coverage_check_complete_report() {
    let dir = TestDir::new("loopr-cov-complete");
    let stores = test_stores(&dir);

    let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
    spec.force_status(HierarchyStatus::Active);
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let report = make_coverage_report("plan", &plan_id, CoverageVerdict::Complete);
    stores
        .coverage_reports
        .write()
        .unwrap()
        .insert(report.id.clone(), report);

    // Complete report at plan level - no pending check at plan level
    let check = find_pending_coverage_check(&stores);
    // Should now check spec level (no phases, so no check needed)
    assert!(check.is_none());
}

#[test]
fn test_find_incomplete_decomposition() {
    let dir = TestDir::new("loopr-cov-incomplete");
    let stores = test_stores(&dir);
    let coord_state = CoordinatorState::new("goal-1".into(), crate::config::InterviewMode::Auto);

    let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let report = make_coverage_report("plan", &plan_id, CoverageVerdict::Incomplete);
    stores
        .coverage_reports
        .write()
        .unwrap()
        .insert(report.id.clone(), report);

    let result = find_incomplete_decomposition(&stores, &coord_state, 3);
    assert!(result.is_some());
    let result = result.unwrap();
    assert_eq!(result.parent_id, plan_id);
    assert_eq!(result.attempt_count, 0);
    assert!(!result.gap_descriptions.is_empty());
}

#[test]
fn test_is_decomposition_cap_reached() {
    let dir = TestDir::new("loopr-cov-cap");
    let stores = test_stores(&dir);
    let mut coord_state = CoordinatorState::new("goal-1".into(), crate::config::InterviewMode::Auto);

    let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
    plan.force_status(HierarchyStatus::Active);
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let report = make_coverage_report("plan", &plan_id, CoverageVerdict::Incomplete);
    stores
        .coverage_reports
        .write()
        .unwrap()
        .insert(report.id.clone(), report);

    // Not at cap yet
    assert!(is_decomposition_cap_reached(&stores, &coord_state, 3).is_none());

    // Increment to cap
    coord_state.increment_decomposition_attempts(&plan_id);
    coord_state.increment_decomposition_attempts(&plan_id);
    coord_state.increment_decomposition_attempts(&plan_id);

    let result = is_decomposition_cap_reached(&stores, &coord_state, 3);
    assert!(result.is_some());
    assert_eq!(result.unwrap(), ("plan".to_string(), plan_id));
}
