use std::collections::HashMap;
use std::path::PathBuf;

use super::schema::{self, CompositeOperator, CountQuery, Operator, TriggerDefinition, TriggerKind};

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
