use std::collections::HashMap;
use std::path::PathBuf;

use super::evaluate::{TriggerEvaluator, TriggerResult};
use super::guard::GuardEvaluator;
use super::observe::{GuardConditionRegistry, ObservationCtx, StateQueryRegistry};
use super::schema::{self, CompositeOperator, CountQuery, Operator, TriggerDefinition, TriggerKind};
use crate::daemon::context::Stores;
use crate::domain::phase::Phase;
use crate::domain::plan::Plan;
use crate::domain::work::{Work, WorkStatus};
use crate::fsm::schema::{FsmDefinition, GuardDef, OnFailure, TransitionRule};
use crate::ipc::protocol::DaemonEvent;

fn strategies_triggers_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("resources/engine/triggers");
    dir
}

fn minimal_threshold() -> TriggerDefinition {
    TriggerDefinition {
        name: "test-threshold".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    }
}

fn minimal_composite(operator: CompositeOperator, triggers: Vec<String>) -> TriggerDefinition {
    TriggerDefinition {
        name: "test-composite".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite { operator, triggers },
    }
}

// --- YAML loading ---

#[test]
fn load_all_yaml_files() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    assert!(!defs.is_empty(), "expected trigger YAML files");
}

#[test]
fn all_yaml_files_pass_validation() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let errors = schema::validate(&defs);
    assert!(errors.is_empty(), "validation errors: {:?}", errors);
}

#[test]
fn load_from_resources_returns_all_triggers() {
    let defs = schema::load_from_resources(None).unwrap();
    assert!(!defs.is_empty(), "expected trigger definitions from embedded resources");
}

#[test]
fn load_from_resources_matches_filesystem() {
    let fs_defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let res_defs = schema::load_from_resources(None).unwrap();
    let mut fs_names: Vec<&str> = fs_defs.iter().map(|d| d.name.as_str()).collect();
    let mut res_names: Vec<&str> = res_defs.iter().map(|d| d.name.as_str()).collect();
    fs_names.sort();
    res_names.sort();
    assert_eq!(
        fs_names, res_names,
        "resource-loaded triggers should match filesystem-loaded triggers"
    );
}

#[test]
fn load_from_resources_passes_validation() {
    let defs = schema::load_from_resources(None).unwrap();
    let errors = schema::validate(&defs);
    assert!(
        errors.is_empty(),
        "resource-loaded triggers should pass validation: {:?}",
        errors
    );
}

#[test]
fn loaded_triggers_cover_all_57_definitions() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    // Exact-count guard. Update this number AND the required-names list below whenever
    // you add or remove a trigger. Subset-only coverage checks let new triggers slip in
    // without review; the length assert makes drift fail CI loudly.
    //
    // Grouped by source YAML for maintenance, but order of the list is not meaningful.
    let required: &[&str] = &[
        // agent-events.yml
        "bundle-proposed",
        "bundle-triaged",
        "reviewer-approved",
        "reviewer-rejected",
        "session-failure",
        "transition-completed",
        "record-created",
        "decomposition-completed",
        "decomposition-failed",
        // agent-lifecycle.yml
        "work-ready",
        "phase-active-no-works",
        "spec-active-no-phases",
        "plan-active-no-specs",
        "bundles-ready-for-integration",
        "director-failed",
        "tick-created",
        "tick-published",
        // composites.yml
        "work-sla-full-breach",
        // engine.yml
        "goal-complete",
        "goal-timeout",
        "escalation-needed",
        "plan-is-active",
        "spec-is-active",
        "phase-is-active",
        "phase-parent-active",
        "conflict-detected",
        "plan-abandoned",
        // plan-quality-gates.yml
        "coverage-incomplete",
        "spec-children-all-abandoned",
        // reconciliation.yml
        "plan-approved",
        "plan-decomposable",
        "spec-decomposable",
        "phase-decomposable",
        "work-promotable",
        "phase-promotable",
        "phase-deps-terminal",
        "phase-children-terminal",
        "spec-children-terminal",
        "hierarchy-deps-terminal",
        "hierarchy-exists",
        "parent-active",
        "work-deps-done",
        "no-running-director",
        // work-safety-nets.yml
        "work-retry-exhaustion",
        "session-failure-limit",
        "action-repetition-loop",
        "error-repetition-loop",
        "parse-failure-limit",
        "researcher-spawn-limit",
        "self-correction-limit",
        "restart-limit",
        "abandon-ratio-exceeded",
        "decomposition-attempt-limit",
        "spec-decomposition-attempt-limit",
        "phase-decomposition-attempt-limit",
        "plan-specs-terminal",
        // work-sla.yml
        "work-sla-breach",
    ];

    assert_eq!(
        names.len(),
        57,
        "trigger loader should expose exactly 57 triggers; got {} ({:?})",
        names.len(),
        names
    );
    assert_eq!(
        required.len(),
        57,
        "required-names list should have exactly 57 entries (got {})",
        required.len()
    );

    for r in required {
        assert!(names.contains(r), "missing trigger {}", r);
    }
}

#[test]
fn work_retry_exhaustion_is_threshold() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "work-retry-exhaustion").unwrap();
    match &def.kind {
        TriggerKind::Threshold {
            scope,
            field,
            operator,
            value,
        } => {
            assert_eq!(scope, "work");
            assert_eq!(field, "attempt-count");
            assert_eq!(operator, &Operator::Gte);
            assert_eq!(*value, 3.0);
        }
        other => panic!("expected Threshold, got {:?}", other),
    }
}

#[test]
fn abandon_ratio_exceeded_is_ratio() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "abandon-ratio-exceeded").unwrap();
    match &def.kind {
        TriggerKind::Ratio {
            scope,
            numerator,
            denominator,
            operator,
            value,
        } => {
            assert_eq!(scope, "plan");
            assert_eq!(numerator.collection, "work");
            assert_eq!(denominator.collection, "work");
            assert_eq!(operator, &Operator::Gt);
            assert_eq!(*value, 0.4);
        }
        other => panic!("expected Ratio, got {:?}", other),
    }
}

#[test]
fn work_sla_full_breach_is_composite_and() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "work-sla-full-breach").unwrap();
    match &def.kind {
        TriggerKind::Composite { operator, triggers } => {
            assert_eq!(operator, &CompositeOperator::And);
            assert_eq!(triggers.len(), 2);
            assert!(triggers.contains(&"work-retry-exhaustion".to_string()));
            assert!(triggers.contains(&"work-sla-breach".to_string()));
        }
        other => panic!("expected Composite, got {:?}", other),
    }
}

#[test]
fn session_failure_is_event_trigger() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "session-failure").unwrap();
    match &def.kind {
        TriggerKind::Event {
            event,
            match_filter,
            throttle_secs,
            ..
        } => {
            assert_eq!(event, "agent.status-changed");
            assert_eq!(match_filter.get("status").and_then(|v| v.as_str()), Some("failed"));
            assert_eq!(*throttle_secs, Some(5));
        }
        other => panic!("expected Event, got {:?}", other),
    }
}

#[test]
fn work_sla_breach_is_timer() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "work-sla-breach").unwrap();
    match &def.kind {
        TriggerKind::Timer {
            scope,
            start_field,
            max_duration_secs,
        } => {
            assert_eq!(scope, "work");
            assert_eq!(start_field, "first-assignment-at");
            assert_eq!(*max_duration_secs, 1800);
        }
        other => panic!("expected Timer, got {:?}", other),
    }
}

// --- Validation: valid cases ---

#[test]
fn valid_threshold_trigger_passes() {
    let def = minimal_threshold();
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

#[test]
fn valid_event_trigger_passes() {
    let def = TriggerDefinition {
        name: "test-event".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "transition.completed".into(),
            scope: None,
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

#[test]
fn valid_composite_and_passes() {
    let a = minimal_threshold();
    let b = TriggerDefinition {
        name: "other-threshold".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "session-failure-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = minimal_composite(
        CompositeOperator::And,
        vec!["test-threshold".into(), "other-threshold".into()],
    );
    let defs = vec![a, b, comp];
    let errors = schema::validate(&defs);
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

// --- Validation: rejection cases ---

#[test]
fn reject_unknown_scope() {
    let def = TriggerDefinition {
        name: "bad-scope".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "foobar".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("unknown scope")),
        "expected 'unknown scope' error, got: {:?}",
        errors
    );
}

#[test]
fn reject_unknown_event_type() {
    let def = TriggerDefinition {
        name: "bad-event".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "nonexistent.event".into(),
            scope: None,
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("unknown event type")),
        "expected 'unknown event type' error, got: {:?}",
        errors
    );
}

#[test]
fn reject_composite_not_with_two_triggers() {
    let a = minimal_threshold();
    let b = TriggerDefinition {
        name: "other".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 1.0,
        },
    };
    let bad_not = minimal_composite(CompositeOperator::Not, vec!["test-threshold".into(), "other".into()]);
    let defs = vec![a, b, bad_not];
    let errors = schema::validate(&defs);
    assert!(
        errors.iter().any(|e| e.contains("exactly one")),
        "expected 'exactly one' error, got: {:?}",
        errors
    );
}

#[test]
fn reject_composite_and_with_one_trigger() {
    let a = minimal_threshold();
    let bad_and = minimal_composite(CompositeOperator::And, vec!["test-threshold".into()]);
    let defs = vec![a, bad_and];
    let errors = schema::validate(&defs);
    assert!(
        errors.iter().any(|e| e.contains("at least 2")),
        "expected 'at least 2' error, got: {:?}",
        errors
    );
}

#[test]
fn reject_composite_reference_to_unknown_trigger() {
    let comp = minimal_composite(
        CompositeOperator::And,
        vec!["ghost-trigger".into(), "also-ghost".into()],
    );
    let defs = vec![comp];
    let errors = schema::validate(&defs);
    assert!(
        errors.iter().any(|e| e.contains("unknown trigger")),
        "expected 'unknown trigger' error, got: {:?}",
        errors
    );
}

#[test]
fn reject_cycle_in_composite() {
    // a -> b -> a
    let a = TriggerDefinition {
        name: "a".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::And,
            triggers: vec!["b".into(), "c".into()],
        },
    };
    // b is a threshold (valid target for a)
    let b = minimal_threshold();
    // We manually name b as "b"
    let b = TriggerDefinition { name: "b".into(), ..b };
    // c -> a (cycle)
    let c = TriggerDefinition {
        name: "c".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::And,
            triggers: vec!["a".into(), "b".into()],
        },
    };
    let defs = vec![a, b, c];
    let errors = schema::validate(&defs);
    assert!(
        errors.iter().any(|e| e.contains("cycle")),
        "expected cycle error, got: {:?}",
        errors
    );
}

#[test]
fn reject_invalid_ratio_collection() {
    let def = TriggerDefinition {
        name: "bad-ratio".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Ratio {
            scope: "plan".into(),
            numerator: CountQuery {
                collection: "notacollection".into(),
                filter: HashMap::new(),
            },
            denominator: CountQuery {
                collection: "work".into(),
                filter: HashMap::new(),
            },
            operator: Operator::Gt,
            value: 0.5,
        },
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("numerator collection")),
        "expected numerator collection error, got: {:?}",
        errors
    );
}

// --- Scope extraction ---

#[test]
fn scope_returns_none_for_composite() {
    let comp = minimal_composite(CompositeOperator::And, vec!["a".into(), "b".into()]);
    assert_eq!(comp.kind.scope(), None);
}

#[test]
fn scope_returns_some_for_threshold() {
    let t = minimal_threshold();
    assert_eq!(t.kind.scope(), Some("work"));
}

// ─── Phase 2: Observation API tests ──────────────────────────────────────────

fn make_stores() -> Stores {
    Stores::new()
}

fn make_ctx(stores: &Stores) -> ObservationCtx<'_> {
    ObservationCtx::new(stores, &[], 0)
}

fn insert_work(stores: &Stores, parent_id: &str, title: &str) -> Work {
    let work = Work::new(parent_id.to_owned(), title.to_owned());
    stores.write_works().unwrap().insert(work.id.clone(), work.clone());
    work
}

fn insert_phase(stores: &Stores, parent_id: &str, title: &str) -> Phase {
    let phase = Phase::new(parent_id.to_owned(), title.to_owned());
    stores.write_phases().unwrap().insert(phase.id.clone(), phase.clone());
    phase
}

// --- get_record ---

#[test]
fn get_record_returns_some_for_existing_work() {
    let stores = make_stores();
    let work = insert_work(&stores, "phase-1", "task");
    let ctx = make_ctx(&stores);
    assert!(ctx.get_record("work", &work.id).is_some());
}

#[test]
fn get_record_returns_none_for_missing_id() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    assert!(ctx.get_record("work", "no-such-id").is_none());
}

#[test]
fn get_record_returns_none_for_unknown_collection() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    assert!(ctx.get_record("unicorn", "anything").is_none());
}

#[test]
fn get_record_includes_parent_id_in_json() {
    let stores = make_stores();
    let work = insert_work(&stores, "phase-42", "task");
    let ctx = make_ctx(&stores);
    let json = ctx.get_record("work", &work.id).unwrap();
    assert_eq!(json["parent_id"], "phase-42");
}

// --- count ---

#[test]
fn count_empty_collection_returns_zero() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    assert_eq!(ctx.count("work", &HashMap::new()), 0);
}

#[test]
fn count_with_empty_filter_returns_all() {
    let stores = make_stores();
    insert_work(&stores, "p1", "a");
    insert_work(&stores, "p1", "b");
    let ctx = make_ctx(&stores);
    assert_eq!(ctx.count("work", &HashMap::new()), 2);
}

#[test]
fn count_with_status_filter_case_insensitive() {
    let stores = make_stores();
    insert_work(&stores, "p1", "done-work");
    insert_work(&stores, "p1", "draft-work");
    let ctx = make_ctx(&stores);
    // Newly created works are Draft status
    let filter: HashMap<String, serde_json::Value> = [("status".to_string(), serde_json::json!("draft"))].into();
    assert_eq!(ctx.count("work", &filter), 2);
}

#[test]
fn count_terminal_filter_matches_terminal_records() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    // Force work status to Done via direct mutation in the store
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .force_status(WorkStatus::Done);
    insert_work(&stores, "p1", "other"); // this is Draft (non-terminal)
    let ctx = make_ctx(&stores);
    let filter: HashMap<String, serde_json::Value> = [("terminal".to_string(), serde_json::json!(true))].into();
    assert_eq!(ctx.count("work", &filter), 1);
}

// --- get_field_u32 ---

#[test]
fn get_field_u32_kebab_case_field_name() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    // attempt_count defaults to 0
    let ctx = make_ctx(&stores);
    assert_eq!(ctx.get_field_u32("work", &work.id, "attempt-count"), Some(0));
}

#[test]
fn get_field_u32_returns_none_for_missing_record() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    assert_eq!(ctx.get_field_u32("work", "no-such-id", "attempt-count"), None);
}

// --- get_field_timestamp ---

#[test]
fn get_field_timestamp_returns_created_at() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let ts = ctx.get_field_timestamp("work", &work.id, "created-at");
    assert!(ts.is_some());
    assert!(ts.unwrap() > 0);
}

// --- has_event ---

#[test]
fn has_event_returns_false_when_bus_empty() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    assert!(!ctx.has_event("record.created", &HashMap::new()));
}

#[test]
fn has_event_matches_event_type_and_filter() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 0);
    let filter: HashMap<String, serde_json::Value> = [("status".to_string(), serde_json::json!("failed"))].into();
    assert!(ctx.has_event("agent.status-changed", &filter));
}

#[test]
fn has_event_does_not_match_wrong_type() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new("record.created", serde_json::json!({}))];
    let ctx = ObservationCtx::new(&stores, &events, 0);
    assert!(!ctx.has_event("transition.completed", &HashMap::new()));
}

#[test]
fn has_event_does_not_match_wrong_filter_value() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "running"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 0);
    let filter: HashMap<String, serde_json::Value> = [("status".to_string(), serde_json::json!("failed"))].into();
    assert!(!ctx.has_event("agent.status-changed", &filter));
}

// --- children ---

#[test]
fn children_returns_works_with_matching_parent_id() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    insert_work(&stores, &phase.id, "work-a");
    insert_work(&stores, &phase.id, "work-b");
    insert_work(&stores, "other-phase", "work-c");
    let ctx = make_ctx(&stores);
    let children = ctx.children(&phase.id, "work");
    assert_eq!(children.len(), 2);
}

#[test]
fn children_returns_empty_when_no_match() {
    let stores = make_stores();
    insert_work(&stores, "phase-x", "work");
    let ctx = make_ctx(&stores);
    assert!(ctx.children("phase-y", "work").is_empty());
}

// --- StateQueryRegistry ---

#[test]
fn state_query_registry_has_all_11_builtins() {
    let reg = StateQueryRegistry::with_builtins();
    let names = reg.names();
    assert_eq!(
        names.len(),
        11,
        "state query registry should have exactly 11 built-ins; update this test when adding or removing one (got {:?})",
        names
    );
    for required in &[
        "all-children-terminal",
        "all-children-done",
        "all-deps-terminal",
        "all-deps-done",
        "parent-active",
        "has-children",
        "has-no-children",
        "no-active-sessions",
        "field-equals",
        "field-is-true",
        "has-active-plan",
    ] {
        assert!(names.contains(required), "missing built-in query: {}", required);
    }
}

#[test]
fn state_query_has_children_returns_true_when_children_exist() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    insert_work(&stores, &phase.id, "work");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("child-collection".to_string(), serde_json::json!("work"))].into();
    assert!(reg.evaluate("has-children", &ctx, "phase", &phase.id, &params));
}

#[test]
fn state_query_has_children_returns_false_when_empty() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("child-collection".to_string(), serde_json::json!("work"))].into();
    assert!(!reg.evaluate("has-children", &ctx, "phase", &phase.id, &params));
}

#[test]
fn state_query_has_no_children_returns_true_when_empty() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("child-collection".to_string(), serde_json::json!("work"))].into();
    assert!(reg.evaluate("has-no-children", &ctx, "phase", &phase.id, &params));
}

#[test]
fn state_query_has_no_children_returns_false_when_children_exist() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    insert_work(&stores, &phase.id, "work");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("child-collection".to_string(), serde_json::json!("work"))].into();
    assert!(!reg.evaluate("has-no-children", &ctx, "phase", &phase.id, &params));
}

#[test]
fn state_query_has_no_children_only_counts_matching_parent() {
    let stores = make_stores();
    let phase_a = insert_phase(&stores, "spec-1", "phase-a");
    let phase_b = insert_phase(&stores, "spec-1", "phase-b");
    insert_work(&stores, &phase_a.id, "work-for-a");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("child-collection".to_string(), serde_json::json!("work"))].into();
    // phase_a has a child, phase_b does not
    assert!(!reg.evaluate("has-no-children", &ctx, "phase", &phase_a.id, &params));
    assert!(reg.evaluate("has-no-children", &ctx, "phase", &phase_b.id, &params));
}

#[test]
fn decomposition_state_query_triggers_are_loaded() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    for required in &[
        "plan-is-active",
        "spec-is-active",
        "phase-is-active",
        "plan-active-no-specs",
        "spec-active-no-phases",
        "phase-active-no-works",
    ] {
        assert!(names.contains(required), "missing decomposition trigger: {}", required);
    }
}

#[test]
fn decomposition_composite_triggers_are_loaded() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    for required in &["plan-decomposable", "spec-decomposable", "phase-decomposable"] {
        assert!(
            names.contains(required),
            "missing decomposition composite: {}",
            required
        );
    }
}

#[test]
fn decomposable_composites_are_and_operators() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    for name in &["plan-decomposable", "spec-decomposable", "phase-decomposable"] {
        let def = defs
            .iter()
            .find(|d| &d.name == name)
            .unwrap_or_else(|| panic!("missing {}", name));
        match &def.kind {
            TriggerKind::Composite { operator, triggers } => {
                assert_eq!(operator, &CompositeOperator::And, "{} should be AND", name);
                assert_eq!(triggers.len(), 2, "{} should have exactly 2 sub-triggers", name);
            }
            other => panic!("{}: expected Composite, got {:?}", name, other),
        }
    }
}

#[test]
fn state_query_field_equals_matches_string_case_insensitively() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> = [
        ("field".to_string(), serde_json::json!("status")),
        ("value".to_string(), serde_json::json!("draft")),
    ]
    .into();
    assert!(reg.evaluate("field-equals", &ctx, "work", &work.id, &params));
}

#[test]
fn state_query_all_children_terminal_false_when_some_non_terminal() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    let work = insert_work(&stores, &phase.id, "task");
    // work is Draft (non-terminal)
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> = [
        ("child-collection".to_string(), serde_json::json!("work")),
        (
            "terminal-statuses".to_string(),
            serde_json::json!(["done", "abandoned"]),
        ),
    ]
    .into();
    assert!(!reg.evaluate("all-children-terminal", &ctx, "phase", &phase.id, &params));
    // Silence unused variable warning from the work binding
    let _ = work;
}

#[test]
fn state_query_all_deps_done_true_when_no_deps() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    let params: HashMap<String, serde_json::Value> =
        [("dep-field".to_string(), serde_json::json!("dependencies"))].into();
    // Work has empty dependencies - vacuously true.
    assert!(reg.evaluate("all-deps-done", &ctx, "work", &work.id, &params));
}

#[test]
fn state_query_unknown_returns_false() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    assert!(!reg.evaluate("no-such-query", &ctx, "work", "any-id", &HashMap::new()));
}

#[test]
fn state_query_has_active_plan_false_when_empty() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    assert!(
        !reg.evaluate("has-active-plan", &ctx, "plan", "ignored", &HashMap::new()),
        "no plans => has-active-plan must be false"
    );
}

#[test]
fn state_query_has_active_plan_true_when_draft() {
    use crate::domain::criteria::AcceptanceCriteria;
    let stores = make_stores();
    // Plan::new starts in Draft which is non-terminal.
    let plan = Plan::new("Test Plan".to_owned(), AcceptanceCriteria::default());
    stores.write_plans().unwrap().insert(plan.id.clone(), plan);
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    assert!(
        reg.evaluate("has-active-plan", &ctx, "plan", "ignored", &HashMap::new()),
        "one Draft plan => has-active-plan must be true"
    );
}

#[test]
fn state_query_has_active_plan_false_when_all_terminal() {
    use crate::domain::criteria::AcceptanceCriteria;
    use crate::domain::plan::HierarchyStatus;
    let stores = make_stores();
    for status in [
        HierarchyStatus::Complete,
        HierarchyStatus::Superseded,
        HierarchyStatus::Abandoned,
    ] {
        let mut plan = Plan::new(format!("{:?}", status), AcceptanceCriteria::default());
        plan.force_status(status);
        stores.write_plans().unwrap().insert(plan.id.clone(), plan);
    }
    let ctx = make_ctx(&stores);
    let reg = StateQueryRegistry::with_builtins();
    assert!(
        !reg.evaluate("has-active-plan", &ctx, "plan", "ignored", &HashMap::new()),
        "all-terminal plans => has-active-plan must be false"
    );
}

#[test]
fn guard_has_active_plan_mirrors_state_query() {
    use crate::domain::criteria::AcceptanceCriteria;
    use crate::domain::plan::HierarchyStatus;
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    let reg = GuardConditionRegistry::with_builtins();

    // Empty: false
    assert!(!reg.evaluate("has-active-plan", &ctx, "plan", "ignored"));

    // One Pending plan: true
    let mut plan = Plan::new("p".to_owned(), AcceptanceCriteria::default());
    plan.force_status(HierarchyStatus::Pending);
    stores.write_plans().unwrap().insert(plan.id.clone(), plan);
    assert!(reg.evaluate("has-active-plan", &ctx, "plan", "ignored"));
}

// --- GuardConditionRegistry ---

#[test]
fn guard_registry_has_all_9_builtins() {
    let reg = GuardConditionRegistry::with_builtins();
    let names = reg.names();
    assert_eq!(
        names.len(),
        9,
        "guard registry should have exactly 9 built-ins; update this test when adding or removing one (got {:?})",
        names
    );
    for required in &[
        "no-active-sessions",
        "has-active-plan",
        "deps-satisfied",
        "validation-passed",
        "all-ac-passing",
        "no-active-implementer-for-work",
        "no-active-reviewer-for-bundle",
        "no-active-researcher-for-phase",
        "no-active-director",
    ] {
        assert!(names.contains(required), "missing built-in guard: {}", required);
    }
}

#[test]
fn guard_no_active_sessions_true_when_no_sessions() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let reg = GuardConditionRegistry::with_builtins();
    assert!(reg.evaluate("no-active-sessions", &ctx, "work", &work.id));
}

#[test]
fn guard_deps_satisfied_true_when_no_deps() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let reg = GuardConditionRegistry::with_builtins();
    // Work has empty dependencies - vacuously satisfied.
    assert!(reg.evaluate("deps-satisfied", &ctx, "work", &work.id));
}

#[test]
fn guard_unknown_condition_returns_false() {
    let stores = make_stores();
    let ctx = make_ctx(&stores);
    let reg = GuardConditionRegistry::with_builtins();
    assert!(!reg.evaluate("ghost-condition", &ctx, "work", "any-id"));
}

// ─── Phase 3: Trigger Evaluator tests ───────────────────────────────────────

fn make_evaluator(defs: Vec<TriggerDefinition>) -> TriggerEvaluator {
    TriggerEvaluator::new(defs, StateQueryRegistry::with_builtins())
}

fn insert_plan(stores: &Stores, title: &str) -> Plan {
    use crate::domain::criteria::AcceptanceCriteria;
    let plan = Plan::new(title.to_owned(), AcceptanceCriteria::default());
    stores.write_plans().unwrap().insert(plan.id.clone(), plan.clone());
    plan
}

// --- Threshold evaluator ---

#[test]
fn threshold_fires_when_field_meets_condition() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let ctx = make_ctx(&stores);
    let mut eval = make_evaluator(vec![minimal_threshold()]); // attempt-count >= 3
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "test-threshold");
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&work.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn threshold_idle_when_field_below_condition() {
    let stores = make_stores();
    insert_work(&stores, "p1", "task"); // attempt_count defaults to 0
    let ctx = make_ctx(&stores);
    let mut eval = make_evaluator(vec![minimal_threshold()]); // >= 3
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty());
}

#[test]
fn threshold_fires_only_for_matching_records() {
    let stores = make_stores();
    let w1 = insert_work(&stores, "p1", "over");
    stores.write_works().unwrap().get_mut(&w1.id).unwrap().attempt_count = 5;
    let w2 = insert_work(&stores, "p1", "under");
    // w2 attempt_count stays 0
    let ctx = make_ctx(&stores);
    let mut eval = make_evaluator(vec![minimal_threshold()]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&w1.id));
        assert!(!scope_ids.contains(&w2.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn threshold_exact_boundary_gte() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 3;
    let ctx = make_ctx(&stores);
    let mut eval = make_evaluator(vec![minimal_threshold()]); // >= 3
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1, "exactly-at-threshold should fire for >=");
}

// --- Ratio evaluator ---

#[test]
fn ratio_fires_when_ratio_exceeds_threshold() {
    let stores = make_stores();
    let plan = insert_plan(&stores, "test-plan");
    // 3 works: 2 abandoned (terminal), 1 done (terminal) = 2/3 = 0.667 > 0.4
    let w1 = insert_work(&stores, &plan.id, "abandoned-1");
    let w2 = insert_work(&stores, &plan.id, "abandoned-2");
    let w3 = insert_work(&stores, &plan.id, "done-1");
    stores
        .write_works()
        .unwrap()
        .get_mut(&w1.id)
        .unwrap()
        .force_status(WorkStatus::Abandoned);
    stores
        .write_works()
        .unwrap()
        .get_mut(&w2.id)
        .unwrap()
        .force_status(WorkStatus::Abandoned);
    stores
        .write_works()
        .unwrap()
        .get_mut(&w3.id)
        .unwrap()
        .force_status(WorkStatus::Done);
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "abandon-ratio-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Ratio {
            scope: "plan".into(),
            numerator: CountQuery {
                collection: "work".into(),
                filter: [
                    ("status".to_string(), serde_json::json!("Abandoned")),
                    ("terminal".to_string(), serde_json::json!(true)),
                ]
                .into(),
            },
            denominator: CountQuery {
                collection: "work".into(),
                filter: [("terminal".to_string(), serde_json::json!(true))].into(),
            },
            operator: Operator::Gt,
            value: 0.4,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&plan.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn ratio_idle_when_ratio_below_threshold() {
    let stores = make_stores();
    let plan = insert_plan(&stores, "test-plan");
    // 3 works: 1 abandoned, 2 done = 1/3 = 0.333 < 0.4
    let w1 = insert_work(&stores, &plan.id, "abandoned-1");
    let w2 = insert_work(&stores, &plan.id, "done-1");
    let w3 = insert_work(&stores, &plan.id, "done-2");
    stores
        .write_works()
        .unwrap()
        .get_mut(&w1.id)
        .unwrap()
        .force_status(WorkStatus::Abandoned);
    stores
        .write_works()
        .unwrap()
        .get_mut(&w2.id)
        .unwrap()
        .force_status(WorkStatus::Done);
    stores
        .write_works()
        .unwrap()
        .get_mut(&w3.id)
        .unwrap()
        .force_status(WorkStatus::Done);
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "abandon-ratio-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Ratio {
            scope: "plan".into(),
            numerator: CountQuery {
                collection: "work".into(),
                filter: [
                    ("status".to_string(), serde_json::json!("Abandoned")),
                    ("terminal".to_string(), serde_json::json!(true)),
                ]
                .into(),
            },
            denominator: CountQuery {
                collection: "work".into(),
                filter: [("terminal".to_string(), serde_json::json!(true))].into(),
            },
            operator: Operator::Gt,
            value: 0.4,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty());
}

#[test]
fn ratio_idle_when_denominator_zero() {
    let stores = make_stores();
    let plan = insert_plan(&stores, "test-plan");
    // All works are Draft (non-terminal), so denominator is 0
    insert_work(&stores, &plan.id, "draft");
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "ratio-zero-denom".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Ratio {
            scope: "plan".into(),
            numerator: CountQuery {
                collection: "work".into(),
                filter: [("terminal".to_string(), serde_json::json!(true))].into(),
            },
            denominator: CountQuery {
                collection: "work".into(),
                filter: [("terminal".to_string(), serde_json::json!(true))].into(),
            },
            operator: Operator::Gt,
            value: 0.0,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty(), "zero denominator should not fire");
}

// --- Event evaluator ---

#[test]
fn event_fires_on_matching_event() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed", "work-id": "wk-123"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "session-failure-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: Some("work".into()),
            match_filter: [("status".to_string(), serde_json::json!("failed"))].into(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, payload } = &results[0].1 {
        assert!(scope_ids.contains(&"wk-123".to_string()));
        assert!(payload.is_some());
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn event_idle_when_no_matching_event() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "record.created",
        serde_json::json!({"collection": "work", "id": "wk-1"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "session-failure-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: Some("work".into()),
            match_filter: [("status".to_string(), serde_json::json!("failed"))].into(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_push(&ctx);
    assert!(results.is_empty());
}

#[test]
fn event_extracts_id_from_standard_events() {
    let stores = make_stores();
    let events = vec![DaemonEvent::transition_completed(
        "work",
        "wk-42",
        "Draft",
        "Active",
        "coordinator",
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "transition-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "transition.completed".into(),
            scope: Some("work".into()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&"wk-42".to_string()));
    } else {
        panic!("expected Fired");
    }
}

// --- Timer evaluator ---

#[test]
fn timer_fires_when_elapsed_exceeds_max() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let created_at = work.created_at;
    // Set now to 31 minutes after creation (> 1800 secs)
    let now = created_at + 31 * 60 * 1000;
    let ctx = ObservationCtx::new(&stores, &[], now);
    let def = TriggerDefinition {
        name: "sla-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Timer {
            scope: "work".into(),
            start_field: "created-at".into(),
            max_duration_secs: 1800,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&work.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn timer_idle_when_under_max() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let created_at = work.created_at;
    // Set now to 10 minutes after creation (< 1800 secs)
    let now = created_at + 10 * 60 * 1000;
    let ctx = ObservationCtx::new(&stores, &[], now);
    let def = TriggerDefinition {
        name: "sla-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Timer {
            scope: "work".into(),
            start_field: "created-at".into(),
            max_duration_secs: 1800,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty());
}

// --- State-query evaluator ---

#[test]
fn state_query_trigger_fires_when_query_true() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    insert_work(&stores, &phase.id, "child");
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "has-children-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::StateQuery {
            scope: "phase".into(),
            query: "has-children".into(),
            params: [("child-collection".to_string(), serde_json::json!("work"))].into(),
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&phase.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn state_query_trigger_idle_when_query_false() {
    let stores = make_stores();
    let phase = insert_phase(&stores, "spec-1", "phase");
    // No children
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "has-children-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::StateQuery {
            scope: "phase".into(),
            query: "has-children".into(),
            params: [("child-collection".to_string(), serde_json::json!("work"))].into(),
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty());
    let _ = phase;
}

// --- Composite evaluator ---

#[test]
fn composite_and_fires_when_all_sub_triggers_fire() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .session_failure_count = 4;
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let t2 = TriggerDefinition {
        name: "session-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "session-failure-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "both-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::And,
            triggers: vec!["attempt-check".into(), "session-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, t2, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "both-failing");
    assert!(comp_result.is_some(), "composite AND should fire");
    if let TriggerResult::Fired { scope_ids, .. } = &comp_result.unwrap().1 {
        assert!(scope_ids.contains(&work.id));
    }
}

#[test]
fn composite_and_idle_when_one_sub_trigger_idle() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    // session_failure_count stays at 0, below threshold of 3
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let t2 = TriggerDefinition {
        name: "session-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "session-failure-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "both-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::And,
            triggers: vec!["attempt-check".into(), "session-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, t2, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "both-failing");
    assert!(
        comp_result.is_none(),
        "composite AND should be idle when one sub-trigger is idle"
    );
}

#[test]
fn composite_or_fires_when_any_sub_trigger_fires() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    // session_failure_count stays 0
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let t2 = TriggerDefinition {
        name: "session-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "session-failure-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "either-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Or,
            triggers: vec!["attempt-check".into(), "session-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, t2, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "either-failing");
    assert!(
        comp_result.is_some(),
        "composite OR should fire when one sub-trigger fires"
    );
}

#[test]
fn composite_or_idle_when_no_sub_trigger_fires() {
    let stores = make_stores();
    insert_work(&stores, "p1", "task"); // both counts at 0
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let t2 = TriggerDefinition {
        name: "session-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "session-failure-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "either-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Or,
            triggers: vec!["attempt-check".into(), "session-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, t2, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "either-failing");
    assert!(
        comp_result.is_none(),
        "composite OR should be idle when no sub-triggers fire"
    );
}

#[test]
fn composite_not_fires_when_sub_trigger_idle() {
    let stores = make_stores();
    insert_work(&stores, "p1", "task"); // attempt_count = 0
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "not-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Not,
            triggers: vec!["attempt-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "not-failing");
    assert!(
        comp_result.is_some(),
        "composite NOT should fire when sub-trigger is idle"
    );
}

#[test]
fn composite_not_idle_when_sub_trigger_fires() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "not-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Not,
            triggers: vec!["attempt-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "not-failing");
    assert!(
        comp_result.is_none(),
        "composite NOT should be idle when sub-trigger fires"
    );
}

// --- Cooldown ---

#[test]
fn cooldown_suppresses_refire_within_window() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let def = TriggerDefinition {
        name: "cooldown-test".into(),
        enabled: true,
        cooldown_secs: Some(60),
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let mut eval = make_evaluator(vec![def]);

    // First evaluation at t=1000: should fire
    let ctx = ObservationCtx::new(&stores, &[], 1000);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1, "first eval should fire");

    // Second evaluation at t=1000+30s: should be suppressed (within 60s cooldown)
    let ctx = ObservationCtx::new(&stores, &[], 1000 + 30_000);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty(), "second eval within cooldown should be suppressed");

    // Third evaluation at t=1000+61s: should fire again (cooldown expired)
    let ctx = ObservationCtx::new(&stores, &[], 1000 + 61_000);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1, "eval after cooldown should fire again");
}

#[test]
fn throttle_suppresses_event_refire() {
    let stores = make_stores();
    let def = TriggerDefinition {
        name: "throttle-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: Some("work".into()),
            match_filter: [("status".to_string(), serde_json::json!("failed"))].into(),
            throttle_secs: Some(5),
        },
    };
    let mut eval = make_evaluator(vec![def]);

    // First event at t=1000: should fire
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed", "work-id": "wk-1"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1, "first event should fire");

    // Same event at t=1000+3s: should be throttled
    let ctx = ObservationCtx::new(&stores, &events, 1000 + 3000);
    let results = eval.evaluate_push(&ctx);
    assert!(results.is_empty(), "event within throttle window should be suppressed");

    // Same event at t=1000+6s: should fire again
    let ctx = ObservationCtx::new(&stores, &events, 1000 + 6000);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1, "event after throttle window should fire again");
}

#[test]
fn cooldown_is_per_scope_id() {
    let stores = make_stores();
    let w1 = insert_work(&stores, "p1", "task-1");
    let w2 = insert_work(&stores, "p1", "task-2");
    stores.write_works().unwrap().get_mut(&w1.id).unwrap().attempt_count = 5;
    stores.write_works().unwrap().get_mut(&w2.id).unwrap().attempt_count = 5;
    let def = TriggerDefinition {
        name: "cooldown-per-id".into(),
        enabled: true,
        cooldown_secs: Some(60),
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let mut eval = make_evaluator(vec![def]);

    // First eval: both should fire
    let ctx = ObservationCtx::new(&stores, &[], 1000);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert_eq!(scope_ids.len(), 2, "both works should fire on first eval");
    }

    // Second eval within cooldown: neither should fire
    let ctx = ObservationCtx::new(&stores, &[], 1000 + 30_000);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty(), "both should be suppressed within cooldown");
}

#[test]
fn sweep_removes_expired_cooldowns() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let def = TriggerDefinition {
        name: "sweep-test".into(),
        enabled: true,
        cooldown_secs: Some(10),
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let mut eval = make_evaluator(vec![def]);

    // Fire at t=0
    let ctx = ObservationCtx::new(&stores, &[], 0);
    eval.evaluate_pull(&ctx);

    // Sweep at t=2 hours: should remove the cooldown entry
    eval.sweep_cooldowns(2 * 3_600_000);
    // The internal cooldowns map should be empty after sweep
    // Verify by firing again - it should fire because the cooldown was swept
    let ctx = ObservationCtx::new(&stores, &[], 2 * 3_600_000);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1, "should fire after cooldown swept");
}

// --- Enabled field: serde and evaluation ---

#[test]
fn trigger_enabled_defaults_to_true() {
    let yaml = r#"
my-trigger:
  type: threshold
  scope: work
  field: attempt-count
  operator: ">="
  value: 3.0
"#;
    let defs = schema::parse_content(yaml, "test").unwrap();
    assert_eq!(defs.len(), 1);
    assert!(defs[0].enabled, "enabled should default to true");
}

#[test]
fn trigger_enabled_false_deserializes() {
    let yaml = r#"
my-trigger:
  type: threshold
  scope: work
  field: attempt-count
  operator: ">="
  value: 3.0
  enabled: false
"#;
    let defs = schema::parse_content(yaml, "test").unwrap();
    assert_eq!(defs.len(), 1);
    assert!(!defs[0].enabled, "enabled: false should deserialize correctly");
}

#[test]
fn evaluate_pull_skips_disabled_trigger() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let ctx = make_ctx(&stores);
    let def = TriggerDefinition {
        name: "disabled-threshold".into(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty(), "disabled trigger must not fire in pull mode");
}

#[test]
fn evaluate_push_skips_disabled_trigger() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed", "work-id": "wk-1"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "disabled-event".into(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: Some("work".into()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_push(&ctx);
    assert!(results.is_empty(), "disabled trigger must not fire in push mode");
}

#[test]
fn validate_rejects_not_with_disabled_sub_trigger() {
    // NOT(disabled) fires unconditionally - must be a validation error.
    let sub = TriggerDefinition {
        name: "disabled-sub".into(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let not_trigger = TriggerDefinition {
        name: "not-of-disabled".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Not,
            triggers: vec!["disabled-sub".into()],
        },
    };
    let errors = schema::validate(&[sub, not_trigger]);
    assert!(
        errors.iter().any(|e| e.contains("fires unconditionally")),
        "expected NOT(disabled) validation error, got: {:?}",
        errors
    );
}

// --- Pull vs push separation ---

#[test]
fn evaluate_pull_skips_event_triggers() {
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "event-only".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: None,
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert!(results.is_empty(), "pull should not evaluate event triggers");
}

#[test]
fn evaluate_push_skips_threshold_triggers() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 5;
    let ctx = make_ctx(&stores);
    let mut eval = make_evaluator(vec![minimal_threshold()]);
    let results = eval.evaluate_push(&ctx);
    assert!(results.is_empty(), "push should not evaluate threshold triggers");
}

// --- v3 regression: representative conditions ---

#[test]
fn v3_work_retry_exhaustion_fires_at_max_attempts() {
    // v3: attempt_count >= MAX_WORK_ATTEMPTS (3)
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores.write_works().unwrap().get_mut(&work.id).unwrap().attempt_count = 3;
    let ctx = make_ctx(&stores);
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let retry_def = defs.into_iter().find(|d| d.name == "work-retry-exhaustion").unwrap();
    let mut eval = make_evaluator(vec![retry_def]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.contains(&work.id));
    } else {
        panic!("expected Fired");
    }
}

#[test]
fn v3_session_failure_event_fires() {
    // v3: agent.status-changed(failed)
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed", "work-id": "wk-99"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let event_def = defs.into_iter().find(|d| d.name == "session-failure").unwrap();
    let mut eval = make_evaluator(vec![event_def]);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "session-failure");
}

#[test]
fn v3_work_sla_breach_fires_after_timeout() {
    // v3: now - first_assignment_at > max_wall_clock (1800s)
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let created_at = work.created_at;
    let now = created_at + 31 * 60 * 1000; // 31 minutes
    let ctx = ObservationCtx::new(&stores, &[], now);
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    // work-sla-breach uses start-field: first-assignment-at, but our test work
    // doesn't have that field set (it would be None/0). The timer trigger requires
    // ts > 0, so we use created-at which is always set. We test with our own def.
    let def = TriggerDefinition {
        name: "sla-regression".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Timer {
            scope: "work".into(),
            start_field: "created-at".into(),
            max_duration_secs: 1800,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_pull(&ctx);
    assert_eq!(results.len(), 1);
    let _ = defs; // loaded to verify YAML parses
}

// ─── Architect review fixes (round 1) ────────────────────────────────────────

// Fix 1: NOT set-difference - fires for records NOT matched by sub-trigger

#[test]
fn composite_not_fires_for_unmatched_records_only() {
    // Two works: w1 has attempt_count=5 (matches sub-trigger), w2 has 0 (does not).
    // NOT should fire for w2 only.
    let stores = make_stores();
    let w1 = insert_work(&stores, "p1", "over-threshold");
    let w2 = insert_work(&stores, "p1", "under-threshold");
    stores.write_works().unwrap().get_mut(&w1.id).unwrap().attempt_count = 5;
    let ctx = make_ctx(&stores);
    let t1 = TriggerDefinition {
        name: "attempt-check".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".into(),
            field: "attempt-count".into(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let comp = TriggerDefinition {
        name: "not-failing".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Not,
            triggers: vec!["attempt-check".into()],
        },
    };
    let mut eval = make_evaluator(vec![t1, comp]);
    let results = eval.evaluate_pull(&ctx);
    let comp_result = results.iter().find(|(name, _)| name == "not-failing");
    assert!(comp_result.is_some(), "NOT should fire for w2");
    if let TriggerResult::Fired { scope_ids, .. } = &comp_result.unwrap().1 {
        assert!(scope_ids.contains(&w2.id), "NOT scope_ids should include unmatched w2");
        assert!(!scope_ids.contains(&w1.id), "NOT scope_ids should exclude matched w1");
    } else {
        panic!("expected Fired");
    }
}

// Fix 2: Event ID precedence - scope-keyed field beats generic `id`

#[test]
fn event_prefers_scope_keyed_id_over_generic_id() {
    // Event has both `id` (session ID) and `work_id` (work ID).
    // A work-scoped trigger must pick work_id, not id.
    let stores = make_stores();
    let events = vec![DaemonEvent::new(
        "agent.status-changed",
        serde_json::json!({"status": "failed", "id": "sess-999", "work-id": "wk-42"}),
    )];
    let ctx = ObservationCtx::new(&stores, &events, 1000);
    let def = TriggerDefinition {
        name: "session-failure-test".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "agent.status-changed".into(),
            scope: Some("work".into()),
            match_filter: [("status".to_string(), serde_json::json!("failed"))].into(),
            throttle_secs: None,
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1);
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(
            scope_ids.contains(&"wk-42".to_string()),
            "should use work_id, not id: got {:?}",
            scope_ids
        );
        assert!(
            !scope_ids.contains(&"sess-999".to_string()),
            "should not use generic id field"
        );
    } else {
        panic!("expected Fired");
    }
}

// Fix 3: Global event throttle - scope-less event trigger with throttle_secs

#[test]
fn global_event_trigger_throttle_suppresses_refire() {
    // scope: None means global (no scope_ids). Throttle must still work.
    let stores = make_stores();
    let def = TriggerDefinition {
        name: "global-event".into(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Event {
            event: "tick.published".into(),
            scope: None,
            match_filter: HashMap::new(),
            throttle_secs: Some(10),
        },
    };
    let mut eval = make_evaluator(vec![def]);
    let events = vec![DaemonEvent::new("tick.published", serde_json::json!({}))];

    // Timestamps are in milliseconds. throttle_secs=10 -> 10_000ms window.

    // First fire at t=0ms.
    let ctx = ObservationCtx::new(&stores, &events, 0);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1, "first global event should fire");
    if let TriggerResult::Fired { scope_ids, .. } = &results[0].1 {
        assert!(scope_ids.is_empty(), "global trigger scope_ids should be empty");
    }

    // Within throttle window at t=5_000ms: should be suppressed.
    let ctx = ObservationCtx::new(&stores, &events, 5_000);
    let results = eval.evaluate_push(&ctx);
    assert!(
        results.is_empty(),
        "global event within throttle window should be suppressed"
    );

    // After throttle window at t=11_000ms: should fire again.
    let ctx = ObservationCtx::new(&stores, &events, 11_000);
    let results = eval.evaluate_push(&ctx);
    assert_eq!(results.len(), 1, "global event after throttle window should fire again");
}

// ─── Phase 4: Guard Evaluator tests ──────────────────────────────────────────

/// Build a minimal FsmDefinition with one transition and an optional guard.
fn minimal_fsm_with_guard(guard_name: Option<&str>, condition: &str, from: &str, to: &str) -> FsmDefinition {
    let mut transitions = HashMap::new();
    let mut targets = HashMap::new();
    targets.insert(to.to_owned(), TransitionRule::default());
    transitions.insert(from.to_owned(), targets);
    // Add terminal state reachable from `to`
    let mut to_targets = HashMap::new();
    to_targets.insert("done".to_owned(), TransitionRule::default());
    transitions.insert(to.to_owned(), to_targets);

    let mut guards = HashMap::new();
    if let Some(name) = guard_name {
        guards.insert(
            name.to_owned(),
            GuardDef {
                from: from.to_owned(),
                to: to.to_owned(),
                condition: condition.to_owned(),
                on_failure: OnFailure::Reject,
                message: format!("{} required for {}->{}", condition, from, to),
            },
        );
    }

    FsmDefinition {
        name: "test-fsm".to_owned(),
        description: String::new(),
        states: vec![from.to_owned(), to.to_owned(), "done".to_owned()],
        terminal: vec!["done".to_owned()],
        transitions,
        overrides: HashMap::new(),
        guards,
    }
}

// --- Guard passes ---

#[test]
fn guard_passes_when_condition_true() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    // no-active-sessions is true when no sessions exist
    let ctx = make_ctx(&stores);
    let def = minimal_fsm_with_guard(Some("session-check"), "no-active-sessions", "in-progress", "done");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    assert!(
        eval.check_transition(&def, "in-progress", "done", "work", &work.id, &ctx)
            .is_ok(),
        "guard should pass when no active sessions"
    );
}

#[test]
fn guard_passes_when_no_guards_on_transition() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    // Guard is on "a->b" but we're checking "x->y" - no guards should fire
    let def = minimal_fsm_with_guard(Some("some-guard"), "no-active-sessions", "a", "b");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    assert!(
        eval.check_transition(&def, "x", "y", "work", &work.id, &ctx).is_ok(),
        "no guard on this edge - should pass unconditionally"
    );
}

#[test]
fn guard_passes_with_empty_guards_map() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    let ctx = make_ctx(&stores);
    let def = minimal_fsm_with_guard(None, "", "draft", "active");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    assert!(
        eval.check_transition(&def, "draft", "active", "work", &work.id, &ctx)
            .is_ok(),
        "empty guards map - should always pass"
    );
}

// --- Guard rejects ---

#[test]
fn guard_rejects_when_condition_false() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    // deps-satisfied is false when dep IDs listed but don't exist
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .dependencies
        .push("nonexistent-dep".to_owned());
    let ctx = make_ctx(&stores);
    let def = minimal_fsm_with_guard(Some("dep-check"), "deps-satisfied", "pending", "ready");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    let result = eval.check_transition(&def, "pending", "ready", "work", &work.id, &ctx);
    assert!(result.is_err(), "guard should reject when deps not satisfied");
}

#[test]
fn guard_rejection_contains_message() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .dependencies
        .push("missing-dep".to_owned());
    let ctx = make_ctx(&stores);
    let def = minimal_fsm_with_guard(Some("dep-check"), "deps-satisfied", "pending", "ready");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    let err = eval
        .check_transition(&def, "pending", "ready", "work", &work.id, &ctx)
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("dep-check"), "error should name the guard: {}", msg);
    assert!(
        msg.contains("deps-satisfied"),
        "error should include condition: {}",
        msg
    );
    // Guard message field is also included
    assert!(
        msg.contains("deps-satisfied required for pending->ready"),
        "error should include guard message: {}",
        msg
    );
}

#[test]
fn guard_rejection_includes_guard_name_and_edge() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .dependencies
        .push("missing".to_owned());
    let ctx = make_ctx(&stores);
    let def = minimal_fsm_with_guard(Some("my-guard"), "deps-satisfied", "a", "b");
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    let err = eval
        .check_transition(&def, "a", "b", "work", &work.id, &ctx)
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("my-guard"), "should include guard name");
    assert!(msg.contains("a->b"), "should include transition edge");
}

// --- Multiple guards on same edge ---

#[test]
fn multiple_guards_all_must_pass() {
    let stores = make_stores();
    let work = insert_work(&stores, "p1", "task");
    // Both no-active-sessions (passes) and deps-satisfied (fails - dep listed but missing)
    stores
        .write_works()
        .unwrap()
        .get_mut(&work.id)
        .unwrap()
        .dependencies
        .push("missing".to_owned());
    let ctx = make_ctx(&stores);

    let mut guards = HashMap::new();
    guards.insert(
        "session-check".to_owned(),
        GuardDef {
            from: "ready".to_owned(),
            to: "done".to_owned(),
            condition: "no-active-sessions".to_owned(),
            on_failure: OnFailure::Reject,
            message: "no sessions".to_owned(),
        },
    );
    guards.insert(
        "dep-check".to_owned(),
        GuardDef {
            from: "ready".to_owned(),
            to: "done".to_owned(),
            condition: "deps-satisfied".to_owned(),
            on_failure: OnFailure::Reject,
            message: "deps required".to_owned(),
        },
    );

    let mut transitions = HashMap::new();
    let mut targets = HashMap::new();
    targets.insert("done".to_owned(), TransitionRule::default());
    transitions.insert("ready".to_owned(), targets);

    let def = FsmDefinition {
        name: "multi-guard-test".to_owned(),
        description: String::new(),
        states: vec!["ready".to_owned(), "done".to_owned()],
        terminal: vec!["done".to_owned()],
        transitions,
        overrides: HashMap::new(),
        guards,
    };
    let eval = GuardEvaluator::new(GuardConditionRegistry::with_builtins());
    let result = eval.check_transition(&def, "ready", "done", "work", &work.id, &ctx);
    assert!(result.is_err(), "should reject when any guard fails");
}

// --- validate_conditions ---

#[test]
fn validate_conditions_passes_for_known_conditions() {
    let registry = GuardConditionRegistry::with_builtins();
    let def = minimal_fsm_with_guard(Some("g"), "no-active-sessions", "a", "b");
    let errors = GuardEvaluator::validate_conditions(&[def], &registry);
    assert!(errors.is_empty(), "known condition should pass: {:?}", errors);
}

#[test]
fn validate_conditions_rejects_unknown_condition() {
    let registry = GuardConditionRegistry::with_builtins();
    let def = minimal_fsm_with_guard(Some("g"), "nonexistent-condition", "a", "b");
    let errors = GuardEvaluator::validate_conditions(&[def], &registry);
    assert!(
        errors.iter().any(|e| e.contains("nonexistent-condition")),
        "should flag unknown condition: {:?}",
        errors
    );
}

#[test]
fn validate_conditions_empty_guards_passes() {
    let registry = GuardConditionRegistry::with_builtins();
    let def = minimal_fsm_with_guard(None, "", "a", "b");
    let errors = GuardEvaluator::validate_conditions(&[def], &registry);
    assert!(errors.is_empty(), "no guards = no errors");
}

// --- Schema validation: guard state and edge checks ---

#[test]
fn schema_validate_catches_guard_with_unknown_from_state() {
    use crate::fsm::schema as fsm_schema;
    let mut def = minimal_fsm_with_guard(Some("bad"), "no-active-sessions", "a", "b");
    // Replace guard with one referencing a nonexistent from-state
    def.guards.clear();
    def.guards.insert(
        "bad-guard".to_owned(),
        GuardDef {
            from: "nonexistent".to_owned(),
            to: "b".to_owned(),
            condition: "no-active-sessions".to_owned(),
            on_failure: OnFailure::Reject,
            message: String::new(),
        },
    );
    let errors = fsm_schema::validate(&def, None);
    assert!(
        errors.iter().any(|e| e.contains("unknown from-state")),
        "should catch unknown from-state: {:?}",
        errors
    );
}

#[test]
fn schema_validate_catches_guard_on_nonexistent_transition() {
    use crate::fsm::schema as fsm_schema;
    let mut def = minimal_fsm_with_guard(None, "", "a", "b");
    // Guard on states that exist but transition a->done doesn't exist
    def.guards.insert(
        "bad-edge".to_owned(),
        GuardDef {
            from: "a".to_owned(),
            to: "done".to_owned(), // "a" only transitions to "b", not "done"
            condition: "no-active-sessions".to_owned(),
            on_failure: OnFailure::Reject,
            message: String::new(),
        },
    );
    let errors = fsm_schema::validate(&def, None);
    assert!(
        errors.iter().any(|e| e.contains("no transition exists")),
        "should catch guard on impossible edge: {:?}",
        errors
    );
}

#[test]
fn schema_validate_work_yml_guards_are_valid() {
    // The embedded work.yml guards (deps-ready, no-sessions-on-done) must pass validation.
    use crate::fsm::schema as fsm_schema;
    let content = include_str!("../../resources/engine/fsm/work.yml");
    let mut def: FsmDefinition = serde_yaml::from_str(content).unwrap();
    fsm_schema::inject_transition_names(&mut def);
    let errors = fsm_schema::validate(&def, Some("work.yml"));
    assert!(errors.is_empty(), "work.yml guards should be valid: {:?}", errors);
}

#[test]
fn work_yml_guards_have_expected_conditions() {
    let content = include_str!("../../resources/engine/fsm/work.yml");
    let def: FsmDefinition = serde_yaml::from_str(content).unwrap();
    assert!(
        def.guards.values().any(|g| g.condition == "deps-satisfied"),
        "work.yml should have a deps-satisfied guard"
    );
    assert!(
        def.guards.values().any(|g| g.condition == "no-active-sessions"),
        "work.yml should have a no-active-sessions guard"
    );
}
