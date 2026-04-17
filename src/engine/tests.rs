use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use super::schema::{self, ActionStep, StrategyDefinition};
use crate::primitive::types::{
    Idempotency, InputField, OutputField, OutputType, Primitive, PrimitiveContext, PrimitiveOutput,
};

fn strategies_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("resources/engine/strategies");
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
    assert!(
        !defs.is_empty(),
        "expected strategy YAML files under resources/engine/strategies/"
    );
}

#[test]
fn all_default_strategies_pass_structural_validation() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let errors = schema::validate(&defs);
    assert!(errors.is_empty(), "structural validation errors: {:?}", errors);
}

#[test]
fn load_from_resources_returns_all_strategies() {
    let defs = schema::load_from_resources(None).unwrap();
    assert!(
        !defs.is_empty(),
        "expected strategy definitions from embedded resources"
    );
}

#[test]
fn load_from_resources_includes_both_engine_and_decompose_strategies() {
    // load_from_resources covers engine/strategies/ + decompose/strategies/ -
    // it should return strictly more definitions than either directory alone.
    let engine_defs = schema::load_dir(&strategies_dir()).unwrap();
    let mut decompose_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    decompose_dir.push("resources/decompose/strategies");
    let decompose_defs = schema::load_dir(&decompose_dir).unwrap();
    let all_defs = schema::load_from_resources(None).unwrap();
    assert!(
        all_defs.len() >= engine_defs.len() + decompose_defs.len(),
        "load_from_resources ({}) should include engine ({}) + decompose ({}) strategies",
        all_defs.len(),
        engine_defs.len(),
        decompose_defs.len()
    );
}

#[test]
fn load_from_resources_passes_structural_validation() {
    let defs = schema::load_from_resources(None).unwrap();
    let errors = schema::validate(&defs);
    assert!(
        errors.is_empty(),
        "resource-loaded strategies should pass validation: {:?}",
        errors
    );
}

#[test]
fn loaded_strategies_cover_all_v3_behaviors() {
    let defs = schema::load_dir(&strategies_dir()).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();

    // Exact-count guard. Update this number AND add a .contains() assertion below whenever
    // you add or remove a strategy. Subset-only coverage checks let new strategies slip in
    // without review; the length assert makes drift fail CI loudly.
    assert_eq!(
        names.len(),
        28,
        "strategy loader should expose exactly 28 strategies; got {:?}",
        names
    );

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
        names.contains(&"restart-director-on-event"),
        "missing restart-director-on-event"
    );
    assert!(
        names.contains(&"restart-director-on-state"),
        "missing restart-director-on-state"
    );

    // Agent lifecycle (5 strategies - v4 cutover)
    assert!(
        names.contains(&"spawn-implementer-for-ready-work"),
        "missing spawn-implementer-for-ready-work"
    );
    assert!(
        names.contains(&"auto-triage-proposed-bundle"),
        "missing auto-triage-proposed-bundle"
    );
    assert!(
        names.contains(&"spawn-reviewer-for-triaged-bundle"),
        "missing spawn-reviewer-for-triaged-bundle"
    );
    assert!(
        names.contains(&"accept-approved-bundle"),
        "missing accept-approved-bundle"
    );
    assert!(
        names.contains(&"handle-rejected-bundle"),
        "missing handle-rejected-bundle"
    );

    // Completion (1 strategy - v4 cutover)
    assert!(
        names.contains(&"complete-plan-on-goal"),
        "missing complete-plan-on-goal"
    );

    // Git lifecycle (3 strategies - v4 cutover)
    assert!(
        names.contains(&"create-integration-branch-on-plan-active"),
        "missing create-integration-branch-on-plan-active"
    );
    assert!(
        names.contains(&"merge-integration-to-main-on-goal"),
        "missing merge-integration-to-main-on-goal"
    );
    assert!(
        names.contains(&"delete-integration-branch-on-abandon"),
        "missing delete-integration-branch-on-abandon"
    );

    // Conflict resolution (1 strategy - v4 cutover)
    assert!(
        names.contains(&"resolve-structural-conflict"),
        "missing resolve-structural-conflict"
    );

    // Feedback (1 strategy - v4 cutover)
    assert!(
        names.contains(&"revise-parent-on-impossible-spec"),
        "missing revise-parent-on-impossible-spec"
    );

    // Tooling (1 strategy - v4 cutover)
    assert!(
        names.contains(&"register-tools-on-phase-active"),
        "missing register-tools-on-phase-active"
    );

    // Recovery additions (2 strategies - v4 cutover)
    assert!(
        names.contains(&"handle-work-sla-breach"),
        "missing handle-work-sla-breach"
    );
    assert!(names.contains(&"escalate-to-director"), "missing escalate-to-director");

    // Tick validation (1 strategy)
    assert!(
        names.contains(&"validate-and-publish-tick"),
        "missing validate-and-publish-tick"
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
    assert_eq!(by_name["restart-director-on-event"].scope, "session");
    assert_eq!(by_name["restart-director-on-state"].scope, "plan");
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
    let restart_state = defs.iter().find(|d| d.name == "restart-director-on-state").unwrap();
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

// ─── Engine tick tests ──────────────────────────────────────────────────────

// Test primitive that always succeeds and records its invocation.
struct RecordingPrimitive {
    name: &'static str,
}

impl Primitive for RecordingPrimitive {
    fn name(&self) -> &'static str {
        self.name
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async move {
            Ok(PrimitiveOutput {
                values: params.as_object().cloned().unwrap_or_default().into_iter().collect(),
                summary: format!("{} executed", self.name),
            })
        })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![OutputField {
            name: "result".to_owned(),
            field_type: OutputType::String,
        }]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

// Test primitive that always fails.
struct FailingPrimitive;

impl Primitive for FailingPrimitive {
    fn name(&self) -> &'static str {
        "always-fail"
    }

    fn execute<'a>(
        &'a self,
        _ctx: &'a mut PrimitiveContext<'_>,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<PrimitiveOutput>> + Send + 'a>> {
        Box::pin(async { eyre::bail!("intentional test failure") })
    }

    fn output_schema(&self) -> Vec<OutputField> {
        vec![]
    }

    fn input_schema(&self) -> Vec<InputField> {
        vec![]
    }

    fn idempotency(&self) -> Idempotency {
        Idempotency::Idempotent
    }
}

fn test_registry() -> crate::primitive::registry::PrimitiveRegistry {
    let mut reg = crate::primitive::registry::PrimitiveRegistry::new();
    reg.register(Box::new(RecordingPrimitive { name: "promote-record" }))
        .unwrap();
    reg.register(Box::new(RecordingPrimitive {
        name: "complete-record",
    }))
    .unwrap();
    reg.register(Box::new(RecordingPrimitive { name: "retry-work" }))
        .unwrap();
    reg.register(Box::new(RecordingPrimitive {
        name: "check-threshold",
    }))
    .unwrap();
    reg.register(Box::new(RecordingPrimitive {
        name: "increment-failure-count",
    }))
    .unwrap();
    reg.register(Box::new(RecordingPrimitive { name: "abandon-work" }))
        .unwrap();
    reg.register(Box::new(FailingPrimitive)).unwrap();
    reg
}

fn test_trigger_evaluator(
    triggers: Vec<crate::trigger::schema::TriggerDefinition>,
) -> crate::trigger::evaluate::TriggerEvaluator {
    let sq = crate::trigger::observe::StateQueryRegistry::with_builtins();
    crate::trigger::evaluate::TriggerEvaluator::new(triggers, sq)
}

fn test_engine_context<'a>(
    stores: &'a crate::daemon::context::Stores,
    events: &'a [crate::ipc::protocol::DaemonEvent],
    event_tx: &'a tokio::sync::broadcast::Sender<crate::ipc::protocol::DaemonEvent>,
    bridge: &'a crate::agents::bridge::AgentIpcBridge,
    repo_path: &'a std::path::Path,
    worktree_mgr: &'a crate::worktree::manager::WorktreeManager,
) -> super::tick::EngineContext<'a> {
    super::tick::EngineContext {
        stores,
        events,
        event_tx,
        bridge,
        repo_path,
        worktree_mgr,
        now: chrono::Utc::now().timestamp_millis(),
        guard_conditions: None,
    }
}

/// Build the shared test infrastructure needed by engine tick tests.
fn test_infra(
    dir: &std::path::Path,
) -> (
    Arc<crate::daemon::context::Stores>,
    tokio::sync::broadcast::Sender<crate::ipc::protocol::DaemonEvent>,
    crate::agents::bridge::AgentIpcBridge,
    crate::worktree::manager::WorktreeManager,
) {
    let stores = Arc::new(crate::daemon::context::Stores::new());
    let (tx, _) = tokio::sync::broadcast::channel(64);
    let wm = crate::worktree::manager::WorktreeManager::new(dir.to_path_buf(), dir.join("worktrees"));
    let bridge = crate::agents::bridge::AgentIpcBridge::new(
        stores.clone(),
        tx.clone(),
        wm.clone(),
        stores.config.clone(),
        stores.fsm.clone(),
    );
    (stores, tx, bridge, wm)
}

#[tokio::test]
async fn tick_no_triggers_converges_immediately() {
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-empty");
    let (stores, tx, bridge, wm) = test_infra(&dir);
    let events: Vec<crate::ipc::protocol::DaemonEvent> = vec![];

    let te = test_trigger_evaluator(vec![]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 0);
    assert_eq!(outcome.convergence_iterations, 1);
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_event_trigger_fires_strategy() {
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-event");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    // Create an event trigger
    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    // Strategy that fires on the event trigger
    let strategy = StrategyDefinition {
        name: "handle-test-event".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "promote-record".to_owned(),
            guard: None,
            params: [("id".to_owned(), serde_json::json!("$trigger.scope-id"))].into(),
        }],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };

    // Event on the bus
    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-123"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 1);
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_priority_ordering() {
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-priority");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let low_priority = StrategyDefinition {
        name: "low-priority".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 50,
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
    };

    let high_priority = StrategyDefinition {
        name: "high-priority".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 1000,
        action: vec![ActionStep {
            name: None,
            primitive: "promote-record".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-1"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    // Note: low_priority is first in the vec, but high_priority should fire first
    let mut engine = super::tick::CompositionEngine::new(vec![low_priority, high_priority], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 2);
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_on_failure_wiring_fires_when_action_fails() {
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-failure");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "failing-strategy".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "always-fail".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: vec![ActionStep {
            name: None,
            primitive: "promote-record".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_failure: vec![ActionStep {
            name: None,
            primitive: "abandon-work".to_owned(),
            guard: None,
            params: [("work-id".to_owned(), serde_json::json!("$trigger.scope-id"))].into(),
        }],
        enabled: true,
        cooldown_secs: None,
    };

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-99"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 1);
    assert!(outcome.had_failures);
}

#[tokio::test]
async fn tick_disabled_strategy_is_skipped() {
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-disabled");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "disabled-strategy".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "promote-record".to_owned(),
            guard: None,
            params: HashMap::new(),
        }],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: false,
        cooldown_secs: None,
    };

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-1"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 0);
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_scope_id_explosion() {
    // A trigger that fires for multiple scope IDs should produce one execution per ID
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-explode");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "multi.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "multi-scope".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: None,
            primitive: "promote-record".to_owned(),
            guard: None,
            params: [("id".to_owned(), serde_json::json!("$trigger.scope-id"))].into(),
        }],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };

    // Two events with different work IDs
    let events = vec![
        crate::ipc::protocol::DaemonEvent {
            event: "multi.fired".to_owned(),
            data: serde_json::json!({"work_id": "wi-1"}),
        },
        crate::ipc::protocol::DaemonEvent {
            event: "multi.fired".to_owned(),
            data: serde_json::json!({"work_id": "wi-2"}),
        },
    ];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    // Should fire once per unique scope_id
    assert_eq!(outcome.strategies_fired, 2);
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_context_passes_between_steps() {
    // Verify that a named step's output is available to subsequent steps via $context
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-ctx");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "context-chain".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: Some("step-one".to_owned()),
                primitive: "check-threshold".to_owned(),
                guard: None,
                params: [("id".to_owned(), serde_json::json!("$trigger.scope-id"))].into(),
            },
            ActionStep {
                name: None,
                primitive: "retry-work".to_owned(),
                guard: None,
                params: [("ref-id".to_owned(), serde_json::json!("$context.step-one.id"))].into(),
            },
        ],
        on_success: Vec::new(),
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-42"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 1);
    assert!(!outcome.had_failures);
}

// ─── Phase 3: Registry validation tests ──────────────────────────────────────

fn full_primitive_registry() -> crate::primitive::registry::PrimitiveRegistry {
    let mut reg = crate::primitive::registry::PrimitiveRegistry::new();
    crate::primitive::catalog::register_all(&mut reg).unwrap();
    reg
}

fn triggers_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("resources/engine/triggers");
    dir
}

#[test]
fn all_strategy_primitives_exist_in_registry() {
    let strategies = schema::load_from_resources(None).unwrap();
    let registry = full_primitive_registry();

    let mut missing = Vec::new();
    for strategy in &strategies {
        for step in strategy
            .action
            .iter()
            .chain(strategy.on_success.iter())
            .chain(strategy.on_failure.iter())
        {
            if registry.get(&step.primitive).is_none() {
                missing.push(format!(
                    "strategy '{}': primitive '{}' not in registry",
                    strategy.name, step.primitive
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "primitives missing from registry:\n{}",
        missing.join("\n")
    );
}

#[test]
fn all_strategy_triggers_exist_in_trigger_definitions() {
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let triggers = crate::trigger::schema::load_dir(&triggers_dir()).unwrap();
    let trigger_names: std::collections::HashSet<&str> = triggers.iter().map(|t| t.name.as_str()).collect();

    let mut missing = Vec::new();
    for strategy in &strategies {
        if !trigger_names.contains(strategy.trigger.as_str()) {
            missing.push(format!(
                "strategy '{}': trigger '{}' not defined",
                strategy.name, strategy.trigger
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "triggers missing from definitions:\n{}",
        missing.join("\n")
    );
}

#[test]
fn validate_cross_references_passes_for_valid_strategies() {
    let triggers = crate::trigger::schema::load_dir(&triggers_dir()).unwrap();
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let results = schema::validate_cross_references(&strategies, &triggers);
    let errors: Vec<_> = results
        .iter()
        .filter(|r| matches!(r.severity, schema::Severity::Error))
        .collect();
    assert!(
        errors.is_empty(),
        "all embedded strategies should reference valid triggers: {:?}",
        errors.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}

#[test]
fn validate_cross_references_catches_unknown_trigger() {
    let triggers = crate::trigger::schema::load_dir(&triggers_dir()).unwrap();
    let bad_strategy = StrategyDefinition {
        name: "bad-strategy".to_owned(),
        description: String::new(),
        trigger: "nonexistent-trigger".to_owned(),
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
    };
    let results = schema::validate_cross_references(&[bad_strategy], &triggers);
    assert!(
        results
            .iter()
            .any(|r| matches!(r.severity, schema::Severity::Error) && r.message.contains("nonexistent-trigger")),
        "expected error for unknown trigger, got: {:?}",
        results.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}

#[test]
fn validate_cross_references_warns_for_strategy_referencing_disabled_trigger() {
    use crate::trigger::schema::{Operator, TriggerDefinition, TriggerKind};
    let disabled_trigger = TriggerDefinition {
        name: "disabled-trigger".to_owned(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".to_owned(),
            field: "attempt-count".to_owned(),
            operator: Operator::Gte,
            value: 3.0,
        },
    };
    let strategy = StrategyDefinition {
        name: "strat-using-disabled".to_owned(),
        description: String::new(),
        trigger: "disabled-trigger".to_owned(),
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
    };
    let results = schema::validate_cross_references(&[strategy], &[disabled_trigger]);
    assert!(
        results
            .iter()
            .any(|r| matches!(r.severity, schema::Severity::Warn) && r.message.contains("disabled-trigger")),
        "expected Warn for strategy referencing disabled trigger, got: {:?}",
        results.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}

#[test]
fn validate_cross_references_errors_for_not_composite_with_disabled_sub_trigger() {
    use crate::trigger::schema::{CompositeOperator, Operator, TriggerDefinition, TriggerKind};
    let disabled_sub = TriggerDefinition {
        name: "disabled-sub".to_owned(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".to_owned(),
            field: "attempt-count".to_owned(),
            operator: Operator::Gte,
            value: 1.0,
        },
    };
    let not_composite = TriggerDefinition {
        name: "not-of-disabled".to_owned(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::Not,
            triggers: vec!["disabled-sub".to_owned()],
        },
    };
    let results = schema::validate_cross_references(&[], &[disabled_sub, not_composite]);
    assert!(
        results
            .iter()
            .any(|r| matches!(r.severity, schema::Severity::Error) && r.message.contains("not-of-disabled")),
        "expected Error for NOT composite with disabled sub-trigger, got: {:?}",
        results.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}

#[test]
fn validate_cross_references_warns_for_and_composite_with_disabled_sub_trigger() {
    use crate::trigger::schema::{CompositeOperator, Operator, TriggerDefinition, TriggerKind};
    let disabled_sub = TriggerDefinition {
        name: "disabled-arm".to_owned(),
        enabled: false,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".to_owned(),
            field: "attempt-count".to_owned(),
            operator: Operator::Gte,
            value: 1.0,
        },
    };
    let enabled_sub = TriggerDefinition {
        name: "enabled-arm".to_owned(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Threshold {
            scope: "work".to_owned(),
            field: "attempt-count".to_owned(),
            operator: Operator::Gte,
            value: 1.0,
        },
    };
    let and_composite = TriggerDefinition {
        name: "and-with-disabled".to_owned(),
        enabled: true,
        cooldown_secs: None,
        kind: TriggerKind::Composite {
            operator: CompositeOperator::And,
            triggers: vec!["disabled-arm".to_owned(), "enabled-arm".to_owned()],
        },
    };
    let results = schema::validate_cross_references(&[], &[disabled_sub, enabled_sub, and_composite]);
    assert!(
        results
            .iter()
            .any(|r| matches!(r.severity, schema::Severity::Warn) && r.message.contains("and-with-disabled")),
        "expected Warn for AND composite with disabled sub-trigger, got: {:?}",
        results.iter().map(|r| &r.message).collect::<Vec<_>>()
    );
}

#[test]
fn all_strategy_params_pass_primitive_validation() {
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let registry = full_primitive_registry();

    let mut errors = Vec::new();
    for strategy in &strategies {
        for step in strategy
            .action
            .iter()
            .chain(strategy.on_success.iter())
            .chain(strategy.on_failure.iter())
        {
            if let Some(prim) = registry.get(&step.primitive) {
                let params = resolve_for_validation(&step.params);
                if let Err(e) = prim.validate_params(&params) {
                    errors.push(format!(
                        "strategy '{}' primitive '{}': {}",
                        strategy.name, step.primitive, e
                    ));
                }
            }
        }
    }

    assert!(errors.is_empty(), "param validation errors:\n{}", errors.join("\n"));
}

/// Build a params Value suitable for validate_params() by replacing
/// `$trigger.*` and `$context.*` references with placeholder strings.
fn resolve_for_validation(params: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    let mut resolved = serde_json::Map::new();
    for (k, v) in params {
        resolved.insert(k.clone(), resolve_value_for_validation(v));
    }
    serde_json::Value::Object(resolved)
}

fn resolve_value_for_validation(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) if s.starts_with('$') => serde_json::Value::String("placeholder".to_owned()),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(resolve_value_for_validation).collect())
        }
        serde_json::Value::Object(map) => {
            let resolved: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), resolve_value_for_validation(v)))
                .collect();
            serde_json::Value::Object(resolved)
        }
        other => other.clone(),
    }
}

#[test]
fn v3_reconciliation_strategies_use_correct_priority_ordering() {
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = strategies.iter().map(|d| (d.name.as_str(), d)).collect();

    let promote_specs = by_name["promote-pending-specs"].priority;
    let promote_phases = by_name["promote-pending-phases"].priority;
    let promote_works = by_name["promote-pending-works"].priority;
    let complete_phases = by_name["complete-phases"].priority;
    let complete_specs = by_name["complete-specs"].priority;

    assert!(promote_specs > promote_phases, "specs should promote before phases");
    assert!(promote_phases > promote_works, "phases should promote before works");
    assert!(complete_phases > complete_specs, "phases should complete before specs");
}

#[test]
fn v3_safety_nets_have_highest_priority() {
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = strategies.iter().map(|d| (d.name.as_str(), d)).collect();

    let hard_cap = by_name["work-attempt-hard-cap"].priority;
    let ratio_escalation = by_name["abandon-ratio-escalation"].priority;
    // v4-cutover renamed the supervisor strategy: Coordinator -> Director.
    let supervisor = by_name["restart-director-on-event"].priority;

    assert!(hard_cap >= 1000, "hard cap priority {} < 1000", hard_cap);
    assert!(
        ratio_escalation >= 1000,
        "ratio escalation priority {} < 1000",
        ratio_escalation
    );
    assert!(supervisor >= 1000, "supervisor priority {} < 1000", supervisor);

    let retry = by_name["work-retry-on-failure"].priority;
    assert!(hard_cap > retry, "hard cap must fire before retry");
}

#[test]
fn engine_constructs_with_real_registries() {
    let strategies = schema::load_dir(&strategies_dir()).unwrap();
    let triggers = crate::trigger::schema::load_dir(&triggers_dir()).unwrap();
    let registry = full_primitive_registry();
    let sq = crate::trigger::observe::StateQueryRegistry::with_builtins();
    let te = crate::trigger::evaluate::TriggerEvaluator::new(triggers, sq);

    let engine = super::tick::CompositionEngine::new(strategies, registry, te);
    drop(engine);
}

// ─── Architect review fixes ──────────────────────────────────────────────────

#[tokio::test]
async fn tick_context_passes_from_action_to_on_success() {
    // Verify that $context references in on_success can see named step outputs from action.
    // This is the bug the Architect caught: strategy_ctx was previously scoped inside
    // execute_steps, so on_success got a fresh empty context.
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-ctx-wiring");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "ctx-wiring".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![ActionStep {
            name: Some("threshold-check".to_owned()),
            primitive: "check-threshold".to_owned(),
            guard: None,
            params: [("id".to_owned(), serde_json::json!("$trigger.scope-id"))].into(),
        }],
        on_success: vec![ActionStep {
            name: None,
            primitive: "retry-work".to_owned(),
            guard: None,
            // This $context reference reaches back into the action sequence.
            // Before the fix, this would fail with "step 'threshold-check' has no output".
            params: [("ref".to_owned(), serde_json::json!("$context.threshold-check.id"))].into(),
        }],
        on_failure: Vec::new(),
        enabled: true,
        cooldown_secs: None,
    };

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-77"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    let mut ctx = test_engine_context(&stores, &events, &tx, &bridge, &dir, &wm);
    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 1);
    // The key assertion: no failures means $context resolved successfully across the boundary
    assert!(!outcome.had_failures);
}

#[tokio::test]
async fn tick_guard_skips_step_but_strategy_succeeds() {
    // Verify that a failed guard skips the guarded step but the strategy still succeeds.
    let dir = crate::test_util::TestDir::new("loopr-engine-tick-guard");
    let (stores, tx, bridge, wm) = test_infra(&dir);

    let trigger = crate::trigger::schema::TriggerDefinition {
        name: "test-event".to_owned(),
        kind: crate::trigger::schema::TriggerKind::Event {
            event: "test.fired".to_owned(),
            scope: Some("work".to_owned()),
            match_filter: HashMap::new(),
            throttle_secs: None,
        },
        enabled: true,
        cooldown_secs: None,
    };

    let strategy = StrategyDefinition {
        name: "guarded-strategy".to_owned(),
        description: String::new(),
        trigger: "test-event".to_owned(),
        scope: "work".to_owned(),
        priority: 100,
        action: vec![
            ActionStep {
                name: None,
                // This step has a guard that will fail (unknown guard -> false when registry exists)
                primitive: "promote-record".to_owned(),
                guard: Some("nonexistent-guard".to_owned()),
                params: HashMap::new(),
            },
            ActionStep {
                name: None,
                // This step has no guard, so it should still execute
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

    let events = vec![crate::ipc::protocol::DaemonEvent {
        event: "test.fired".to_owned(),
        data: serde_json::json!({"work_id": "wi-guard"}),
    }];

    let te = test_trigger_evaluator(vec![trigger]);
    let registry = test_registry();
    let mut engine = super::tick::CompositionEngine::new(vec![strategy], registry, te);

    // Use a guard registry so that unknown guards evaluate to false (not the no-registry passthrough)
    let guard_registry = crate::trigger::observe::GuardConditionRegistry::with_builtins();
    let mut ctx = super::tick::EngineContext {
        stores: &stores,
        events: &events,
        event_tx: &tx,
        bridge: &bridge,
        repo_path: &dir,
        worktree_mgr: &wm,
        now: chrono::Utc::now().timestamp_millis(),
        guard_conditions: Some(&guard_registry),
    };

    let outcome = engine.tick(&mut ctx).await.unwrap();
    assert_eq!(outcome.strategies_fired, 1);
    // Strategy succeeds even though the first step was skipped by the guard
    assert!(!outcome.had_failures);
}

// ─── Decomposition strategy tests ────────────────────────────────────────────

#[test]
fn decomposition_strategies_are_loaded() {
    let defs = schema::load_from_resources(None).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    for required in &[
        "decompose-plan",
        "decompose-spec",
        "decompose-phase",
        "classify-and-configure",
        "validate-after-decomposition",
        "ratify-spec-level",
        "re-decompose-on-gaps",
    ] {
        assert!(names.contains(required), "missing decomposition strategy: {}", required);
    }
}

#[test]
fn decomposition_strategies_have_correct_scopes() {
    let defs = schema::load_from_resources(None).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
    assert_eq!(by_name["decompose-plan"].scope, "plan");
    assert_eq!(by_name["decompose-spec"].scope, "spec");
    assert_eq!(by_name["decompose-phase"].scope, "phase");
    assert_eq!(by_name["classify-and-configure"].scope, "plan");
    assert_eq!(by_name["re-decompose-on-gaps"].scope, "plan");
}

#[test]
fn decomposition_strategies_have_correct_priorities() {
    let defs = schema::load_from_resources(None).unwrap();
    let by_name: HashMap<&str, &StrategyDefinition> = defs.iter().map(|d| (d.name.as_str(), d)).collect();
    assert_eq!(by_name["classify-and-configure"].priority, 1000);
    assert_eq!(by_name["decompose-plan"].priority, 850);
    assert_eq!(by_name["decompose-spec"].priority, 850);
    assert_eq!(by_name["decompose-phase"].priority, 850);
    assert_eq!(by_name["validate-after-decomposition"].priority, 800);
    assert_eq!(by_name["ratify-spec-level"].priority, 790);
    assert_eq!(by_name["re-decompose-on-gaps"].priority, 780);
}

#[test]
fn classify_and_configure_has_named_classify_step() {
    let defs = schema::load_from_resources(None).unwrap();
    let def = defs.iter().find(|d| d.name == "classify-and-configure").unwrap();
    let named: Vec<&str> = def.action.iter().filter_map(|s| s.name.as_deref()).collect();
    assert!(
        named.contains(&"classify"),
        "classify-and-configure should have a 'classify' named step"
    );
}

#[test]
fn roles_are_not_loaded_as_strategies() {
    // Verifies that decompose/roles/ is not included in strategy loading.
    // Role config YAML has a different schema; mixing it in would cause parse errors.
    let defs = schema::load_from_resources(None).unwrap();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    // Role config files are keyed under "decomposer", not valid strategy names.
    assert!(
        !names.contains(&"decomposer"),
        "decompose/roles/ was loaded as strategy; 'decomposer' should not appear as a strategy"
    );
}
