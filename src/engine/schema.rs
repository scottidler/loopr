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
    /// Names must be unique within an action sequence. They are intentional scaffolding -
    /// a named step that no current `$context` reference consumes is still valid; it is
    /// pre-wired for future strategy evolution without requiring a schema change.
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
    ///
    /// # Reference syntax
    ///
    /// - `$trigger.scope-id` - ID of the record the trigger fired on.
    /// - `$trigger.event.{field}` - field from the event payload (event triggers only).
    /// - `$context.{step-name}.{field}` - output field from a preceding named step.
    ///
    /// # String interpolation is NOT supported
    ///
    /// A `$context.*` reference must occupy the entire parameter value. Embedded
    /// references like `"prefix $context.step.field"` are NOT detected by validation
    /// and will be passed as literal strings to the primitive at runtime. This is an
    /// intentional simplicity boundary: params are either literals or references, not
    /// templates. If template interpolation is needed in the future, it should be
    /// added as an explicit primitive (e.g., `format-string`) rather than expanding
    /// the parameter syntax.
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

/// Load all strategy definitions from embedded resources, with optional repo-local override.
///
/// Uses `Resources::load_dir_excluding()` to discover strategy YAML files from the
/// embedded strategies directory, skipping fsm/, triggers/, and roles/ subdirectories
/// that share the same embed root but contain non-strategy YAML schemas.
pub fn load_from_resources(repo_path: Option<&Path>) -> eyre::Result<Vec<StrategyDefinition>> {
    let entries = crate::resources::Resources::load_dir_excluding(&["fsm/", "triggers/", "roles/"], repo_path)?;
    let mut defs = Vec::new();
    for (path, content) in entries {
        let mut file_defs = parse_content(&content, &path)?;
        defs.append(&mut file_defs);
    }
    info!("loaded {} strategy definitions from resources", defs.len());
    Ok(defs)
}

/// Load all strategy definitions from a directory tree (recursive).
/// Strategies are organized in subdirectories (e.g. `recovery/`, `reconciliation/`).
/// The `fsm/`, `triggers/`, and `roles/` subdirs at the ROOT level are skipped - they
/// share the `strategies/` root but contain non-strategy YAML files with incompatible
/// schemas. This skip is anchored to the root level only.
pub fn load_dir(dir: &Path) -> eyre::Result<Vec<StrategyDefinition>> {
    let mut defs = Vec::new();
    load_dir_recursive(dir, dir, &mut defs)?;
    info!("loaded {} strategy definitions from {}", defs.len(), dir.display());
    Ok(defs)
}

fn load_dir_recursive(root: &Path, dir: &Path, defs: &mut Vec<StrategyDefinition>) -> eyre::Result<()> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| eyre::eyre!("failed to read strategy dir {}: {}", dir.display(), e))?;
    let mut entries: Vec<_> = entries.collect::<Result<_, _>>()?;
    // Sort for deterministic load order (alphabetical by filename)
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            // Skip fsm/ and triggers/ at the root level only - these share the strategies/
            // root but contain non-strategy YAML files with incompatible schemas.
            if dir == root {
                let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if matches!(dir_name, "fsm" | "triggers" | "roles") {
                    continue;
                }
            }
            load_dir_recursive(root, &path, defs)?;
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
    parse_content(&content, &path.display().to_string())
}

/// Parse strategy definitions from YAML content string.
/// The content must be a keyed map: strategy-name -> StrategyDefinition.
pub fn parse_content(content: &str, source: &str) -> eyre::Result<Vec<StrategyDefinition>> {
    let raw: HashMap<String, StrategyDefinition> =
        serde_yaml::from_str(content).map_err(|e| eyre::eyre!("failed to parse {}: {}", source, e))?;
    let mut defs: Vec<StrategyDefinition> = raw
        .into_iter()
        .map(|(name, mut def)| {
            def.name = name;
            def
        })
        .collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    info!("loaded {} strategies from {}", defs.len(), source);
    Ok(defs)
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// Structural validation of a set of strategy definitions.
///
/// Checks correctness without requiring external registries. Registry-dependent
/// checks (trigger existence, primitive existence, param types) are deferred to
/// Phase 2 when TriggerEvaluator and PrimitiveRegistry are available.
///
/// Returns a list of error strings. Empty vec = valid.
pub fn validate(defs: &[StrategyDefinition]) -> Vec<String> {
    let mut errors = Vec::new();

    // Check for duplicate strategy names across all loaded definitions
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
    // trigger must be non-empty (existence in trigger registry is Phase 2)
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

    // Step names must be unique within each sequence
    validate_step_name_uniqueness(def, "action", &def.action, errors);
    validate_step_name_uniqueness(def, "on-success", &def.on_success, errors);
    validate_step_name_uniqueness(def, "on-failure", &def.on_failure, errors);

    // Validate action steps: each step may reference named steps that precede it.
    for (i, step) in def.action.iter().enumerate() {
        validate_step(def, "action", step, &def.action[..i], &[], errors);
    }

    // on-success steps may reference:
    //   - any named step in the action sequence, AND
    //   - named steps earlier in the same on-success sequence.
    for (i, step) in def.on_success.iter().enumerate() {
        validate_step(def, "on-success", step, &def.action, &def.on_success[..i], errors);
    }

    // on-failure steps follow the same rule as on-success.
    for (i, step) in def.on_failure.iter().enumerate() {
        validate_step(def, "on-failure", step, &def.action, &def.on_failure[..i], errors);
    }
}

fn validate_step_name_uniqueness(
    def: &StrategyDefinition,
    sequence_label: &str,
    steps: &[ActionStep],
    errors: &mut Vec<String>,
) {
    let mut seen: HashSet<&str> = HashSet::new();
    for step in steps {
        if let Some(name) = step.name.as_deref()
            && !seen.insert(name)
        {
            errors.push(format!(
                "strategy '{}' {}: duplicate step name '{}'",
                def.name, sequence_label, name
            ));
        }
    }
}

/// Validate a single action step.
///
/// `preceding_primary` - steps whose names are always visible (the action sequence).
/// `preceding_secondary` - additional steps whose names are visible (earlier steps in
///   the same on-success or on-failure sequence being validated).
fn validate_step(
    def: &StrategyDefinition,
    context: &str,
    step: &ActionStep,
    preceding_primary: &[ActionStep],
    preceding_secondary: &[ActionStep],
    errors: &mut Vec<String>,
) {
    // primitive name must be non-empty (existence in registry is Phase 2)
    if step.primitive.is_empty() {
        errors.push(format!(
            "strategy '{}' {}: step has empty primitive name",
            def.name, context
        ));
    }

    // Build the set of named steps visible from this position
    let named_preceding: HashSet<&str> = preceding_primary
        .iter()
        .chain(preceding_secondary.iter())
        .filter_map(|s| s.name.as_deref())
        .collect();

    for (param_key, param_val) in &step.params {
        // Reject malformed $context.{step} references missing the .{field} suffix.
        // A well-formed reference is $context.{step-name}.{field}; anything that starts
        // with "$context." but has no second dot is a typo that would silently produce
        // a literal string at runtime instead of the intended value.
        for bad_ref in find_malformed_context_refs(param_val) {
            errors.push(format!(
                "strategy '{}' {} param '{}': malformed context reference '{}' - expected $context.{{step}}.{{field}}",
                def.name, context, param_key, bad_ref
            ));
        }

        // Validate that well-formed $context.{step}.{field} references point to known
        // named steps that precede the current step (not forward references).
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

// ─── Reference utilities ─────────────────────────────────────────────────────

/// Extract all step names from well-formed `$context.{step}.{field}` patterns in a value.
/// A well-formed reference has exactly two dots after `$context`: step name and field name.
pub fn extract_context_refs(val: &serde_json::Value) -> Vec<String> {
    let mut refs = Vec::new();
    collect_context_refs(val, &mut refs);
    refs
}

fn collect_context_refs(val: &serde_json::Value, refs: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => {
            // Malformed refs (no .field suffix) are handled by find_malformed_context_refs
            if let Some(rest) = s.strip_prefix("$context.")
                && let Some(dot_pos) = rest.find('.')
            {
                refs.push(rest[..dot_pos].to_owned());
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

/// Find all strings that start with `$context.` but are missing the `.{field}` suffix.
/// These are malformed references that would silently produce literal strings at runtime.
pub fn find_malformed_context_refs(val: &serde_json::Value) -> Vec<String> {
    let mut malformed = Vec::new();
    collect_malformed_context_refs(val, &mut malformed);
    malformed
}

fn collect_malformed_context_refs(val: &serde_json::Value, malformed: &mut Vec<String>) {
    match val {
        serde_json::Value::String(s) => {
            if let Some(rest) = s.strip_prefix("$context.")
                && !rest.contains('.')
            {
                malformed.push(s.clone());
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_malformed_context_refs(item, malformed);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_malformed_context_refs(v, malformed);
            }
        }
        _ => {}
    }
}
