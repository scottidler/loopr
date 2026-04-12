use std::collections::HashMap;
use std::path::Path;

use tracing::info;

use super::schema::{self, FsmDefinition};
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
    /// or Err if the transition is invalid.
    pub fn validate_transition(&self, fsm_name: &str, from: &str, to: &str, role: &str) -> eyre::Result<Transition> {
        if from == to {
            return Ok(Transition::Unchanged);
        }
        let def = self.get_definition(fsm_name)?;
        let targets = def
            .transitions
            .get(from)
            .ok_or_else(|| invalid_transition(fsm_name, from, to, role, None))?;
        self.check_target(targets, to, role, fsm_name, from)
    }

    /// Validate an override transition.
    /// Checks normal transitions first, then override edges.
    pub fn validate_override(&self, fsm_name: &str, from: &str, to: &str, role: &str) -> eyre::Result<Transition> {
        if from == to {
            return Ok(Transition::Unchanged);
        }
        // Try normal transition first
        match self.validate_transition(fsm_name, from, to, role) {
            Ok(result) => Ok(result),
            Err(normal_err) => {
                let def = self.get_definition(fsm_name)?;
                let targets = def.overrides.get(from).ok_or_else(|| {
                    invalid_transition(
                        fsm_name,
                        from,
                        to,
                        role,
                        Some(&format!("normal transition also failed: {}", normal_err)),
                    )
                })?;
                self.check_target(targets, to, role, fsm_name, from)
            }
        }
    }

    /// Check if a state is terminal.
    pub fn is_terminal(&self, fsm_name: &str, state: &str) -> eyre::Result<bool> {
        let def = self.get_definition(fsm_name)?;
        Ok(def.terminal.iter().any(|t| t == state))
    }

    /// Get all valid target states from a given state for a role.
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
        targets: &HashMap<String, super::schema::TransitionRule>,
        to: &str,
        role: &str,
        fsm_name: &str,
        from: &str,
    ) -> eyre::Result<Transition> {
        match targets.get(to) {
            Some(rule) if rule.by.is_empty() || rule.by.iter().any(|r| r == role) => Ok(Transition::Changed),
            Some(_) => Err(invalid_transition(fsm_name, from, to, role, None)),
            None => Err(invalid_transition(fsm_name, from, to, role, None)),
        }
    }
}

fn invalid_transition(fsm: &str, from: &str, to: &str, role: &str, context: Option<&str>) -> eyre::Report {
    let base = format!("invalid {} transition: {} -> {} (role: {})", fsm, from, to, role);
    match context {
        Some(ctx) => eyre::eyre!("{}\n  {}", base, ctx),
        None => eyre::eyre!("{}", base),
    }
}
