use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use super::schema::{self, FsmDefinition, TransitionRule};
use crate::domain::transition::Transition;

/// Holds all loaded FSM definitions. Immutable after startup.
pub struct FsmInterpreter {
    definitions: HashMap<String, FsmDefinition>,
}

impl FsmInterpreter {
    /// Load all FSM definitions from a directory and validate them.
    pub fn load(dir: &Path) -> eyre::Result<Self> {
        let defs = schema::load_dir(dir)?;
        let mut definitions = HashMap::new();
        let mut all_errors = Vec::new();

        for def in defs {
            let filename = format!("{}.yml", def.name);
            let errors = schema::validate(&def, Some(&filename));
            if !errors.is_empty() {
                for e in &errors {
                    all_errors.push(format!("{}: {}", def.name, e));
                }
            }
            definitions.insert(def.name.clone(), def);
        }

        if !all_errors.is_empty() {
            eyre::bail!("FSM validation failed:\n  {}", all_errors.join("\n  "));
        }

        info!("loaded {} FSM definitions", definitions.len());
        Ok(Self { definitions })
    }

    /// Build from pre-loaded definitions (for testing).
    pub fn from_definitions(defs: Vec<FsmDefinition>) -> Self {
        let definitions = defs.into_iter().map(|d| (d.name.clone(), d)).collect();
        Self { definitions }
    }

    /// Validate a normal transition.
    /// Returns Changed if valid, Unchanged if from == to (idempotent),
    /// or Err with rich hints if the transition is invalid.
    pub fn validate_transition(&self, fsm_name: &str, from: &str, to: &str, role: &str) -> eyre::Result<Transition> {
        if from == to {
            return Ok(Transition::Unchanged);
        }
        let def = self.get_definition(fsm_name)?;
        let targets = def
            .transitions
            .get(from)
            .ok_or_else(|| self.rich_error(def, from, to, role, None))?;
        self.check_target(targets, to, role, def, from, None)
    }

    /// Validate an override transition.
    /// Checks normal transitions first, then override edges.
    /// Chains both error contexts if both paths fail.
    pub fn validate_override(&self, fsm_name: &str, from: &str, to: &str, role: &str) -> eyre::Result<Transition> {
        if from == to {
            return Ok(Transition::Unchanged);
        }
        match self.validate_transition(fsm_name, from, to, role) {
            Ok(result) => Ok(result),
            Err(normal_err) => {
                let def = self.get_definition(fsm_name)?;
                let context = format!("normal transition also failed: {}", normal_err);
                let targets = def
                    .overrides
                    .get(from)
                    .ok_or_else(|| self.rich_error(def, from, to, role, Some(&context)))?;
                self.check_target(targets, to, role, def, from, Some(&context))
            }
        }
    }

    /// Check if a state is terminal.
    pub fn is_terminal(&self, fsm_name: &str, state: &str) -> eyre::Result<bool> {
        let def = self.get_definition(fsm_name)?;
        Ok(def.terminal.iter().any(|t| t == state))
    }

    /// Get all valid target states from a given state for a role.
    /// Includes both normal transitions and override targets (marked).
    pub fn valid_targets(&self, fsm_name: &str, from: &str, role: &str) -> eyre::Result<Vec<String>> {
        let def = self.get_definition(fsm_name)?;
        let mut targets = Vec::new();
        if let Some(trans) = def.transitions.get(from) {
            for (to, rule) in trans {
                if rule.by.is_empty() || rule.by.iter().any(|r| r == role) {
                    targets.push(to.clone());
                }
            }
        }
        if let Some(overrides) = def.overrides.get(from) {
            for (to, rule) in overrides {
                let authorized = rule.by.is_empty() || rule.by.iter().any(|r| r == role);
                if authorized && !targets.contains(to) {
                    targets.push(format!("{} (override)", to));
                }
            }
        }
        Ok(targets)
    }

    /// Number of loaded FSM definitions.
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether the interpreter has any definitions.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }

    fn get_definition(&self, name: &str) -> eyre::Result<&FsmDefinition> {
        self.definitions
            .get(name)
            .ok_or_else(|| eyre::eyre!("unknown FSM: {}", name))
    }

    fn check_target(
        &self,
        targets: &HashMap<String, TransitionRule>,
        to: &str,
        role: &str,
        def: &FsmDefinition,
        from: &str,
        context: Option<&str>,
    ) -> eyre::Result<Transition> {
        match targets.get(to) {
            Some(rule) if rule.by.is_empty() || rule.by.iter().any(|r| r == role) => Ok(Transition::Changed),
            _ => Err(self.rich_error(def, from, to, role, context)),
        }
    }

    /// Build a rich error message with hints about valid targets and overrides.
    fn rich_error(&self, def: &FsmDefinition, from: &str, to: &str, role: &str, context: Option<&str>) -> eyre::Report {
        let mut msg = format!("invalid {} transition: {} -> {} (role: {})", def.name, from, to, role);

        // Hint: valid normal targets from this state
        if let Some(trans) = def.transitions.get(from) {
            let hints: Vec<String> = trans
                .iter()
                .map(|(target, rule)| {
                    if rule.by.is_empty() {
                        format!("{} (any)", target)
                    } else {
                        format!("{} ({})", target, rule.by.join(", "))
                    }
                })
                .collect();
            if !hints.is_empty() {
                msg.push_str(&format!("\n  hint: valid targets from {}: {}", from, hints.join(", ")));
            }
        }

        // Hint: valid override targets from this state
        if let Some(overrides) = def.overrides.get(from) {
            let hints: Vec<String> = overrides
                .iter()
                .map(|(target, rule)| {
                    if rule.by.is_empty() {
                        format!("{} (any)", target)
                    } else {
                        format!("{} ({})", target, rule.by.join(", "))
                    }
                })
                .collect();
            if !hints.is_empty() {
                msg.push_str(&format!("\n  hint: with overrides: {}", hints.join(", ")));
            }
        }

        if let Some(ctx) = context {
            msg.push_str(&format!("\n  {}", ctx));
        }

        eyre::eyre!("{}", msg)
    }
}
