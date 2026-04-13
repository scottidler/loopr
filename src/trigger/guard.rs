use std::collections::HashSet;

use tracing::debug;

use crate::fsm::schema::FsmDefinition;
use crate::trigger::observe::{GuardConditionRegistry, ObservationCtx};

/// Evaluates guard conditions against FSM transitions.
///
/// Guards are synchronous, pure-read conditions that must pass before an FSM
/// transition is allowed. They are evaluated after structural validation
/// (`FsmInterpreter::validate_transition`) confirms the edge exists and the
/// role is authorized.
///
/// Usage pattern:
/// ```ignore
/// fsm.validate_transition(name, from, to, role)?;   // structural check
/// guard_eval.check_transition(def, from, to, collection, id, ctx)?; // guard check
/// ```
pub struct GuardEvaluator {
    conditions: GuardConditionRegistry,
}

impl GuardEvaluator {
    /// Create a new evaluator backed by the given condition registry.
    pub fn new(conditions: GuardConditionRegistry) -> Self {
        debug!("GuardEvaluator::new: {} conditions", conditions.names().len());
        Self { conditions }
    }

    /// Check all guards on the `from -> to` transition in `def` for the record at
    /// `collection/id`. Returns `Ok(())` if all guards pass. Returns `Err` with the
    /// first rejection message if any guard condition returns false.
    ///
    /// Guards with `on-failure: warn` are not yet implemented as a separate code path;
    /// all guards currently reject on failure. Warn semantics are reserved for a future
    /// iteration when the composition engine can consume warnings as events.
    pub fn check_transition(
        &self,
        def: &FsmDefinition,
        from: &str,
        to: &str,
        collection: &str,
        id: &str,
        ctx: &ObservationCtx<'_>,
    ) -> eyre::Result<()> {
        debug!(
            "GuardEvaluator::check_transition: {}.{}->{} for {}/{}",
            def.name, from, to, collection, id
        );
        for (guard_name, guard) in &def.guards {
            if guard.from != from || guard.to != to {
                continue;
            }
            let passes = self.conditions.evaluate(&guard.condition, ctx, collection, id);
            if !passes {
                let msg = if guard.message.is_empty() {
                    format!("guard condition '{}' failed", guard.condition)
                } else {
                    guard.message.clone()
                };
                eyre::bail!("guard '{}' rejected {}->{}: {}", guard_name, from, to, msg);
            }
        }
        Ok(())
    }

    /// Validate that every guard condition name referenced in the given FSM
    /// definitions is registered in `registry`. Returns one error string per
    /// unknown condition. Called at daemon startup alongside FSM schema validation.
    pub fn validate_conditions(defs: &[FsmDefinition], registry: &GuardConditionRegistry) -> Vec<String> {
        let known: HashSet<&str> = registry.names().into_iter().collect();
        let mut errors = Vec::new();
        for def in defs {
            for (guard_name, guard) in &def.guards {
                if !known.contains(guard.condition.as_str()) {
                    errors.push(format!(
                        "FSM '{}' guard '{}': unknown condition '{}'",
                        def.name, guard_name, guard.condition
                    ));
                }
            }
        }
        errors
    }
}
