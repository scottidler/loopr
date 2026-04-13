use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use serde::Deserialize;
use tracing::info;

/// Known valid roles - validated at startup.
const VALID_ROLES: &[&str] = &["coordinator", "integrator", "implementer", "reviewer", "researcher"];

/// Authorization rule for a transition to a target state.
/// The `name` field holds the target state name, populated from the YAML
/// map key after deserialization (keyby convention).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TransitionRule {
    /// Target state name - populated from the YAML map key post-deserialization.
    #[serde(skip_deserializing)]
    pub name: String,
    /// Roles authorized to perform this transition.
    /// Empty vec means any role is allowed.
    #[serde(default)]
    pub by: Vec<String>,
}

/// What the engine does when a guard condition fails.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    /// Reject the transition with an error message. Default.
    #[default]
    Reject,
    /// Allow the transition but emit a warning.
    Warn,
}

/// A guard condition on a transition.
/// Guards are evaluated synchronously during transition validation; if the
/// condition returns false the transition is rejected (or warned, per on-failure).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct GuardDef {
    pub from: String,
    pub to: String,
    pub condition: String,
    #[serde(default)]
    pub on_failure: OnFailure,
    #[serde(default)]
    pub message: String,
}

/// Complete FSM definition loaded from YAML.
#[derive(Debug, Clone, Deserialize)]
pub struct FsmDefinition {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub states: Vec<String>,
    pub terminal: Vec<String>,
    /// Source state -> (target state -> rule).
    pub transitions: HashMap<String, HashMap<String, TransitionRule>>,
    /// Source state -> (target state -> rule) for override-only edges.
    #[serde(default)]
    pub overrides: HashMap<String, HashMap<String, TransitionRule>>,
    #[serde(default)]
    pub guards: HashMap<String, GuardDef>,
}

/// Load a single FSM definition from a YAML file.
pub fn load_file(path: &Path) -> eyre::Result<FsmDefinition> {
    let content = std::fs::read_to_string(path).map_err(|e| eyre::eyre!("failed to read {}: {}", path.display(), e))?;
    let mut def: FsmDefinition =
        serde_yaml::from_str(&content).map_err(|e| eyre::eyre!("failed to parse {}: {}", path.display(), e))?;
    inject_transition_names(&mut def);
    info!("loaded FSM '{}' from {}", def.name, path.display());
    Ok(def)
}

/// Load all FSM definitions from a directory.
pub fn load_dir(dir: &Path) -> eyre::Result<Vec<FsmDefinition>> {
    let mut defs = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| eyre::eyre!("failed to read dir {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "yml" || e == "yaml").unwrap_or(false) {
            defs.push(load_file(&path)?);
        }
    }
    Ok(defs)
}

/// Inject the YAML map key into each TransitionRule's `name` field.
/// This follows the keyby convention: the map key IS the name, and the
/// struct carries it for contexts where the HashMap is not available.
/// keyby's derive macro can't drive nested HashMap deserialization, so
/// we populate the field in a post-deserialization pass.
pub fn inject_transition_names(def: &mut FsmDefinition) {
    for targets in def.transitions.values_mut() {
        for (target_name, rule) in targets.iter_mut() {
            rule.name = target_name.clone();
        }
    }
    for targets in def.overrides.values_mut() {
        for (target_name, rule) in targets.iter_mut() {
            rule.name = target_name.clone();
        }
    }
}

/// Validate an FSM definition. Returns a list of errors (empty = valid).
/// Errors are collected, not fail-on-first.
pub fn validate(def: &FsmDefinition, filename: Option<&str>) -> Vec<String> {
    let mut errors = Vec::new();
    let states: HashSet<&str> = def.states.iter().map(|s| s.as_str()).collect();

    // FSM name matches filename
    if let Some(fname) = filename {
        let expected = fname
            .strip_suffix(".yml")
            .or_else(|| fname.strip_suffix(".yaml"))
            .unwrap_or(fname);
        if def.name != expected {
            errors.push(format!("FSM name '{}' does not match filename '{}'", def.name, fname));
        }
    }

    // At least one terminal state
    if def.terminal.is_empty() {
        errors.push("no terminal states defined".to_string());
    }

    // All terminal states are listed in states
    for t in &def.terminal {
        if !states.contains(t.as_str()) {
            errors.push(format!("terminal state '{}' not listed in states", t));
        }
    }

    // All transition sources are listed in states
    for from in def.transitions.keys() {
        if !states.contains(from.as_str()) {
            errors.push(format!("transition source '{}' not listed in states", from));
        }
    }

    // All transition targets are listed in states
    for (from, targets) in &def.transitions {
        for to in targets.keys() {
            if !states.contains(to.as_str()) {
                errors.push(format!(
                    "transition target '{}' (from '{}') not listed in states",
                    to, from
                ));
            }
        }
    }

    // Terminal states have no outgoing transitions
    for t in &def.terminal {
        if def.transitions.contains_key(t) {
            errors.push(format!("terminal state '{}' has outgoing transitions", t));
        }
    }

    // Terminal states have no outgoing overrides
    for t in &def.terminal {
        if def.overrides.contains_key(t) {
            errors.push(format!("terminal state '{}' has outgoing overrides", t));
        }
    }

    // All override sources are listed in states
    for from in def.overrides.keys() {
        if !states.contains(from.as_str()) {
            errors.push(format!("override source '{}' not listed in states", from));
        }
    }

    // All override targets are listed in states
    for (from, targets) in &def.overrides {
        for to in targets.keys() {
            if !states.contains(to.as_str()) {
                errors.push(format!(
                    "override target '{}' (from '{}') not listed in states",
                    to, from
                ));
            }
        }
    }

    // All roles are valid
    for (from, targets) in &def.transitions {
        for (to, rule) in targets {
            for role in &rule.by {
                if !VALID_ROLES.contains(&role.as_str()) {
                    errors.push(format!("unknown role '{}' in transition {} -> {}", role, from, to));
                }
            }
        }
    }
    for (from, targets) in &def.overrides {
        for (to, rule) in targets {
            for role in &rule.by {
                if !VALID_ROLES.contains(&role.as_str()) {
                    errors.push(format!("unknown role '{}' in override {} -> {}", role, from, to));
                }
            }
        }
    }

    // Guard from/to states must exist, and the transition/override edge must exist
    for (guard_name, guard) in &def.guards {
        if !states.contains(guard.from.as_str()) {
            errors.push(format!("guard '{}': unknown from-state '{}'", guard_name, guard.from));
        }
        if !states.contains(guard.to.as_str()) {
            errors.push(format!("guard '{}': unknown to-state '{}'", guard_name, guard.to));
        }
        // Only check edge existence when both states are valid (avoid redundant errors)
        if states.contains(guard.from.as_str()) && states.contains(guard.to.as_str()) {
            let edge_exists = def
                .transitions
                .get(&guard.from)
                .map(|targets| targets.contains_key(&guard.to))
                .unwrap_or(false)
                || def
                    .overrides
                    .get(&guard.from)
                    .map(|targets| targets.contains_key(&guard.to))
                    .unwrap_or(false);
            if !edge_exists {
                errors.push(format!(
                    "guard '{}': no transition exists from '{}' to '{}' in FSM '{}'",
                    guard_name, guard.from, guard.to, def.name
                ));
            }
        }
    }

    // All non-terminal states can reach a terminal (BFS reachability)
    let terminal_set: HashSet<&str> = def.terminal.iter().map(|s| s.as_str()).collect();
    for state in &def.states {
        if terminal_set.contains(state.as_str()) {
            continue;
        }
        if !can_reach_terminal(state, &def.transitions, &terminal_set) {
            errors.push(format!(
                "non-terminal state '{}' cannot reach any terminal state",
                state
            ));
        }
    }

    errors
}

/// BFS from a state through transitions to check if any terminal is reachable.
fn can_reach_terminal(
    start: &str,
    transitions: &HashMap<String, HashMap<String, TransitionRule>>,
    terminals: &HashSet<&str>,
) -> bool {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(start);
    visited.insert(start);

    while let Some(current) = queue.pop_front() {
        if terminals.contains(current) {
            return true;
        }
        if let Some(targets) = transitions.get(current) {
            for target in targets.keys() {
                if !visited.contains(target.as_str()) {
                    visited.insert(target.as_str());
                    queue.push_back(target.as_str());
                }
            }
        }
    }
    false
}
