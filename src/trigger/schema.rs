use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use tracing::info;

/// Known valid collection names for scope and ratio queries.
const VALID_SCOPES: &[&str] = &["work", "bundle", "plan", "spec", "phase", "session", "tick", "lock"];

/// Known valid event types for event triggers.
const VALID_EVENTS: &[&str] = &[
    "transition.completed",
    "record.created",
    "record.updated",
    "agent.status-changed",
    "agent.created",
    "tick.published",
    "decomposition.completed",
    "decomposition.failed",
    "escalation",
    "reconciliation-failed",
    "integration.conflict-detected",
];

/// Comparison operators for threshold and ratio triggers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub enum Operator {
    #[serde(rename = ">=")]
    Gte,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<=")]
    Lte,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
}

/// Boolean operators for composite triggers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompositeOperator {
    And,
    Or,
    Not,
}

/// A count query used in ratio trigger numerator/denominator.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CountQuery {
    /// Collection to count records in.
    pub collection: String,
    /// Optional filter: field-value pairs that must all match.
    #[serde(default)]
    pub filter: HashMap<String, serde_json::Value>,
}

/// A trigger definition loaded from YAML.
/// The name is injected from the YAML map key (keyby convention).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TriggerDefinition {
    /// Trigger name - injected from the YAML map key, not a YAML field.
    #[serde(skip_deserializing)]
    pub name: String,
    /// Whether this trigger is active. Disabled triggers are parsed but never evaluated.
    /// Defaults to true. Field ordering is load-bearing: must precede the #[serde(flatten)]
    /// field to ensure the default applies correctly (serde-rs/serde#1626).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Suppress re-fires for a scope_id within this window after firing.
    #[serde(default)]
    pub cooldown_secs: Option<u32>,
    /// The kind of trigger and its type-specific parameters.
    #[serde(flatten)]
    pub kind: TriggerKind,
}

fn default_enabled() -> bool {
    true
}

/// The kind of trigger and its type-specific parameters.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TriggerKind {
    /// Fires when a numeric field on a scoped record meets an operator+value condition.
    Threshold {
        scope: String,
        field: String,
        operator: Operator,
        value: f64,
    },
    /// Fires when count(numerator)/count(denominator) meets an operator+value condition.
    Ratio {
        scope: String,
        numerator: CountQuery,
        denominator: CountQuery,
        operator: Operator,
        value: f64,
    },
    /// Fires when a matching event arrives on the event bus.
    Event {
        event: String,
        #[serde(default)]
        scope: Option<String>,
        #[serde(rename = "match", default)]
        match_filter: HashMap<String, serde_json::Value>,
        #[serde(rename = "throttle-secs", default)]
        throttle_secs: Option<u32>,
    },
    /// Fires when elapsed time since a timestamp field exceeds a duration.
    Timer {
        scope: String,
        #[serde(rename = "start-field")]
        start_field: String,
        #[serde(rename = "max-duration-secs")]
        max_duration_secs: u64,
    },
    /// Fires when a named built-in state query returns true.
    StateQuery {
        scope: String,
        query: String,
        #[serde(default)]
        params: HashMap<String, serde_json::Value>,
    },
    /// Combines other triggers with boolean logic (and/or/not).
    Composite {
        operator: CompositeOperator,
        triggers: Vec<String>,
    },
}

impl TriggerKind {
    /// Return the scope for non-composite triggers (None for composite).
    pub fn scope(&self) -> Option<&str> {
        match self {
            TriggerKind::Threshold { scope, .. } => Some(scope),
            TriggerKind::Ratio { scope, .. } => Some(scope),
            TriggerKind::Event { scope, .. } => scope.as_deref(),
            TriggerKind::Timer { scope, .. } => Some(scope),
            TriggerKind::StateQuery { scope, .. } => Some(scope),
            TriggerKind::Composite { .. } => None,
        }
    }
}

/// Load all trigger definitions from embedded resources, with optional repo-local override.
///
/// Uses `Resources::load_dir("engine/triggers/", ...)` to discover trigger YAML files from
/// the embedded resources directory, falling back through the standard override chain
/// (repo-local > XDG > embedded).
pub fn load_from_resources(repo_path: Option<&Path>) -> eyre::Result<Vec<TriggerDefinition>> {
    let entries = crate::resources::Resources::load_dir("engine/triggers/", repo_path)?;
    let mut defs = Vec::new();
    for (path, content) in entries {
        let mut file_defs = parse_content(&content, &path)?;
        defs.append(&mut file_defs);
    }
    info!("loaded {} trigger definitions from resources", defs.len());
    Ok(defs)
}

/// Load all trigger definitions from a directory of YAML files.
pub fn load_dir(dir: &Path) -> eyre::Result<Vec<TriggerDefinition>> {
    let mut defs = Vec::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| eyre::eyre!("failed to read trigger dir {}: {}", dir.display(), e))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "yml" || e == "yaml").unwrap_or(false) {
            let mut file_defs = load_file(&path)?;
            defs.append(&mut file_defs);
        }
    }
    info!("loaded {} trigger definitions from {}", defs.len(), dir.display());
    Ok(defs)
}

/// Load trigger definitions from a single YAML file.
/// The file must be a keyed map: trigger-name -> TriggerDefinition.
pub fn load_file(path: &Path) -> eyre::Result<Vec<TriggerDefinition>> {
    let content = std::fs::read_to_string(path).map_err(|e| eyre::eyre!("failed to read {}: {}", path.display(), e))?;
    parse_content(&content, &path.display().to_string())
}

/// Parse trigger definitions from YAML content string.
/// The content must be a keyed map: trigger-name -> TriggerDefinition.
pub fn parse_content(content: &str, source: &str) -> eyre::Result<Vec<TriggerDefinition>> {
    let raw: HashMap<String, TriggerDefinition> =
        serde_yaml::from_str(content).map_err(|e| eyre::eyre!("failed to parse {}: {}", source, e))?;
    let mut defs: Vec<TriggerDefinition> = raw
        .into_iter()
        .map(|(name, mut def)| {
            def.name = name;
            def
        })
        .collect();
    // Sort by name for deterministic ordering in tests.
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    info!("loaded {} triggers from {}", defs.len(), source);
    Ok(defs)
}

/// Validate a set of trigger definitions. Returns a list of errors (empty = valid).
/// Validates all triggers collectively so composite references can be checked.
pub fn validate(defs: &[TriggerDefinition]) -> Vec<String> {
    let mut errors = Vec::new();
    let names: HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    let enabled_map: HashMap<&str, bool> = defs.iter().map(|d| (d.name.as_str(), d.enabled)).collect();

    for def in defs {
        validate_trigger(def, &names, &enabled_map, &mut errors);
    }

    // Check for cycles in composite triggers.
    check_cycles(defs, &mut errors);

    errors
}

fn validate_trigger(
    def: &TriggerDefinition,
    all_names: &HashSet<&str>,
    enabled_map: &HashMap<&str, bool>,
    errors: &mut Vec<String>,
) {
    match &def.kind {
        TriggerKind::Threshold { scope, field, .. } => {
            if !VALID_SCOPES.contains(&scope.as_str()) {
                errors.push(format!("trigger '{}': unknown scope '{}'", def.name, scope));
            }
            if field.is_empty() {
                errors.push(format!("trigger '{}': field must not be empty", def.name));
            }
        }
        TriggerKind::Ratio {
            scope,
            numerator,
            denominator,
            ..
        } => {
            if !VALID_SCOPES.contains(&scope.as_str()) {
                errors.push(format!("trigger '{}': unknown scope '{}'", def.name, scope));
            }
            if !VALID_SCOPES.contains(&numerator.collection.as_str()) {
                errors.push(format!(
                    "trigger '{}': numerator collection '{}' is not a valid scope",
                    def.name, numerator.collection
                ));
            }
            if !VALID_SCOPES.contains(&denominator.collection.as_str()) {
                errors.push(format!(
                    "trigger '{}': denominator collection '{}' is not a valid scope",
                    def.name, denominator.collection
                ));
            }
        }
        TriggerKind::Event { event, .. } => {
            if !VALID_EVENTS.contains(&event.as_str()) {
                errors.push(format!("trigger '{}': unknown event type '{}'", def.name, event));
            }
        }
        TriggerKind::Timer { scope, start_field, .. } => {
            if !VALID_SCOPES.contains(&scope.as_str()) {
                errors.push(format!("trigger '{}': unknown scope '{}'", def.name, scope));
            }
            if start_field.is_empty() {
                errors.push(format!("trigger '{}': start-field must not be empty", def.name));
            }
        }
        TriggerKind::StateQuery { scope, query, .. } => {
            if !VALID_SCOPES.contains(&scope.as_str()) {
                errors.push(format!("trigger '{}': unknown scope '{}'", def.name, scope));
            }
            if query.is_empty() {
                errors.push(format!("trigger '{}': query must not be empty", def.name));
            }
        }
        TriggerKind::Composite { operator, triggers } => {
            // Not validates exactly one trigger.
            if *operator == CompositeOperator::Not && triggers.len() != 1 {
                errors.push(format!(
                    "trigger '{}': composite 'not' must have exactly one sub-trigger, got {}",
                    def.name,
                    triggers.len()
                ));
            }
            // And/or needs at least 2.
            if *operator != CompositeOperator::Not && triggers.len() < 2 {
                errors.push(format!(
                    "trigger '{}': composite '{:?}' must have at least 2 sub-triggers, got {}",
                    def.name,
                    operator,
                    triggers.len()
                ));
            }
            // All referenced triggers must exist.
            for referenced in triggers {
                if !all_names.contains(referenced.as_str()) {
                    errors.push(format!(
                        "trigger '{}': references unknown trigger '{}'",
                        def.name, referenced
                    ));
                }
            }
            // NOT composite: sub-trigger must not be disabled.
            // NOT(disabled) returns Idle which inverts to fire unconditionally - dangerous.
            // AND/OR(disabled) silently degrades but is warn-only; caught by validate_cross_references.
            if *operator == CompositeOperator::Not {
                for sub in triggers {
                    if enabled_map.get(sub.as_str()) == Some(&false) {
                        errors.push(format!(
                            "trigger '{}': NOT sub-trigger '{}' is disabled; NOT(disabled) fires unconditionally",
                            def.name, sub
                        ));
                    }
                }
            }
        }
    }
}

/// DFS cycle detection for composite triggers.
fn check_cycles(defs: &[TriggerDefinition], errors: &mut Vec<String>) {
    let trigger_map: HashMap<&str, &TriggerDefinition> = defs.iter().map(|d| (d.name.as_str(), d)).collect();

    for def in defs {
        if let TriggerKind::Composite { .. } = &def.kind {
            let mut visited = HashSet::new();
            let mut path = Vec::new();
            if has_cycle(def.name.as_str(), &trigger_map, &mut visited, &mut path) {
                errors.push(format!(
                    "trigger '{}': cycle detected in composite: {}",
                    def.name,
                    path.join(" -> ")
                ));
            }
        }
    }
}

fn has_cycle<'a>(
    name: &'a str,
    map: &HashMap<&str, &'a TriggerDefinition>,
    visited: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> bool {
    if path.contains(&name) {
        path.push(name);
        return true;
    }
    if visited.contains(name) {
        return false;
    }
    visited.insert(name);
    path.push(name);
    if let Some(def) = map.get(name)
        && let TriggerKind::Composite { triggers, .. } = &def.kind
    {
        for sub in triggers {
            if has_cycle(sub.as_str(), map, visited, path) {
                return true;
            }
        }
    }
    path.pop();
    false
}
