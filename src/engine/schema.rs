use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use tracing::info;

/// Known valid collection names for strategy scope.
const VALID_SCOPES: &[&str] = &["work", "bundle", "plan", "spec", "phase", "session", "tick", "lock"];

// ─── ActionStep ──────────────────────────────────────────────────────────────

/// A single step in a strategy's action, on-success, or on-failure sequence.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ActionStep {
    /// Optional name for referencing this step's output via `$context.{name}.{field}`.
    #[serde(default)]
    pub name: Option<String>,
    /// Primitive to invoke (must be registered in PrimitiveRegistry - validated in Phase 2).
    pub primitive: String,
    /// Optional guard condition (Doc 4) that must pass before this step executes.
    /// If the guard fails the step is skipped, not the whole strategy.
    #[serde(default)]
    pub guard: Option<String>,
    /// Parameters passed to the primitive. Values may contain `$trigger.*` or `$context.*`
    /// references resolved at execution time.
    #[serde(default)]
    pub params: HashMap<String, serde_json::Value>,
}

// ─── StrategyDefinition ──────────────────────────────────────────────────────

/// A strategy definition loaded from YAML.
/// The name is injected from the YAML map key (keyby convention).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StrategyDefinition {
    /// Strategy name - injected from the YAML map key, not a YAML field.
    #[serde(skip_deserializing)]
    pub name: String,
    /// Human-readable description of what this strategy does.
    #[serde(default)]
    pub description: String,
    /// Name of a trigger defined in `strategies/triggers/`. Validated against
    /// the trigger registry in Phase 2.
    pub trigger: String,
    /// Domain collection this strategy operates on (e.g. "work", "plan").
    pub scope: String,
    /// Execution priority. Higher fires first. Default 100.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Ordered action sequence executed when the trigger fires. Must be non-empty.
    pub action: Vec<ActionStep>,
    /// Primitives to execute when all action steps succeed.
    #[serde(default)]
    pub on_success: Vec<ActionStep>,
    /// Primitives to execute when any action step fails.
    #[serde(default)]
    pub on_failure: Vec<ActionStep>,
    /// Whether this strategy is active. Disabled strategies are parsed but never fired.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Suppress re-execution for a scope_id within this window (seconds) after firing.
    #[serde(default)]
    pub cooldown_secs: Option<u32>,
}

fn default_priority() -> u32 {
    100
}

fn default_enabled() -> bool {
    true
}

// ─── Loading ─────────────────────────────────────────────────────────────────

/// Load all strategy definitions from a directory tree (recursive).
/// Strategies may be organized in subdirectories (e.g. `recovery/`, `reconciliation/`).
pub fn load_dir(dir: &Path) -> eyre::Result<Vec<StrategyDefinition>> {
    let mut defs = Vec::new();
    load_dir_recursive(dir, &mut defs)?;
    info!("loaded {} strategy definitions from {}", defs.len(), dir.display());
    Ok(defs)
}

fn load_dir_recursive(dir: &Path, defs: &mut Vec<StrategyDefinition>) -> eyre::Result<()> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| eyre::eyre!("failed to read strategy dir {}: {}", dir.display(), e))?;
    let mut entries: Vec<_> = entries.collect::<Result<_, _>>()?;
    // Sort for deterministic load order (alphabetical by filename)
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            // Skip non-strategy subdirs that share the strategies/ root
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(dir_name, "fsm" | "triggers") {
                continue;
            }
            load_dir_recursive(&path, defs)?;
        } else if path.extension().map(|e| e == "yml" || e == "yaml").unwrap_or(false) {
            let mut file_defs = load_file(&path)?;
            defs.append(&mut file_defs);
        }
    }
    Ok(())
}

/// Load strategy definitions from a single YAML file.
/// The file must be a keyed map: strategy-name -> StrategyDefinition.
pub fn load_file(path: &Path) -> eyre::Result<Vec<StrategyDefinition>> {
    let content = std::fs::read_to_string(path).map_err(|e| eyre::eyre!("failed to read {}: {}", path.display(), e))?;
    let raw: HashMap<String, StrategyDefinition> =
        serde_yaml::from_str(&content).map_err(|e| eyre::eyre!("failed to parse {}: {}", path.display(), e))?;
    let mut defs: Vec<StrategyDefinition> = raw
        .into_iter()
        .map(|(name, mut def)| {
            def.name = name;
            def
        })
        .collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    info!("loaded {} strategies from {}", defs.len(), path.display());
    Ok(defs)
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// Structural validation of a set of strategy definitions.
/// Checks correctness without requiring external registries (trigger/primitive existence
/// is deferred to Phase 2 when TriggerEvaluator and PrimitiveRegistry are available).
///
/// Returns a list of error strings. Empty vec = valid.
pub fn validate(defs: &[StrategyDefinition]) -> Vec<String> {
    let mut errors = Vec::new();

    // Check for duplicate strategy names
    let mut seen_names: HashSet<&str> = HashSet::new();
    for def in defs {
        if !seen_names.insert(def.name.as_str()) {
            errors.push(format!("strategy '{}': duplicate name", def.name));
        }
    }

    for def in defs {
        validate_strategy(def, &mut errors);
    }

    errors
}

fn validate_strategy(def: &StrategyDefinition, errors: &mut Vec<String>) {
    // trigger must be non-empty
    if def.trigger.is_empty() {
        errors.push(format!("strategy '{}': trigger must not be empty", def.name));
    }

    // scope must be a valid collection name
    if !VALID_SCOPES.contains(&def.scope.as_str()) {
        errors.push(format!("strategy '{}': unknown scope '{}'", def.name, def.scope));
    }

    // action sequence must be non-empty
    if def.action.is_empty() {
        errors.push(format!("strategy '{}': action sequence must not be empty", def.name));
    }

    // priority must be >= 1 (0 is reserved/invalid)
    if def.priority == 0 {
        errors.push(format!("strategy '{}': priority must be >= 1, got 0", def.name));
    }

    // Validate action steps and their $context references
    for (i, step) in def.action.iter().enumerate() {
        validate_step(def, "action", step, &def.action[..i], errors);
    }
    // on-success and on-failure steps can reference any named action step
    for step in &def.on_success {
        validate_step(def, "on-success", step, &def.action, errors);
    }
    for step in &def.on_failure {
        validate_step(def, "on-failure", step, &def.action, errors);
    }
}

fn validate_step(
    def: &StrategyDefinition,
    context: &str,
    step: &ActionStep,
    preceding_action_steps: &[ActionStep],
    errors: &mut Vec<String>,
) {
    // primitive name must be non-empty
    if step.primitive.is_empty() {
        errors.push(format!(
            "strategy '{}' {}: step has empty primitive name",
            def.name, context
        ));
    }

    // $context.{step}.{field} references must point to named steps earlier in the action sequence
    let named_preceding: HashSet<&str> = preceding_action_steps
        .iter()
        .filter_map(|s| s.name.as_deref())
        .collect();

    for (param_key, param_val) in &step.params {
        for step_ref in extract_context_refs(param_val) {
            if !named_preceding.contains(step_ref.as_str()) {
                errors.push(format!(
                    "strategy '{}' {} param '{}': $context.{} references unknown or forward-declared step",
                    def.name, context, param_key, step_ref
                ));
            }
        }
    }
}

/// Extract all step names referenced by `$context.{step}.{field}` patterns in a JSON value.
pub fn extract_context_refs(val: &serde_json::Value) -> Vec<String> {
    let mut refs = Vec::new();
    collect_context_refs(val, &mut refs);
    refs
}

fn collect_context_refs(val: &serde_json::Value, refs: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("$context.") {
                // Pattern: $context.{step-name}.{field} - extract step-name
                if let Some(dot_pos) = rest.find('.') {
                    refs.push(rest[..dot_pos].to_owned());
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_context_refs(item, refs);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_context_refs(v, refs);
            }
        }
        _ => {}
    }
}
