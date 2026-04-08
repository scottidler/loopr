use super::*;
use crate::config::{Config, ProjectConfig, PromotionPolicy, ToolEntry};
use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::learning::Learning;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::spec::Spec;
use crate::domain::work::Work;
use crate::test_util::TestDir;
use crate::tools::ToolRunner;
use std::sync::{Arc, Mutex as StdMutex};
use taskstore::Store;

fn make_learning(source_id: &str, scope: LearningScope, content: &str) -> Learning {
    Learning::new(source_id.to_string(), scope, content.to_string())
}

fn make_learning_with_role(source_id: &str, scope: LearningScope, content: &str, roles: Vec<Role>) -> Learning {
    let mut l = make_learning(source_id, scope, content);
    l.applicable_roles = Some(roles);
    l
}

fn make_learning_with_confidence(source_id: &str, scope: LearningScope, content: &str, confidence: f32) -> Learning {
    let mut l = make_learning(source_id, scope, content);
    l.confidence = confidence;
    l
}

fn to_map(learnings: Vec<Learning>) -> HashMap<String, Learning> {
    learnings.into_iter().map(|l| (l.id.clone(), l)).collect()
}

// --- select_learnings: Basic scope filtering ---

#[test]
fn test_select_by_scope_work() {
    let l = make_learning("wi-1", LearningScope::Work, "insight");
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "insight");
}

#[test]
fn test_select_by_scope_chain() {
    let l1 = make_learning("wi-1", LearningScope::Work, "wi insight");
    let l2 = make_learning("phase-1", LearningScope::Phase, "phase insight");
    let l3 = make_learning("spec-1", LearningScope::Spec, "spec insight");
    let l4 = make_learning("plan-1", LearningScope::Plan, "plan insight");
    let map = to_map(vec![l1, l2, l3, l4]);

    let scope_ids = [
        ("wi-1", LearningScope::Work),
        ("phase-1", LearningScope::Phase),
        ("spec-1", LearningScope::Spec),
        ("plan-1", LearningScope::Plan),
    ];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 4);
}

#[test]
fn test_select_global_always_included() {
    let l = make_learning("global", LearningScope::Global, "global insight");
    let map = to_map(vec![l]);

    // Empty scope chain — only Global should match
    let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "global insight");
}

#[test]
fn test_select_excludes_unrelated_scope() {
    let l1 = make_learning("wi-1", LearningScope::Work, "relevant");
    let l2 = make_learning("wi-999", LearningScope::Work, "unrelated");
    let map = to_map(vec![l1, l2]);

    let scope_ids = [("wi-1", LearningScope::Work)];
    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "relevant");
}

// --- select_learnings: Role filtering ---

#[test]
fn test_select_by_role_match() {
    let l = make_learning_with_role("wi-1", LearningScope::Work, "impl insight", vec![Role::Implementer]);
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_select_by_role_mismatch() {
    let l = make_learning_with_role("wi-1", LearningScope::Work, "reviewer only", vec![Role::Reviewer]);
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 0);
}

#[test]
fn test_select_none_roles_applies_to_all() {
    let l = make_learning("wi-1", LearningScope::Work, "universal");
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    for role in [
        Role::Implementer,
        Role::Reviewer,
        Role::Coordinator,
        Role::Researcher,
        Role::Integrator,
    ] {
        let result = select_learnings(&map, &scope_ids, role, 0.0, 20);
        assert_eq!(result.len(), 1, "should apply to role {role}");
    }
}

// --- select_learnings: Confidence filtering ---

#[test]
fn test_select_above_confidence_threshold() {
    let l = make_learning_with_confidence("wi-1", LearningScope::Work, "high conf", 0.8);
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_select_below_confidence_threshold() {
    let l = make_learning_with_confidence("wi-1", LearningScope::Work, "low conf", 0.1);
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
    assert_eq!(result.len(), 0);
}

#[test]
fn test_select_promoted_always_included_regardless_of_confidence() {
    let mut l = make_learning_with_confidence("wi-1", LearningScope::Work, "policy", 0.1);
    l.promoted = true;
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.9, 20);
    assert_eq!(result.len(), 1);
    assert!(result[0].promoted);
}

// --- select_learnings: Sorting ---

#[test]
fn test_sort_promoted_first() {
    let mut l1 = make_learning_with_confidence("wi-1", LearningScope::Work, "normal", 0.9);
    l1.updated_at = 1000;
    let mut l2 = make_learning_with_confidence("wi-1", LearningScope::Work, "policy", 0.5);
    l2.promoted = true;
    l2.updated_at = 500;
    let map = to_map(vec![l1, l2]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 2);
    assert!(result[0].promoted, "promoted should come first");
    assert!(!result[1].promoted);
}

#[test]
fn test_sort_by_confidence_desc() {
    let mut l1 = make_learning_with_confidence("wi-1", LearningScope::Work, "low", 0.3);
    l1.updated_at = 1000;
    let mut l2 = make_learning_with_confidence("wi-1", LearningScope::Work, "high", 0.9);
    l2.updated_at = 1000;
    let map = to_map(vec![l1, l2]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 2);
    assert!(result[0].confidence > result[1].confidence);
}

#[test]
fn test_sort_by_recency_desc() {
    let mut l1 = make_learning_with_confidence("wi-1", LearningScope::Work, "older", 0.5);
    l1.updated_at = 1000;
    let mut l2 = make_learning_with_confidence("wi-1", LearningScope::Work, "newer", 0.5);
    l2.updated_at = 2000;
    let map = to_map(vec![l1, l2]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 2);
    assert!(result[0].updated_at > result[1].updated_at);
}

// --- select_learnings: Truncation ---

#[test]
fn test_max_count_truncation() {
    let learnings: Vec<Learning> = (0..30)
        .map(|i| make_learning("wi-1", LearningScope::Work, &format!("insight {i}")))
        .collect();
    let map = to_map(learnings);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 10);
    assert_eq!(result.len(), 10);
}

#[test]
fn test_fewer_than_max_count() {
    let l = make_learning("wi-1", LearningScope::Work, "only one");
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
}

// --- select_learnings: Empty inputs ---

#[test]
fn test_empty_learnings() {
    let map = HashMap::new();
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert!(result.is_empty());
}

#[test]
fn test_empty_scope_ids_only_global() {
    let l1 = make_learning("wi-1", LearningScope::Work, "scoped");
    let l2 = make_learning("global", LearningScope::Global, "global");
    let map = to_map(vec![l1, l2]);

    let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].scope, LearningScope::Global);
}

// --- select_learnings: Combined filtering ---

#[test]
fn test_combined_scope_role_confidence() {
    let l1 = make_learning_with_confidence("wi-1", LearningScope::Work, "good", 0.8);
    let mut l2 = make_learning_with_confidence("wi-1", LearningScope::Work, "wrong role", 0.8);
    l2.applicable_roles = Some(vec![Role::Reviewer]);
    let l3 = make_learning_with_confidence("wi-1", LearningScope::Work, "low conf", 0.1);
    let l4 = make_learning_with_confidence("wi-999", LearningScope::Work, "wrong scope", 0.8);

    let map = to_map(vec![l1, l2, l3, l4]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].content, "good");
}

#[test]
fn test_auto_promoted_learning_sorted_first() {
    let policy = PromotionPolicy {
        min_reinforcements: 2,
        max_age_days: 30,
        auto_promote: true,
    };
    let mut l1 = make_learning_with_confidence("wi-1", LearningScope::Work, "unpromoted", 0.9);
    l1.updated_at = 2000;
    let mut l2 = make_learning_with_confidence("wi-1", LearningScope::Work, "promoted", 0.7);
    l2.reinforce(&policy);
    l2.reinforce(&policy);
    l2.updated_at = 1000;
    assert!(l2.promoted, "should be auto-promoted after 2 reinforcements");

    let map = to_map(vec![l1, l2]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
    assert_eq!(result.len(), 2);
    assert!(result[0].promoted, "promoted should be first");
    assert_eq!(result[0].content, "promoted");
}

// --- select_learnings: Default confidence ---

#[test]
fn test_default_confidence_passes_standard_threshold() {
    let l = make_learning("wi-1", LearningScope::Work, "new insight");
    assert!((l.confidence - 0.5).abs() < f32::EPSILON);
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
    assert_eq!(result.len(), 1);
}

#[test]
fn test_zero_max_count_returns_empty() {
    let l = make_learning("wi-1", LearningScope::Work, "insight");
    let map = to_map(vec![l]);
    let scope_ids = [("wi-1", LearningScope::Work)];

    let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 0);
    assert!(result.is_empty());
}

// =====================================================
// Token estimation tests
// =====================================================

#[test]
fn test_estimate_tokens_empty() {
    assert_eq!(estimate_tokens(""), 0);
}

#[test]
fn test_estimate_tokens_short() {
    // "hello" = 5 chars → (5+3)/4 = 2 tokens
    assert_eq!(estimate_tokens("hello"), 2);
}

#[test]
fn test_estimate_tokens_exact_boundary() {
    // 8 chars → (8+3)/4 = 2 tokens
    assert_eq!(estimate_tokens("abcdefgh"), 2);
}

#[test]
fn test_estimate_tokens_longer() {
    let text = "a".repeat(400);
    // 400 chars → (400+3)/4 = 100 tokens
    assert_eq!(estimate_tokens(&text), 100);
}

// =====================================================
// Truncation tests
// =====================================================

#[test]
fn test_truncate_prose_no_truncation() {
    let text = "Short text.";
    assert_eq!(truncate_prose(text, 100), "Short text.");
}

#[test]
fn test_truncate_prose_at_sentence() {
    // 2 tokens = 8 chars max. "First. Second." = 15 chars, won't fit.
    let text = "First. Second sentence here.";
    let result = truncate_prose(text, 5);
    // 5 tokens = 20 chars. Text is 28 chars. Slice = first 20 = "First. Second senten"
    // rfind(". ") in "First. Second senten" → position 5 ("First. ")
    assert!(result.contains("First."));
    assert!(result.contains("[truncated]"));
}

#[test]
fn test_truncate_prose_at_newline() {
    let text = "Line one\nLine two is much longer and will exceed the budget";
    let result = truncate_prose(text, 5);
    // 5 tokens = 20 chars. Slice = "Line one\nLine two is"
    // No ". " found, rfind('\n') at position 8
    assert!(result.contains("Line one\n"));
    assert!(result.contains("[truncated]"));
}

#[test]
fn test_truncate_from_head_no_truncation() {
    let text = "Short text.";
    assert_eq!(truncate_from_head(text, 100), "Short text.");
}

#[test]
fn test_truncate_from_head_keeps_tail() {
    // Build text where oldest iterations should be dropped
    let text = "--- Iteration 1 ---\nread Cargo.toml\n--- Iteration 2 ---\nwrote src/main.rs\n--- Iteration 3 ---\nran tests (pass)";
    // 5 tokens = 20 chars. Text is 107 chars. Should keep last 20 chars.
    let result = truncate_from_head(text, 5);
    assert!(result.contains("[earlier iterations truncated]"));
    // Should keep the newest content (tail)
    assert!(result.contains("ran tests (pass)"));
    // Should NOT contain oldest content
    assert!(!result.contains("Iteration 1"));
}

#[test]
fn test_truncate_from_head_at_newline() {
    let text = "old content\nnew content that is very important";
    // 5 tokens = 20 chars. Text is 47 chars. start = 47-20 = 27.
    // text[27..] = "s very important". No newline found.
    // So it falls back to "[earlier iterations truncated] s very important"
    let result = truncate_from_head(text, 5);
    assert!(result.starts_with("[earlier iterations truncated]"));
}

#[test]
fn test_truncate_from_head_clean_break() {
    // Ensure it finds a newline boundary when available
    let text = "aaaa\nbbbb\ncccc\ndddd\neeee\nffff\ngggg";
    // 5 tokens = 20 chars. text.len() = 34. start = 14.
    // text[14..] = "\ndddd\neeee\nffff\ngggg". find('\n') at 0.
    // Result: "[earlier iterations truncated]\ndddd\neeee\nffff\ngggg"
    let result = truncate_from_head(text, 5);
    assert!(result.contains("[earlier iterations truncated]"));
    assert!(result.contains("gggg"));
    assert!(!result.contains("aaaa"));
}

#[test]
fn test_truncate_list_empty() {
    let items: Vec<String> = vec![];
    let result = truncate_list(&items, 100);
    assert!(result.is_empty());
}

#[test]
fn test_truncate_list_all_fit() {
    let items = vec!["short".to_string(), "items".to_string()];
    let result = truncate_list(&items, 100);
    assert_eq!(result.len(), 2);
}

#[test]
fn test_truncate_list_exceeds_budget() {
    let items: Vec<String> = (0..20)
        .map(|i| format!("This is learning item number {i} with some extra text"))
        .collect();
    // Each item is ~50 chars = ~13 tokens + 1 = 14 tokens per item
    // Budget of 30 tokens should fit about 2 items
    let result = truncate_list(&items, 30);
    assert!(result.len() < items.len());
    assert!(!result.is_empty());
}

// =====================================================
// TokenBudget tests
// =====================================================

#[test]
fn test_token_budget_for_implementer() {
    let budget = TokenBudget::for_role(Role::Implementer);
    assert_eq!(budget.learnings, 2000);
    assert_eq!(budget.tools_or_actions, 500);
    assert_eq!(budget.previous_summary, 4000);
}

#[test]
fn test_token_budget_for_reviewer() {
    let budget = TokenBudget::for_role(Role::Reviewer);
    assert_eq!(budget.learnings, 2000);
    assert_eq!(budget.tools_or_actions, 0);
    assert_eq!(budget.previous_summary, 0);
}

#[test]
fn test_token_budget_for_coordinator() {
    let budget = TokenBudget::for_role(Role::Coordinator);
    assert!(budget.state_summary > 0);
    assert!(budget.learnings > 0);
}

#[test]
fn test_token_budget_for_researcher() {
    let budget = TokenBudget::for_role(Role::Researcher);
    assert!(budget.learnings > 0);
}

#[test]
fn test_token_budget_for_integrator() {
    let budget = TokenBudget::for_role(Role::Integrator);
    assert!(budget.state_summary > 0);
}

// =====================================================
// ContextBuilder tests
// =====================================================

fn setup_stores(dir: &std::path::Path) -> (Stores, String) {
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

    let plan = Plan::new("Test Plan".into(), "criteria".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let spec = Spec::new(plan_id, "Test Spec".into());
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let phase = Phase::new(spec_id, "Test Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    let wi = Work::new(phase_id.clone(), "Test Work".into());
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let learning = Learning::new(
        phase_id,
        LearningScope::Phase,
        "Previous iteration found a bug in parsing".into(),
    );
    stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

    (stores, wi_id)
}

fn setup_stores_with_bundle(dir: &std::path::Path) -> (Stores, String, String) {
    let (stores, wi_id) = setup_stores(dir);

    let mut bundle = Bundle::new(
        wi_id.clone(),
        Some("tick-001".into()),
        "feature/test".into(),
        vec!["Added test module with basic functionality".into()],
    );
    bundle.force_status(BundleStatus::Triaged);
    bundle.touched_paths = vec!["src/test.rs".into(), "src/main.rs".into()];
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    (stores, wi_id, bundle_id)
}

#[test]
fn test_context_builder_load_work_hierarchy() {
    let dir = TestDir::new("loopr-ctx-hier");
    let (stores, wi_id) = setup_stores(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap();

    assert_eq!(builder.work_title(), Some("Test Work"));
    assert!(builder.plan.is_some());
    assert!(builder.spec.is_some());
    assert!(builder.phase.is_some());
    assert!(builder.work.is_some());
    assert_eq!(builder.scope_ids.len(), 4);
}

#[test]
fn test_context_builder_load_missing_work() {
    let dir = TestDir::new("loopr-ctx-miss");
    let (stores, _) = setup_stores(&dir);

    let result = ContextBuilder::new(&stores, Role::Implementer).load_work_hierarchy("nonexistent");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("work not found"));
}

#[test]
fn test_context_builder_load_bundle_hierarchy() {
    let dir = TestDir::new("loopr-ctx-bundle");
    let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

    let builder = ContextBuilder::new(&stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)
        .unwrap();

    assert_eq!(builder.work_title(), Some("Test Work"));
    assert!(builder.bundle_info.is_some());
    let (bid, _, paths) = builder.bundle_info.as_ref().unwrap();
    assert_eq!(bid, &bundle_id);
    assert_eq!(paths.len(), 2);
}

#[test]
fn test_context_builder_load_missing_bundle() {
    let dir = TestDir::new("loopr-ctx-missbdl");
    let (stores, _) = setup_stores(&dir);

    let result = ContextBuilder::new(&stores, Role::Reviewer).load_bundle_hierarchy("nonexistent");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("bundle not found"));
}

#[test]
fn test_context_builder_build_implementer() {
    let dir = TestDir::new("loopr-ctx-build");
    let (stores, wi_id) = setup_stores(&dir);
    let tool_runner = ToolRunner::new(&[ToolEntry {
        name: "test".into(),
        command: "echo ok".into(),
        timeout_secs: 10,
        worktree: true,
    }]);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_tools(&tool_runner)
        .with_iteration(1)
        .with_footer("Implement the Work described above.".to_string());

    let assembled = builder.build("You are an Implementer.").unwrap();
    assert!(assembled.user_message.contains("Test Plan"));
    assert!(assembled.user_message.contains("Test Spec"));
    assert!(assembled.user_message.contains("Test Phase"));
    assert!(assembled.user_message.contains("Test Work"));
    assert!(assembled.user_message.contains("`test`"));
    assert!(assembled.user_message.contains("Current Iteration: 1"));
    assert!(assembled.user_message.contains("Implement the Work"));
    assert!(assembled.user_message.contains("Learnings"));
    assert_eq!(assembled.system_prompt, "You are an Implementer.");
    assert!(assembled.token_estimate > 0);
}

#[test]
fn test_context_builder_build_reviewer() {
    let dir = TestDir::new("loopr-ctx-rev");
    let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

    let builder = ContextBuilder::new(&stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)
        .unwrap()
        .with_footer("Review this Bundle.".to_string());

    let assembled = builder.build("You are a Reviewer.").unwrap();
    assert!(assembled.user_message.contains("Test Plan"));
    assert!(assembled.user_message.contains("Test Work"));
    assert!(assembled.user_message.contains("Bundle Under Review"));
    assert!(assembled.user_message.contains(&bundle_id));
    assert!(assembled.user_message.contains("`src/test.rs`"));
    assert!(assembled.user_message.contains("Review this Bundle."));
}

#[test]
fn test_context_builder_with_previous_summary() {
    let dir = TestDir::new("loopr-ctx-prev");
    let (stores, wi_id) = setup_stores(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_previous_summary(Some("Last iteration added error types".into()))
        .with_iteration(3);

    let assembled = builder.build("system").unwrap();
    assert!(assembled.user_message.contains("Previous Iteration Summary"));
    assert!(assembled.user_message.contains("Last iteration added error types"));
    assert!(assembled.user_message.contains("Current Iteration: 3"));
}

#[test]
fn test_context_builder_with_staleness() {
    let dir = TestDir::new("loopr-ctx-stale");
    let (stores, wi_id) = setup_stores(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_staleness_note(Some("A new Tick 'tick-99' has been published.".into()))
        .with_iteration(2);

    let assembled = builder.build("system").unwrap();
    assert!(assembled.user_message.contains("Staleness Warning"));
    assert!(assembled.user_message.contains("tick-99"));
}

#[test]
fn test_context_builder_no_learnings_section_when_empty() {
    let dir = TestDir::new("loopr-ctx-nolearn");
    let (stores, wi_id) = setup_stores(&dir);

    // Clear all learnings
    stores.learnings.write().unwrap().clear();

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap();

    let assembled = builder.build("system").unwrap();
    assert!(!assembled.user_message.contains("Learnings"));
}

#[test]
fn test_context_builder_no_tools_section_when_empty() {
    let dir = TestDir::new("loopr-ctx-notool");
    let (stores, wi_id) = setup_stores(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap();
    // No .with_tools() call

    let assembled = builder.build("system").unwrap();
    assert!(!assembled.user_message.contains("Available Tools"));
}

#[test]
fn test_context_builder_reviewer_no_previous_summary() {
    let dir = TestDir::new("loopr-ctx-revnp");
    let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

    // Reviewer budget has previous_summary = 0, so even if set it shouldn't appear
    let builder = ContextBuilder::new(&stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)
        .unwrap()
        .with_previous_summary(Some("This should not appear".into()));

    let assembled = builder.build("system").unwrap();
    assert!(!assembled.user_message.contains("Previous Iteration Summary"));
}

#[test]
fn test_assembled_context_token_estimate() {
    let dir = TestDir::new("loopr-ctx-tokens");
    let (stores, wi_id) = setup_stores(&dir);

    let assembled = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .build("system prompt")
        .unwrap();

    assert!(assembled.token_estimate > 0);
    assert!(!assembled.user_message.is_empty());
}

// =====================================================
// Guidance injection tests
// =====================================================

#[test]
fn test_context_builder_with_guidance_includes_schema_docs() {
    let dir = TestDir::new("loopr-ctx-guid");
    let (stores, wi_id) = setup_stores(&dir);

    let guidance = crate::guidance::AgentGuidance::schema_only();

    let assembled = ContextBuilder::new(&stores, Role::Coordinator)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_guidance(&guidance)
        .build("system")
        .unwrap();

    // Schema docs should appear in the assembled user_message
    assert!(
        assembled.user_message.contains("## Work Status Transitions"),
        "Assembled context missing work transitions"
    );
    assert!(
        assembled.user_message.contains("## Bundle Status Transitions"),
        "Assembled context missing bundle transitions"
    );
    assert!(
        assembled.user_message.contains("## Plan/Spec/Phase Status Transitions"),
        "Assembled context missing hierarchy transitions"
    );
    assert!(
        assembled.user_message.contains("Terminal states:"),
        "Assembled context missing terminal state annotations"
    );
}

#[test]
fn test_context_builder_guidance_contains_role_specific_transitions() {
    let dir = TestDir::new("loopr-ctx-guid-role");
    let (stores, wi_id) = setup_stores(&dir);

    let guidance = crate::guidance::AgentGuidance::schema_only();

    // Coordinator should see Draft → Ready
    let coord = ContextBuilder::new(&stores, Role::Coordinator)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_guidance(&guidance)
        .build("system")
        .unwrap();
    assert!(
        coord.user_message.contains("Draft → Ready"),
        "Coordinator context missing Draft → Ready"
    );

    // Implementer should NOT see Draft → Ready (Coordinator-only)
    let impl_ctx = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_guidance(&guidance)
        .build("system")
        .unwrap();
    assert!(
        !impl_ctx.user_message.contains("Draft → Ready"),
        "Implementer context should not contain Draft → Ready"
    );
    // Implementer should see InProgress → InReview
    assert!(
        impl_ctx.user_message.contains("InProgress → InReview"),
        "Implementer context missing InProgress → InReview"
    );
}

#[test]
fn test_context_builder_guidance_with_loopr_md() {
    let dir = TestDir::new("loopr-ctx-guid-md");
    let (stores, wi_id) = setup_stores(&dir);

    let mut guidance = crate::guidance::AgentGuidance::schema_only();
    guidance.global_md = Some("Always use ES modules".to_string());
    guidance.project_md = Some("Use rspec, not minitest".to_string());

    let assembled = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_guidance(&guidance)
        .build("system")
        .unwrap();

    assert!(
        assembled.user_message.contains("Always use ES modules"),
        "Assembled context missing global LOOPR.md content"
    );
    assert!(
        assembled.user_message.contains("Use rspec, not minitest"),
        "Assembled context missing project LOOPR.md content"
    );
}

#[test]
fn test_context_builder_guidance_appears_before_hierarchy() {
    let dir = TestDir::new("loopr-ctx-guid-order");
    let (stores, wi_id) = setup_stores(&dir);

    let guidance = crate::guidance::AgentGuidance::schema_only();

    let assembled = ContextBuilder::new(&stores, Role::Coordinator)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_guidance(&guidance)
        .build("system")
        .unwrap();

    let guidance_pos = assembled
        .user_message
        .find("## Work Status Transitions")
        .expect("guidance section not found");
    let assignment_pos = assembled
        .user_message
        .find("## Your Assignment")
        .expect("assignment section not found");

    assert!(
        guidance_pos < assignment_pos,
        "Guidance (pos {}) should appear before Your Assignment (pos {})",
        guidance_pos,
        assignment_pos
    );
}

#[test]
fn test_context_builder_reads_work_doc_from_filesystem() {
    let dir = TestDir::new("loopr-ctx-workdoc");
    let (stores, wi_id) = setup_stores(&dir);

    // Pre-create the work doc on disk - ContextBuilder must read this and inject it
    let docs_dir = dir.join("docs/loopr");
    std::fs::create_dir_all(&docs_dir).unwrap();
    let unique_sentinel = "SENTINEL_CONTENT_FROM_DISK_7x9q";
    let work_doc_path = docs_dir.join(format!("{}.md", wi_id));
    std::fs::write(
        &work_doc_path,
        format!("---\nid: {}\n---\n\n{}\n", wi_id, unique_sentinel),
    )
    .unwrap();

    let assembled = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .build("system")
        .unwrap();

    // The unique sentinel must appear verbatim in the user message - proving the file was read
    assert!(
        assembled.user_message.contains(unique_sentinel),
        "Work doc content from disk must appear in context; user_message:\n{}",
        assembled.user_message
    );
    // And it must be under ## Your Assignment
    let assignment_pos = assembled
        .user_message
        .find("## Your Assignment")
        .expect("## Your Assignment section must be present");
    let sentinel_pos = assembled
        .user_message
        .find(unique_sentinel)
        .expect("sentinel must be present");
    assert!(
        sentinel_pos > assignment_pos,
        "sentinel (pos {}) must appear after ## Your Assignment (pos {})",
        sentinel_pos,
        assignment_pos
    );
}

#[test]
fn test_context_builder_no_guidance_when_not_set() {
    let dir = TestDir::new("loopr-ctx-noguid");
    let (stores, wi_id) = setup_stores(&dir);

    // No with_guidance() call
    let assembled = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .build("system")
        .unwrap();

    assert!(
        !assembled.user_message.contains("## Work Status Transitions"),
        "Guidance should not appear when with_guidance() is not called"
    );
}

#[test]
fn test_context_builder_noop_bundle_injects_directive() {
    let dir = TestDir::new("loopr-ctx-noop");
    let (stores, wi_id) = setup_stores(&dir);

    // Create a noop bundle with noop_reason set
    let mut bundle = Bundle::new(
        wi_id.clone(),
        None,
        String::new(), // empty branch for noop
        vec!["criteria already met".into()],
    );
    bundle.force_status(BundleStatus::Triaged);
    bundle.noop_reason = Some("Phase 1 already added Tailwind styling".to_string());
    bundle.touched_paths = vec!["src/main.rs".into()];
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    // Create the file so the context builder can read it
    let repo_path = &stores.config.project.repo_path;
    std::fs::create_dir_all(repo_path.join("src")).unwrap();
    std::fs::write(repo_path.join("src/main.rs"), "fn main() {}").unwrap();

    let builder = ContextBuilder::new(&stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)
        .unwrap();

    assert!(builder.bundle_noop_reason.is_some());
    assert!(builder.bundle_diff.is_none(), "noop bundle should not have a diff");
    assert!(
        builder.noop_file_contents.is_some(),
        "noop bundle should have file contents"
    );

    let assembled = builder.build("You are a Reviewer.").unwrap();
    assert!(
        assembled.user_message.contains("NO-OP BUNDLE"),
        "should contain noop directive"
    );
    assert!(
        assembled.user_message.contains("Phase 1 already added Tailwind"),
        "should contain noop reason"
    );
    assert!(
        assembled.user_message.contains("fn main()"),
        "should contain file contents from repo"
    );
    assert!(
        !assembled.user_message.contains("Code Changes"),
        "should NOT contain diff section"
    );
}

#[test]
fn test_context_builder_normal_bundle_no_noop_directive() {
    let dir = TestDir::new("loopr-ctx-nonnoop");
    let (stores, _, bundle_id) = setup_stores_with_bundle(&dir);

    let builder = ContextBuilder::new(&stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)
        .unwrap();

    assert!(builder.bundle_noop_reason.is_none());
    assert!(builder.noop_file_contents.is_none());

    let assembled = builder.build("You are a Reviewer.").unwrap();
    assert!(
        !assembled.user_message.contains("NO-OP BUNDLE"),
        "normal bundle should NOT contain noop directive"
    );
}

// --- Work enrichment: acceptance_criteria, resource_tags, dependencies ---

fn setup_stores_with_enrichment(dir: &std::path::Path) -> (Stores, String, String) {
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

    let plan = Plan::new("Test Plan".into(), "criteria".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let spec = Spec::new(plan_id, "Test Spec".into());
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let phase = Phase::new(spec_id, "Test Phase".into(), 1);
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    // Dependency work (already done)
    let mut dep_work = Work::new(phase_id.clone(), "Create model".into());
    dep_work.resource_tags = vec!["src/model.rs".into()];
    dep_work.force_status(crate::domain::work::WorkStatus::Done);
    let dep_id = dep_work.id.clone();
    stores.works.write().unwrap().insert(dep_work.id.clone(), dep_work);

    // Main work (depends on dep_work)
    let mut wi = Work::new(phase_id, "Write tests".into());
    wi.resource_tags = vec!["tests/model.rs".into()];
    wi.acceptance_criteria =
        crate::domain::criteria::AcceptanceCriteria(vec!["All tests pass".into(), "Coverage above 80%".into()]);
    wi.dependencies = vec![dep_id.clone()];
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    (stores, wi_id, dep_id)
}

#[test]
fn test_load_work_hierarchy_enrichment() {
    let dir = TestDir::new("loopr-ctx-enrich");
    let (stores, wi_id, dep_id) = setup_stores_with_enrichment(&dir);
    let _ = dep_id;

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap();

    assert_eq!(
        builder.work_acceptance_criteria,
        vec!["All tests pass", "Coverage above 80%"]
    );
    assert_eq!(builder.work_resource_tags, vec!["tests/model.rs"]);
    assert_eq!(builder.dependency_summaries.len(), 1);
    assert_eq!(builder.dependency_summaries[0].title, "Create model");
    assert_eq!(builder.dependency_summaries[0].status, "Done");
    assert_eq!(builder.dependency_summaries[0].resource_tags, vec!["src/model.rs"]);
}

#[test]
fn test_build_renders_enrichment() {
    let dir = TestDir::new("loopr-ctx-enrich-build");
    let (stores, wi_id, _) = setup_stores_with_enrichment(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_iteration(1)
        .with_footer("Go.".to_string());

    let assembled = builder.build("system").unwrap();
    assert!(assembled.user_message.contains("**Acceptance Criteria:**"));
    assert!(assembled.user_message.contains("- All tests pass"));
    assert!(assembled.user_message.contains("- Coverage above 80%"));
    assert!(assembled.user_message.contains("**Allowed Files:**"));
    assert!(assembled.user_message.contains("- tests/model.rs"));
    assert!(assembled.user_message.contains("**Dependencies:**"));
    assert!(
        assembled
            .user_message
            .contains("[Done] Create model - files: src/model.rs")
    );
}

#[test]
fn test_build_omits_empty_enrichment() {
    let dir = TestDir::new("loopr-ctx-enrich-empty");
    let (stores, wi_id) = setup_stores(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap()
        .with_iteration(1)
        .with_footer("Go.".to_string());

    let assembled = builder.build("system").unwrap();
    // Work from setup_stores has no acceptance_criteria, resource_tags, or dependencies
    assert!(!assembled.user_message.contains("**Acceptance Criteria:**"));
    assert!(!assembled.user_message.contains("**Allowed Files:**"));
    assert!(!assembled.user_message.contains("**Dependencies:**"));
}

fn setup_stores_brief(dir: &std::path::Path) -> (Stores, String) {
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

    let plan = Plan::new("Brief Plan".into(), "criteria".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    // Work parented directly to the Plan (Brief mode - no Phase or Spec)
    let wi = Work::new(plan_id.clone(), "Brief Work".into());
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    (stores, wi_id)
}

#[test]
fn test_load_work_hierarchy_brief() {
    let dir = TestDir::new("loopr-ctx-brief");
    let (stores, wi_id) = setup_stores_brief(&dir);

    let builder = ContextBuilder::new(&stores, Role::Implementer)
        .load_work_hierarchy(&wi_id)
        .unwrap();

    assert!(builder.plan.is_some());
    assert!(builder.spec.is_none());
    assert!(builder.phase.is_none());
    assert!(builder.work.is_some());
    assert_eq!(builder.scope_ids.len(), 2);
    assert!(matches!(builder.scope_ids[0].1, LearningScope::Work));
    assert!(matches!(builder.scope_ids[1].1, LearningScope::Plan));
}
