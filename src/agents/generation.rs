//! Document generation prompts for the Coordinator's Plan -> Spec -> Phase -> Work pipeline.
//!
//! The generation engine was purged in the purge-generation-engine refactor (2026-04-04).
//! Only build_work_prompt and the live query helpers remain.

use std::collections::{HashMap, HashSet};

use crate::daemon::context::Stores;
use crate::domain::phase::Phase;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::domain::spec::Spec;
use crate::domain::work::{Work, WorkStatus};

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

/// Build a Work generation prompt.
///
/// Input context:
/// - Active Phase (full `.md` content including frontmatter)
/// - Existing Works in this Phase (to avoid duplicates)
/// - Relevant learnings (scoped to Phase + Spec + Plan + Global)
/// - Codebase context (researcher findings about affected modules)
///
/// `phase_markdown_content` is the Phase's full `docs/loopr/<id>.md` file.
/// `plan_markdown_content` is the Plan's full `docs/loopr/<id>.md` file (optional).
pub fn build_work_prompt(
    phase: &Phase,
    phase_markdown_content: &str,
    existing_works: &[Work],
    work_contents: &HashMap<String, String>,
    learnings: &[String],
    findings: &[String],
    plan_markdown_content: Option<&str>,
    guidance_section: Option<&str>,
) -> GenerationPrompt {
    tracing::debug!(
        "build_work_prompt(phase_id={}, existing_works={}, learnings={}, findings={})",
        phase.id,
        existing_works.len(),
        learnings.len(),
        findings.len()
    );
    let mut msg = String::with_capacity(4096);

    msg.push_str("## Task: Generate Works\n\n");

    // Include the original plan so the LLM can see the full
    // user-agreed structure when generating work items.
    if let Some(plan_md) = plan_markdown_content.filter(|d| !d.is_empty()) {
        msg.push_str("### Original Plan (user-agreed - be faithful to this structure)\n\n");
        msg.push_str(plan_md);
        msg.push_str("\n\n");
    }

    msg.push_str("### Active Phase\n\n");
    msg.push_str(phase_markdown_content);
    msg.push_str("\n\n");

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
            let wi_content = work_contents.get(&wi.id).map(String::as_str).unwrap_or("");
            msg.push_str(&format!(
                "- ID: {} | Title: \"{}\" | Status: {} | {} — {}\n",
                wi.id,
                wi.title,
                wi.status(),
                deps,
                wi_content
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

/// Find the active Plan from stores. Returns None if no active Plan.
pub fn find_active_plan(stores: &Stores) -> Option<Plan> {
    let plans = stores.read_plans().ok()?;
    plans.values().find(|p| p.status() == HierarchyStatus::Active).cloned()
}

/// Find active Specs for a given Plan.
pub fn find_active_specs_for_plan(stores: &Stores, plan_id: &str) -> Vec<Spec> {
    let Ok(specs) = stores.read_specs() else { return vec![] };
    let mut result: Vec<_> = specs
        .values()
        .filter(|s| s.parent_id == plan_id && s.status() == HierarchyStatus::Active)
        .cloned()
        .collect();
    result.sort_by_key(|s| s.created_at);
    result
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
    result.sort_by_key(|p| p.created_at);
    result
}

/// Find all Works whose parent_id matches the given ID.
///
/// In Full mode, parent_id is a Phase ID. In Brief mode, parent_id is a Plan ID.
/// This function is parent-type-agnostic - it just matches the ID.
///
/// NOTE: could be promoted to a typed helper on a RecordId wrapper if prefix
/// logic grows, but a simple filter is sufficient for now.
pub fn find_works_for_parent(stores: &Stores, parent_id: &str) -> Vec<Work> {
    let Ok(works) = stores.read_works() else { return vec![] };
    works.values().filter(|w| w.parent_id == parent_id).cloned().collect()
}

/// Collect all Work items descended from a plan (Brief mode: direct children;
/// Full mode: via Spec -> Phase -> Work chain). Returns owned Work clones.
pub fn collect_goal_works(stores: &Stores, plan_id: &str) -> Vec<Work> {
    let Ok(works) = stores.read_works() else {
        return vec![];
    };
    let Ok(phases) = stores.read_phases() else {
        return vec![];
    };
    let Ok(specs) = stores.read_specs() else {
        return vec![];
    };

    // Collect spec IDs parented to this plan
    let spec_ids: HashSet<&str> = specs
        .values()
        .filter(|s| s.parent_id == plan_id)
        .map(|s| s.id.as_str())
        .collect();

    // Collect phase IDs parented to those specs
    let phase_ids: HashSet<&str> = phases
        .values()
        .filter(|p| spec_ids.contains(p.parent_id.as_str()))
        .map(|p| p.id.as_str())
        .collect();

    // Works under this plan: directly (Brief mode) or via phases (Full mode)
    works
        .values()
        .filter(|w| w.parent_id == plan_id || phase_ids.contains(w.parent_id.as_str()))
        .cloned()
        .collect()
}

/// Compute the fraction of abandoned works across all phases under a given plan.
/// Uses all works (including non-terminal) as denominator.
/// Used for logging and status display. Returns 0.0 if there are no works.
pub fn goal_abandon_ratio(stores: &Stores, plan_id: &str) -> f64 {
    let all_works = collect_goal_works(stores, plan_id);
    let abandoned = all_works
        .iter()
        .filter(|w| matches!(w.status(), WorkStatus::Abandoned))
        .count();
    if all_works.is_empty() {
        return 0.0;
    }
    abandoned as f64 / all_works.len() as f64
}

/// Compute the fraction of abandoned works using only terminal works (Done + Abandoned)
/// as the denominator. Used by the GoalComplete quality gate - this is the meaningful
/// measure at completion time (5/12 not 5/13 if one work is still non-terminal).
/// Returns 0.0 if there are no terminal works.
pub fn goal_abandon_ratio_terminal(stores: &Stores, plan_id: &str) -> f64 {
    let (_, _, terminal_count, abandoned_count) = goal_work_counts(stores, plan_id);
    if terminal_count == 0 {
        return 0.0;
    }
    abandoned_count as f64 / terminal_count as f64
}

/// Returns (done_count, total_all, terminal_count, abandoned_count) for works under a plan.
pub fn goal_work_counts(stores: &Stores, plan_id: &str) -> (usize, usize, usize, usize) {
    let all_works = collect_goal_works(stores, plan_id);
    let done = all_works
        .iter()
        .filter(|w| matches!(w.status(), WorkStatus::Done))
        .count();
    let abandoned = all_works
        .iter()
        .filter(|w| matches!(w.status(), WorkStatus::Abandoned))
        .count();
    let terminal = done + abandoned;
    (done, all_works.len(), terminal, abandoned)
}

pub fn is_phase_complete(stores: &Stores, phase_id: &str) -> bool {
    let Ok(works) = stores.read_works() else { return false };
    let phase_wis: Vec<_> = works.values().filter(|w| w.parent_id == phase_id).collect();
    !phase_wis.is_empty()
        && phase_wis
            .iter()
            .all(|w| matches!(w.status(), WorkStatus::Done | WorkStatus::Abandoned))
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

        let mut plan = Plan::new("Active".into(), "crit".into());
        plan.force_status(HierarchyStatus::Active);
        stores.plans.write().unwrap().insert(plan.id.clone(), plan.clone());

        let found = find_active_plan(&stores).unwrap();
        assert_eq!(found.id, plan.id);
    }

    #[test]
    fn test_find_active_plan_skips_draft() {
        let dir = TestDir::new("loopr-gen-fap-skip");
        let stores = test_stores(&dir);

        let plan = Plan::new("Draft".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert!(find_active_plan(&stores).is_none());
    }

    #[test]
    fn test_find_active_specs_for_plan() {
        let dir = TestDir::new("loopr-gen-fasp");
        let stores = test_stores(&dir);

        let mut spec1 = Spec::new("plan-1".into(), "Active Spec".into());
        spec1.force_status(HierarchyStatus::Active);
        stores.specs.write().unwrap().insert(spec1.id.clone(), spec1);

        let spec2 = Spec::new("plan-1".into(), "Draft Spec".into());
        stores.specs.write().unwrap().insert(spec2.id.clone(), spec2);

        let mut spec3 = Spec::new("plan-2".into(), "Other Plan Spec".into());
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

        // Create p1 first so it gets an earlier created_at (sorted by created_at)
        let mut p1 = Phase::new("spec-1".into(), "Phase 1".into());
        p1.force_status(HierarchyStatus::Active);
        p1.created_at = 1000;
        stores.phases.write().unwrap().insert(p1.id.clone(), p1);

        let mut p2 = Phase::new("spec-1".into(), "Phase 2".into());
        p2.force_status(HierarchyStatus::Active);
        p2.created_at = 2000;
        stores.phases.write().unwrap().insert(p2.id.clone(), p2);

        let phases = find_active_phases_for_spec(&stores, "spec-1");
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].title, "Phase 1");
        assert_eq!(phases[1].title, "Phase 2");
    }

    #[test]
    fn test_find_works_for_parent() {
        let dir = TestDir::new("loopr-gen-fwip");
        let stores = test_stores(&dir);

        let wi1 = Work::new("phase-1".into(), "WI 1".into());
        let wi2 = Work::new("phase-1".into(), "WI 2".into());
        let wi3 = Work::new("phase-2".into(), "WI 3".into());
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);
        stores.works.write().unwrap().insert(wi3.id.clone(), wi3);

        let wis = find_works_for_parent(&stores, "phase-1");
        assert_eq!(wis.len(), 2);
    }

    #[test]
    fn test_find_works_for_parent_ordering() {
        let dir = TestDir::new("loopr-gen-fwip-ord");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-x".into(), "WI A".into());
        wi1.force_status(WorkStatus::Done);
        let mut wi2 = Work::new("phase-x".into(), "WI B".into());
        wi2.force_status(WorkStatus::InProgress);
        let wi3 = Work::new("phase-x".into(), "WI C".into());
        let wi_other = Work::new("phase-y".into(), "WI Other".into());

        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);
        stores.works.write().unwrap().insert(wi3.id.clone(), wi3);
        stores.works.write().unwrap().insert(wi_other.id.clone(), wi_other);

        let wis = find_works_for_parent(&stores, "phase-x");
        assert_eq!(wis.len(), 3);
        assert!(wis.iter().all(|w| w.parent_id == "phase-x"));
    }

    #[test]
    fn test_is_phase_complete_true() {
        let dir = TestDir::new("loopr-gen-ipc-true");
        let stores = test_stores(&dir);

        let mut wi = Work::new("phase-1".into(), "WI".into());
        wi.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert!(is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_not_done() {
        let dir = TestDir::new("loopr-gen-ipc-false");
        let stores = test_stores(&dir);

        let wi = Work::new("phase-1".into(), "WI".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_no_wis() {
        let dir = TestDir::new("loopr-gen-ipc-empty");
        let stores = test_stores(&dir);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_true_with_abandoned() {
        let dir = TestDir::new("loopr-gen-ipc-aband");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-1".into(), "WI Done".into());
        wi1.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

        let mut wi2 = Work::new("phase-1".into(), "WI Abandoned".into());
        wi2.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        assert!(is_phase_complete(&stores, "phase-1"));
    }

    #[test]
    fn test_is_phase_complete_false_mixed_nonterminal() {
        let dir = TestDir::new("loopr-gen-ipc-mixed");
        let stores = test_stores(&dir);

        let mut wi1 = Work::new("phase-1".into(), "WI Done".into());
        wi1.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

        let wi2 = Work::new("phase-1".into(), "WI InProgress".into());
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        assert!(!is_phase_complete(&stores, "phase-1"));
    }

    // --- goal_abandon_ratio tests ---

    #[test]
    fn test_goal_abandon_ratio_empty() {
        let dir = TestDir::new("loopr-gen-gar-empty");
        let stores = test_stores(&dir);
        assert_eq!(goal_abandon_ratio(&stores, "pl-fake"), 0.0);
    }

    #[test]
    fn test_goal_abandon_ratio_brief_mode_all_done() {
        let dir = TestDir::new("loopr-gen-gar-brief-done");
        let stores = test_stores(&dir);

        // Brief mode: works parented directly to plan
        let mut w1 = Work::new("plan-1".into(), "W1".into());
        w1.force_status(WorkStatus::Done);
        let mut w2 = Work::new("plan-1".into(), "W2".into());
        w2.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w1.id.clone(), w1);
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        assert_eq!(goal_abandon_ratio(&stores, "plan-1"), 0.0);
    }

    #[test]
    fn test_goal_abandon_ratio_full_mode_partial_abandon() {
        let dir = TestDir::new("loopr-gen-gar-full-partial");
        let stores = test_stores(&dir);

        // Full mode: plan -> spec -> phase -> works
        let spec = Spec::new("plan-1".into(), "Spec".into());
        let phase = Phase::new(spec.id.clone(), "Phase".into());
        let phase_id = phase.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let mut w1 = Work::new(phase_id.clone(), "W1".into());
        w1.force_status(WorkStatus::Done);
        let mut w2 = Work::new(phase_id.clone(), "W2".into());
        w2.force_status(WorkStatus::Abandoned);
        let mut w3 = Work::new(phase_id.clone(), "W3".into());
        w3.force_status(WorkStatus::Done);
        stores.works.write().unwrap().insert(w1.id.clone(), w1);
        stores.works.write().unwrap().insert(w2.id.clone(), w2);
        stores.works.write().unwrap().insert(w3.id.clone(), w3);

        let ratio = goal_abandon_ratio(&stores, "plan-1");
        assert!((ratio - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn test_goal_abandon_ratio_all_abandoned() {
        let dir = TestDir::new("loopr-gen-gar-all-aband");
        let stores = test_stores(&dir);

        // Brief mode: all works abandoned
        let mut w1 = Work::new("plan-x".into(), "W1".into());
        w1.force_status(WorkStatus::Abandoned);
        let mut w2 = Work::new("plan-x".into(), "W2".into());
        w2.force_status(WorkStatus::Abandoned);
        stores.works.write().unwrap().insert(w1.id.clone(), w1);
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        assert_eq!(goal_abandon_ratio(&stores, "plan-x"), 1.0);
    }
}
