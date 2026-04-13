use std::collections::HashMap;
use std::path::PathBuf;

use crate::domain::transition::Transition;

use super::runtime::FsmInterpreter;
use super::schema::{self, FsmDefinition, TransitionRule};

// --- Helper ---

fn test_strategies_dir() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("strategies/fsm");
    dir
}

fn load_test_interpreter() -> FsmInterpreter {
    FsmInterpreter::load(&test_strategies_dir()).unwrap()
}

fn minimal_def(name: &str) -> FsmDefinition {
    FsmDefinition {
        name: name.to_string(),
        description: String::new(),
        states: vec!["a".to_string(), "b".to_string(), "done".to_string()],
        terminal: vec!["done".to_string()],
        transitions: {
            let mut t = HashMap::new();
            let mut a_targets = HashMap::new();
            a_targets.insert("b".to_string(), TransitionRule::default());
            t.insert("a".to_string(), a_targets);
            let mut b_targets = HashMap::new();
            b_targets.insert("done".to_string(), TransitionRule::default());
            t.insert("b".to_string(), b_targets);
            t
        },
        overrides: HashMap::new(),
        guards: HashMap::new(),
    }
}

// --- Schema loading tests ---

#[test]
fn load_all_yaml_files() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    assert_eq!(defs.len(), 5, "expected 5 FSM YAML files");
    let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
    assert!(names.contains(&"work".to_string()));
    assert!(names.contains(&"bundle".to_string()));
    assert!(names.contains(&"hierarchy".to_string()));
    assert!(names.contains(&"tick".to_string()));
    assert!(names.contains(&"agent".to_string()));
}

#[test]
fn all_yaml_files_pass_validation() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    for def in &defs {
        let filename = format!("{}.yml", def.name);
        let errors = schema::validate(def, Some(&filename));
        assert!(errors.is_empty(), "FSM '{}' validation errors: {:?}", def.name, errors);
    }
}

#[test]
fn work_fsm_has_correct_states() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    let work = defs.iter().find(|d| d.name == "work").unwrap();
    assert_eq!(work.states.len(), 9);
    assert_eq!(work.terminal.len(), 2);
    assert!(work.terminal.contains(&"done".to_string()));
    assert!(work.terminal.contains(&"abandoned".to_string()));
}

#[test]
fn bundle_fsm_has_no_overrides() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    let bundle = defs.iter().find(|d| d.name == "bundle").unwrap();
    assert!(bundle.overrides.is_empty() || bundle.overrides.values().all(|m| m.is_empty()));
}

#[test]
fn agent_fsm_has_no_role_restrictions() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    let agent = defs.iter().find(|d| d.name == "agent").unwrap();
    for targets in agent.transitions.values() {
        for rule in targets.values() {
            assert!(rule.by.is_empty(), "agent FSM should have no role restrictions");
        }
    }
}

// --- Validation negative tests ---

#[test]
fn reject_terminal_not_in_states() {
    let mut def = minimal_def("test");
    def.terminal.push("nonexistent".to_string());
    let errors = schema::validate(&def, None);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("terminal state 'nonexistent' not listed"))
    );
}

#[test]
fn reject_transition_source_not_in_states() {
    let mut def = minimal_def("test");
    def.transitions.insert("nonexistent".to_string(), HashMap::new());
    let errors = schema::validate(&def, None);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("transition source 'nonexistent' not listed"))
    );
}

#[test]
fn reject_transition_target_not_in_states() {
    let mut def = minimal_def("test");
    let mut targets = HashMap::new();
    targets.insert("nonexistent".to_string(), TransitionRule::default());
    def.transitions.insert("a".to_string(), targets);
    let errors = schema::validate(&def, None);
    assert!(errors.iter().any(|e| e.contains("transition target 'nonexistent'")));
}

#[test]
fn reject_terminal_with_outgoing() {
    let mut def = minimal_def("test");
    let mut targets = HashMap::new();
    targets.insert("a".to_string(), TransitionRule::default());
    def.transitions.insert("done".to_string(), targets);
    let errors = schema::validate(&def, None);
    assert!(errors.iter().any(|e| e.contains("terminal state 'done' has outgoing")));
}

#[test]
fn reject_unknown_role() {
    let mut def = minimal_def("test");
    let mut targets = HashMap::new();
    targets.insert(
        "b".to_string(),
        TransitionRule {
            name: String::new(),
            by: vec!["cordinator".to_string()],
        },
    );
    def.transitions.insert("a".to_string(), targets);
    let errors = schema::validate(&def, None);
    assert!(errors.iter().any(|e| e.contains("unknown role 'cordinator'")));
}

#[test]
fn reject_no_terminal_states() {
    let mut def = minimal_def("test");
    def.terminal.clear();
    let errors = schema::validate(&def, None);
    assert!(errors.iter().any(|e| e.contains("no terminal states defined")));
}

#[test]
fn reject_unreachable_state() {
    let mut def = minimal_def("test");
    def.states.push("orphan".to_string());
    // orphan has no incoming or outgoing transitions
    let errors = schema::validate(&def, None);
    assert!(errors.iter().any(|e| e.contains("'orphan' cannot reach any terminal")));
}

#[test]
fn reject_name_mismatch() {
    let def = minimal_def("foo");
    let errors = schema::validate(&def, Some("bar.yml"));
    assert!(errors.iter().any(|e| e.contains("does not match filename")));
}

#[test]
fn validate_collects_all_errors() {
    let mut def = minimal_def("test");
    def.terminal.push("ghost".to_string());
    def.states.push("orphan".to_string());
    let errors = schema::validate(&def, Some("wrong.yml"));
    // Should find at least 3 errors: name mismatch, ghost terminal, orphan state
    assert!(errors.len() >= 3, "expected multiple errors, got: {:?}", errors);
}

// --- Interpreter tests ---

#[test]
fn interpreter_loads_all_fsms() {
    let interp = load_test_interpreter();
    assert_eq!(interp.len(), 5);
}

#[test]
fn work_valid_transition() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "draft", "ready", "coordinator");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn work_self_transition_unchanged() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "draft", "draft", "coordinator");
    assert_eq!(result.unwrap(), Transition::Unchanged);
}

#[test]
fn work_invalid_role() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "in-review", "integrated", "implementer");
    assert!(result.is_err());
}

#[test]
fn work_invalid_target() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "draft", "done", "coordinator");
    assert!(result.is_err());
}

#[test]
fn work_override_valid() {
    let interp = load_test_interpreter();
    // in-progress -> ready is NOT a normal transition, but IS an override
    let result = interp.validate_transition("work", "in-progress", "ready", "coordinator");
    assert!(result.is_err(), "should fail as normal transition");
    let result = interp.validate_override("work", "in-progress", "ready", "coordinator");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn work_override_wrong_role() {
    let interp = load_test_interpreter();
    let result = interp.validate_override("work", "in-progress", "ready", "implementer");
    assert!(result.is_err());
}

#[test]
fn work_is_terminal() {
    let interp = load_test_interpreter();
    assert!(interp.is_terminal("work", "done").unwrap());
    assert!(interp.is_terminal("work", "abandoned").unwrap());
    assert!(!interp.is_terminal("work", "in-progress").unwrap());
}

#[test]
fn work_valid_targets() {
    let interp = load_test_interpreter();
    let targets = interp.valid_targets("work", "in-progress", "implementer").unwrap();
    assert!(targets.contains(&"blocked".to_string())); // any role
    assert!(targets.contains(&"in-review".to_string())); // implementer
    assert!(!targets.contains(&"abandoned".to_string())); // coordinator only
}

#[test]
fn bundle_valid_transition() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("bundle", "proposed", "triaged", "coordinator");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn hierarchy_valid_transition() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("hierarchy", "draft", "active", "coordinator");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn tick_valid_transition() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("tick", "open", "sealing", "integrator");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn agent_any_role_transitions() {
    let interp = load_test_interpreter();
    // Agent FSM has no role restrictions - any role should work
    let result = interp.validate_transition("agent", "starting", "running", "coordinator");
    assert_eq!(result.unwrap(), Transition::Changed);
    let result = interp.validate_transition("agent", "starting", "running", "implementer");
    assert_eq!(result.unwrap(), Transition::Changed);
}

#[test]
fn unknown_fsm_errors() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("nonexistent", "a", "b", "coordinator");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("unknown FSM"));
}

#[test]
fn work_unrestricted_transition() {
    let interp = load_test_interpreter();
    // in-progress -> blocked has empty by (any role)
    let result = interp.validate_transition("work", "in-progress", "blocked", "implementer");
    assert_eq!(result.unwrap(), Transition::Changed);
    let result = interp.validate_transition("work", "in-progress", "blocked", "reviewer");
    assert_eq!(result.unwrap(), Transition::Changed);
}

// --- Terminal override validation ---

#[test]
fn reject_terminal_with_outgoing_overrides() {
    let mut def = minimal_def("test");
    let mut targets = HashMap::new();
    targets.insert("a".to_string(), TransitionRule::default());
    def.overrides.insert("done".to_string(), targets);
    let errors = schema::validate(&def, None);
    assert!(
        errors
            .iter()
            .any(|e| e.contains("terminal state 'done' has outgoing overrides"))
    );
}

// --- Rich error hints ---

#[test]
fn error_includes_valid_target_hints() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "in-progress", "done", "implementer");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("hint: valid targets from in-progress:"), "error: {}", err);
}

#[test]
fn error_includes_override_hints() {
    let interp = load_test_interpreter();
    let result = interp.validate_transition("work", "in-progress", "done", "coordinator");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("hint: with overrides:"), "error: {}", err);
}

#[test]
fn override_error_chains_normal_error() {
    let interp = load_test_interpreter();
    // in-progress -> done with role=implementer fails both normal and override
    let result = interp.validate_override("work", "in-progress", "done", "implementer");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("normal transition also failed"), "error: {}", err);
}

// --- valid_targets includes overrides ---

#[test]
fn valid_targets_includes_overrides() {
    let interp = load_test_interpreter();
    let targets = interp.valid_targets("work", "in-progress", "coordinator").unwrap();
    // Normal: blocked (any), abandoned (coordinator)
    // Override: ready (coordinator), in-review (coordinator)
    assert!(
        targets.iter().any(|t| t.contains("ready") && t.contains("override")),
        "should include override targets: {:?}",
        targets
    );
}

// --- keyby name field ---

#[test]
fn transition_rule_name_populated_from_yaml() {
    let defs = schema::load_dir(&test_strategies_dir()).unwrap();
    let work = defs.iter().find(|d| d.name == "work").unwrap();
    let draft_targets = &work.transitions["draft"];
    let pending_rule = &draft_targets["pending"];
    assert_eq!(pending_rule.name, "pending", "name should be injected from map key");
}
