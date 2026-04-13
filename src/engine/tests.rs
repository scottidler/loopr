use std::collections::HashMap;
use std::path::PathBuf;

use super::schema::{self, ActionStep, StrategyDefinition};

fn strategies_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("strategies");
    dir
}

fn minimal_strategy(name: &str) -> StrategyDefinition {
    StrategyDefinition {
        name: name.to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "retry-work".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    }
}

// ─── YAML loading ─────────────────────────────────────────────────────────────

#[test]
fn load_all_strategy_yaml_files() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    assert!(!defs.is_empty(), "expected strategy YAML files under strategies/");
}

#[test]
fn all_default_strategies_pass_structural_validation() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let errors = schema::validate(&defs);
    assert!(errors.is_empty(), "structural validation errors: {:?}", errors);
}

#[test]
fn loaded_strategies_cover_all_v3_behaviors() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    // Recovery (3 strategies)
    assert!(
        names.contains(&"work-retry-on-failure"),
        "missing work-retry-on-failure"
    );
    assert!(
        names.contains(&"work-attempt-hard-cap"),
        "missing work-attempt-hard-cap"
    );
    assert!(
        names.contains(&"abandon-ratio-escalation"),
        "missing abandon-ratio-escalation"
    );

    // Reconciliation (5 strategies)
    assert!(
        names.contains(&"promote-pending-specs"),
        "missing promote-pending-specs"
    );
    assert!(
        names.contains(&"promote-pending-phases"),
        "missing promote-pending-phases"
    );
    assert!(
        names.contains(&"promote-pending-works"),
        "missing promote-pending-works"
    );
    assert!(names.contains(&"complete-phases"), "missing complete-phases");
    assert!(names.contains(&"complete-specs"), "missing complete-specs");

    // Sweeps (2 strategies)
    assert!(
        names.contains(&"sweep-integrated-to-done"),
        "missing sweep-integrated-to-done"
    );
    assert!(names.contains(&"sweep-stuck-inreview"), "missing sweep-stuck-inreview");

    // Integration (1 strategy)
    assert!(
        names.contains(&"integrate-accepted-bundles"),
        "missing integrate-accepted-bundles"
    );

    // Supervision (2 strategies)
    assert!(
        names.contains(&"restart-coordinator-on-event"),
        "missing restart-coordinator-on-event"
    );
    assert!(
        names.contains(&"restart-coordinator-on-state"),
        "missing restart-coordinator-on-state"
    );
}

#[test]
fn strategies_have_correct_scopes() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = defs.iter().map(|d| (d.name.as_str(), d)).collect();

    assert_eq!(by_name["work-retry-on-failure"].scope, "work");
    assert_eq!(by_name["work-attempt-hard-cap"].scope, "work");
    assert_eq!(by_name["abandon-ratio-escalation"].scope, "plan");
    assert_eq!(by_name["promote-pending-specs"].scope, "spec");
    assert_eq!(by_name["promote-pending-phases"].scope, "phase");
    assert_eq!(by_name["promote-pending-works"].scope, "work");
    assert_eq!(by_name["integrate-accepted-bundles"].scope, "plan");
    assert_eq!(by_name["restart-coordinator-on-event"].scope, "session");
    assert_eq!(by_name["restart-coordinator-on-state"].scope, "plan");
}

#[test]
fn strategies_have_correct_priorities() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = defs.iter().map(|d| (d.name.as_str(), d)).collect();

    // Safety nets must be highest priority
    assert!(
        by_name["work-attempt-hard-cap"].priority >= 1000,
        "hard cap should be high priority"
    );
    assert!(
        by_name["abandon-ratio-escalation"].priority >= 1000,
        "ratio escalation should be high priority"
    );
    // Reconciliation should be high but below safety nets
    assert_eq!(by_name["promote-pending-specs"].priority, 900);
    assert_eq!(by_name["sweep-integrated-to-done"].priority, 950);
    assert_eq!(by_name["integrate-accepted-bundles"].priority, 500);
    // Normal operations (default 100 per spec)
    assert_eq!(by_name["work-retry-on-failure"].priority, 100);
}

#[test]
fn work_retry_strategy_has_named_step_for_context() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let def = defs.iter().find(|d| d.name == "work-retry-on-failure").unwrap();
    let named_steps: Vec<&str> = def.action.iter().filter_map(|s| s.name.as_deref()).collect();
    assert!(
        named_steps.contains(&"threshold-check"),
        "work-retry-on-failure should have threshold-check step"
    );
}

#[test]
fn supervision_strategies_have_cooldown() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let restart_state = defs.iter().find(|d| d.name == "restart-coordinator-on-state").unwrap();
    assert!(
        restart_state.cooldown_secs.is_some(),
        "level-triggered supervisor strategy should have a cooldown"
    );
    assert_eq!(restart_state.cooldown_secs.unwrap(), 60);
}

// ─── Structural validation: valid cases ───────────────────────────────────────

#[test]
fn valid_minimal_strategy_passes() {
    let def = minimal_strategy("test");
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

#[test]
fn valid_strategy_with_on_success_passes() {
    let mut def = minimal_strategy("test");
    def.on_success = vec![ActionStep {
        name: None,
        primitive: "complete-record".to_owned(),
        guard: None,
        params: HashMap::new(),
    }];
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(errors.is_empty(), "unexpected errors: {:?}", errors);
}

#[test]
fn valid_context_ref_to_preceding_step_passes() {
    let def = StrategyDefinition {
        name: "ctx-test".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: Some("step-one".to_owned()),
                primitive: "check-threshold".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
            ActionStep {
                name: None,
                primitive: "retry-work".to_owned(),
                guard: None,
                params: [("result".to_owned(), serde_json::json!("$context.step-one.output"))].into(),
            },
        ],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(errors.is_empty(), "valid context ref should pass: {:?}", errors);
}

// ─── Structural validation: rejection cases ───────────────────────────────────

#[test]
fn reject_empty_trigger() {
    let mut def = minimal_strategy("test");
    def.trigger = String::new();
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("trigger must not be empty")),
        "expected empty trigger error: {:?}",
        errors
    );
}

#[test]
fn reject_unknown_scope() {
    let mut def = minimal_strategy("test");
    def.scope = "foobar".to_owned();
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("unknown scope")),
        "expected unknown scope error: {:?}",
        errors
    );
}

#[test]
fn reject_empty_action_sequence() {
    let mut def = minimal_strategy("test");
    def.action = Vec::new();
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("action sequence must not be empty")),
        "expected empty action error: {:?}",
        errors
    );
}

#[test]
fn reject_zero_priority() {
    let mut def = minimal_strategy("test");
    def.priority = 0;
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("priority must be >= 1")),
        "expected zero priority error: {:?}",
        errors
    );
}

#[test]
fn reject_empty_primitive_name_in_step() {
    let mut def = minimal_strategy("test");
    def.action[0].primitive = String::new();
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("empty primitive name")),
        "expected empty primitive error: {:?}",
        errors
    );
}

#[test]
fn reject_context_ref_to_forward_declared_step() {
    // Step 1 references step 2 (which comes AFTER it) - invalid
    let def = StrategyDefinition {
        name: "ctx-forward-test".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: None,
                primitive: "retry-work".to_owned(),
                guard: None,
                params: [("value".to_owned(), serde_json::json!("$context.step-two.output"))].into(),
            },
            ActionStep {
                name: Some("step-two".to_owned()),
                primitive: "check-threshold".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
        ],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("forward-declared")),
        "expected forward-declared reference error: {:?}",
        errors
    );
}

#[test]
fn reject_duplicate_strategy_names() {
    let a = minimal_strategy("same-name");
    let b = minimal_strategy("same-name");
    let errors = schema::validate(&[a, b]);
    assert!(
        errors.iter().any(|e| e.contains("duplicate name")),
        "expected duplicate name error: {:?}",
        errors
    );
}

// ─── Default values ───────────────────────────────────────────────────────────

#[test]
fn default_priority_is_100() {
    let content = r#"
test-strategy:
  trigger: session-failure
  scope: work
  action:
    - primitive: retry-work
"#;
    let defs = schema::load_file(&std::env::temp_dir().join("test-strategy.yml")).unwrap_or_else(|_| {
        // Parse directly from string for this test
        let raw: HashMap<String, StrategyDefinition> = serde_yaml::from_str(content).unwrap();
        raw.into_iter()
            .map(|(name, mut d)| {
                d.name = name;
                d
            })
            .collect()
    });
    assert!(defs.iter().all(|d| d.priority == 100 || d.name != "test-strategy"));
    // Parse directly to verify default
    let raw: HashMap<String, StrategyDefinition> = serde_yaml::from_str(content).unwrap();
    let def = raw.values().next().unwrap();
    assert_eq!(def.priority, 100);
}

#[test]
fn default_enabled_is_true() {
    let content = r#"
test-strategy:
  trigger: session-failure
  scope: work
  action:
    - primitive: retry-work
"#;
    let raw: HashMap<String, StrategyDefinition> = serde_yaml::from_str(content).unwrap();
    let def = raw.values().next().unwrap();
    assert!(def.enabled);
}

// ─── extract_context_refs ─────────────────────────────────────────────────────

#[test]
fn extract_context_refs_finds_step_name() {
    let val = serde_json::json!("$context.my-step.output");
    let refs = schema::extract_context_refs(&val);
    assert_eq!(refs, vec!["my-step"]);
}

#[test]
fn extract_context_refs_ignores_trigger_refs() {
    let val = serde_json::json!("$trigger.scope-id");
    let refs = schema::extract_context_refs(&val);
    assert!(refs.is_empty());
}

#[test]
fn extract_context_refs_finds_nested_refs() {
    let val = serde_json::json!({
        "a": "$context.step-a.val",
        "b": ["$context.step-b.result", "$trigger.scope-id"]
    });
    let mut refs = schema::extract_context_refs(&val);
    refs.sort();
    assert_eq!(refs, vec!["step-a", "step-b"]);
}

#[test]
fn extract_context_refs_empty_for_non_context() {
    let val = serde_json::json!({"key": "plain-value", "num": 42});
    let refs = schema::extract_context_refs(&val);
    assert!(refs.is_empty());
}

// ─── Architect review fixes ───────────────────────────────────────────────────

// Fix 1.1: on_success/on_failure steps see their own preceding steps

#[test]
fn on_success_step_can_reference_preceding_on_success_step() {
    let def = StrategyDefinition {
        name: "chained-success".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "check-threshold".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: vec![
            ActionStep {
                name: Some("step-one".to_owned()),
                primitive: "get-value".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
            ActionStep {
                name: None,
                primitive: "use-value".to_owned(),
                guard: None,
                params: [("input".to_owned(), serde_json::json!("$context.step-one.output"))].into(),
            },
        ],
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.is_empty(),
        "on_success step should see preceding on_success steps: {:?}",
        errors
    );
}

#[test]
fn on_success_step_cannot_forward_reference_within_sequence() {
    let def = StrategyDefinition {
        name: "bad-forward".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "check-threshold".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: vec![
            ActionStep {
                name: None,
                primitive: "use-value".to_owned(),
                guard: None,
                // References step-two which comes AFTER this step
                params: [("input".to_owned(), serde_json::json!("$context.step-two.output"))].into(),
            },
            ActionStep {
                name: Some("step-two".to_owned()),
                primitive: "get-value".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
        ],
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("forward-declared")),
        "forward reference in on_success should be rejected: {:?}",
        errors
    );
}

// Fix 1.2: malformed $context.step (no .field suffix) is rejected

#[test]
fn reject_malformed_context_ref_missing_field() {
    let def = StrategyDefinition {
        name: "bad-ref".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: Some("prev-step".to_owned()),
                primitive: "check-threshold".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
            ActionStep {
                name: None,
                primitive: "retry-work".to_owned(),
                guard: None,
                // Missing .field suffix - malformed
                params: [("value".to_owned(), serde_json::json!("$context.prev-step"))].into(),
            },
        ],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("malformed context reference")),
        "missing .field suffix should be rejected: {:?}",
        errors
    );
}

#[test]
fn find_malformed_context_refs_catches_missing_field() {
    let val = serde_json::json!("$context.step-name");
    let malformed = schema::find_malformed_context_refs(&val);
    assert_eq!(malformed, vec!["$context.step-name"]);
}

#[test]
fn find_malformed_context_refs_ignores_well_formed() {
    let val = serde_json::json!("$context.step-name.output");
    let malformed = schema::find_malformed_context_refs(&val);
    assert!(malformed.is_empty());
}

// Fix 3.1: duplicate step names within a sequence are rejected

#[test]
fn reject_duplicate_step_names_in_action() {
    let def = StrategyDefinition {
        name: "dup-steps".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: Some("my-step".to_owned()),
                primitive: "check-threshold".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
            ActionStep {
                name: Some("my-step".to_owned()), // duplicate
                primitive: "retry-work".to_owned(),
                guard: None,
                params: HashMap::new(),
            },
        ],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.iter().any(|e| e.contains("duplicate step name")),
        "duplicate step name in action should be rejected: {:?}",
        errors
    );
}

#[test]
fn same_step_name_in_action_and_on_success_is_allowed() {
    // on_success is a different sequence; reusing a name from action is valid
    let def = StrategyDefinition {
        name: "reused-name".to_owned(),
        description: String::new(),
        trigger: "session-failure".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: Some("my-step".to_owned()),
            primitive: "check-threshold".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: vec![ActionStep {
            name: Some("my-step".to_owned()), // same name, different sequence - OK
            primitive: "retry-work".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };
    let errors = schema::validate(std::slice::from_ref(&def));
    assert!(
        errors.is_empty(),
        "reusing step name across sequences should be valid: {:?}",
        errors
    );
}
