use std::collections::HashMap;
use std::path::PathBuf;

use super::observe::{GuardConditionRegistry, ObservationCtx, StateQueryRegistry};
use super::schema::{self, CompositeOperator, CountQuery, Operator, TriggerDefinition, TriggerKind};
use crate::daemon::context::Stores;
use crate::domain::phase::Phase;
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::DaemonEvent;

fn strategies_triggers_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("strategies/triggers");
    dir
}

fn minimal_threshold() -> TriggerDefinition {
    TriggerDefinition {
        name: "test-threshold".into(),
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
fn loaded_triggers_cover_all_27_v3_conditions() {
    let defs = schema::load_dir(&strategies_triggers_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    // Threshold triggers (8)
    assert!(
        names.contains(&"work-retry-exhaustion"),
        "missing work-retry-exhaustion"
    );
    assert!(
        names.contains(&"session-failure-limit"),
        "missing session-failure-limit"
    );
    assert!(
        names.contains(&"action-repetition-loop"),
        "missing action-repetition-loop"
    );
    assert!(
        names.contains(&"error-repetition-loop"),
        "missing error-repetition-loop"
    );
    assert!(names.contains(&"parse-failure-limit"), "missing parse-failure-limit");
    assert!(
        names.contains(&"researcher-spawn-limit"),
        "missing researcher-spawn-limit"
    );
    assert!(
        names.contains(&"self-correction-limit"),
        "missing self-correction-limit"
    );
    assert!(names.contains(&"restart-limit"), "missing restart-limit");

    // Ratio trigger (1)
    assert!(
        names.contains(&"abandon-ratio-exceeded"),
        "missing abandon-ratio-exceeded"
    );

    // Event triggers (5)
    assert!(names.contains(&"session-failure"), "missing session-failure");
    assert!(names.contains(&"transition-completed"), "missing transition-completed");
    assert!(names.contains(&"record-created"), "missing record-created");
    assert!(
        names.contains(&"decomposition-completed"),
        "missing decomposition-completed"
    );
    assert!(names.contains(&"decomposition-failed"), "missing decomposition-failed");

    // Timer triggers (2)
    assert!(names.contains(&"work-sla-breach"), "missing work-sla-breach");
    assert!(names.contains(&"goal-timeout"), "missing goal-timeout");

    // State-query triggers (9)
    assert!(
        names.contains(&"phase-children-terminal"),
        "missing phase-children-terminal"
    );
    assert!(
        names.contains(&"spec-children-terminal"),
        "missing spec-children-terminal"
    );
    assert!(
        names.contains(&"hierarchy-deps-terminal"),
        "missing hierarchy-deps-terminal"
    );
    assert!(names.contains(&"work-deps-done"), "missing work-deps-done");
    assert!(names.contains(&"parent-active"), "missing parent-active");
    assert!(names.contains(&"plan-approved"), "missing plan-approved");
    assert!(names.contains(&"hierarchy-exists"), "missing hierarchy-exists");
    assert!(names.contains(&"goal-complete"), "missing goal-complete");
    assert!(names.contains(&"coverage-incomplete"), "missing coverage-incomplete");

    // Composite trigger (1)
    assert!(names.contains(&"work-sla-full-breach"), "missing work-sla-full-breach");
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
fn state_query_registry_has_all_9_builtins() {
    let reg = StateQueryRegistry::with_builtins();
    let names = reg.names();
    for required in &[
        "all-children-terminal",
        "all-children-done",
        "all-deps-terminal",
        "all-deps-done",
        "parent-active",
        "has-children",
        "no-active-sessions",
        "field-equals",
        "field-is-true",
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

// --- GuardConditionRegistry ---

#[test]
fn guard_registry_has_all_4_builtins() {
    let reg = GuardConditionRegistry::with_builtins();
    let names = reg.names();
    for required in &[
        "no-active-sessions",
        "deps-satisfied",
        "validation-passed",
        "all-ac-passing",
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
