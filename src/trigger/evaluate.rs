use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tracing::debug;

use crate::trigger::observe::{ObservationCtx, StateQueryRegistry, flexible_eq};
use crate::trigger::schema::{CompositeOperator, CountQuery, Operator, TriggerDefinition, TriggerKind};

/// Result of evaluating a single trigger.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerResult {
    /// Trigger condition not met.
    Idle,
    /// Trigger fired for the given scope IDs.
    Fired {
        scope_ids: Vec<String>,
        /// Event payload (for event triggers) or computed values (for ratio triggers).
        payload: Option<Value>,
    },
}

/// Evaluates trigger definitions against runtime state.
///
/// Supports both pull-mode (per-tick: threshold, ratio, timer, state-query, composite)
/// and push-mode (per-event: event triggers) evaluation. Tracks cooldown/throttle state
/// per (trigger-name, scope-id) pair to prevent trigger storms.
pub struct TriggerEvaluator {
    triggers: Vec<TriggerDefinition>,
    index: HashMap<String, usize>,
    state_queries: StateQueryRegistry,
    /// Cooldown/throttle tracking: (trigger-name, scope-id) -> last_fired_at (millis).
    cooldowns: HashMap<(String, String), i64>,
}

impl TriggerEvaluator {
    /// Create a new evaluator with the given trigger definitions and state query registry.
    pub fn new(triggers: Vec<TriggerDefinition>, state_queries: StateQueryRegistry) -> Self {
        debug!(
            "TriggerEvaluator::new: {} triggers, {} state queries",
            triggers.len(),
            state_queries.names().len()
        );
        let index = triggers.iter().enumerate().map(|(i, t)| (t.name.clone(), i)).collect();
        Self {
            triggers,
            index,
            state_queries,
            cooldowns: HashMap::new(),
        }
    }

    /// Evaluate all pull-mode triggers (threshold, ratio, timer, state-query, composite).
    /// Returns (trigger-name, TriggerResult::Fired) for each trigger that fired.
    pub fn evaluate_pull(&mut self, ctx: &ObservationCtx<'_>) -> Vec<(String, TriggerResult)> {
        debug!("evaluate_pull: {} triggers", self.triggers.len());
        self.sweep_cooldowns(ctx.now);
        let names: Vec<String> = self
            .triggers
            .iter()
            .filter(|t| !matches!(t.kind, TriggerKind::Event { .. }))
            .map(|t| t.name.clone())
            .collect();
        let mut results = Vec::new();
        for name in names {
            let result = self.evaluate_with_cooldown(&name, ctx);
            if let TriggerResult::Fired { .. } = &result {
                results.push((name, result));
            }
        }
        results
    }

    /// Evaluate all event triggers against the current event bus.
    /// Returns (trigger-name, TriggerResult::Fired) for each trigger that fired.
    pub fn evaluate_push(&mut self, ctx: &ObservationCtx<'_>) -> Vec<(String, TriggerResult)> {
        debug!("evaluate_push: checking event bus ({} events)", ctx.event_bus.len());
        let names: Vec<String> = self
            .triggers
            .iter()
            .filter(|t| matches!(t.kind, TriggerKind::Event { .. }))
            .map(|t| t.name.clone())
            .collect();
        let mut results = Vec::new();
        for name in names {
            let result = self.evaluate_with_cooldown(&name, ctx);
            if let TriggerResult::Fired { .. } = &result {
                results.push((name, result));
            }
        }
        results
    }

    /// Sweep expired cooldown entries to prevent memory leaks.
    /// Deterministic TTL: remove entries older than 1 hour.
    pub fn sweep_cooldowns(&mut self, now: i64) {
        const MAX_AGE_MS: i64 = 3_600_000;
        self.cooldowns.retain(|_, last_fired| now - *last_fired < MAX_AGE_MS);
    }

    // ─── Evaluation with cooldown ───────────────────────────────────────────

    fn evaluate_with_cooldown(&mut self, name: &str, ctx: &ObservationCtx<'_>) -> TriggerResult {
        let raw = self.evaluate_raw(name, ctx);
        let idx = match self.index.get(name).copied() {
            Some(i) => i,
            None => return TriggerResult::Idle,
        };
        let def = &self.triggers[idx];
        let cooldown = match &def.kind {
            TriggerKind::Event { throttle_secs, .. } => *throttle_secs,
            _ => def.cooldown_secs,
        };
        self.apply_cooldown(name, cooldown, ctx.now, raw)
    }

    // ─── Raw evaluation (no cooldown/throttle) ──────────────────────────────

    fn evaluate_raw(&self, name: &str, ctx: &ObservationCtx<'_>) -> TriggerResult {
        let idx = match self.index.get(name).copied() {
            Some(i) => i,
            None => return TriggerResult::Idle,
        };
        let def = &self.triggers[idx];
        match &def.kind {
            TriggerKind::Threshold {
                scope,
                field,
                operator,
                value,
            } => Self::eval_threshold(ctx, scope, field, operator, *value),
            TriggerKind::Ratio {
                scope,
                numerator,
                denominator,
                operator,
                value,
            } => Self::eval_ratio(ctx, scope, numerator, denominator, operator, *value),
            TriggerKind::Event {
                event,
                scope,
                match_filter,
                ..
            } => Self::eval_event(ctx, event, scope.as_deref(), match_filter),
            TriggerKind::Timer {
                scope,
                start_field,
                max_duration_secs,
            } => Self::eval_timer(ctx, scope, start_field, *max_duration_secs),
            TriggerKind::StateQuery { scope, query, params } => self.eval_state_query(ctx, scope, query, params),
            TriggerKind::Composite { operator, triggers } => self.eval_composite(ctx, operator, triggers),
        }
    }

    // ─── Trigger type evaluators ────────────────────────────────────────────

    /// Threshold: fires when a numeric field on scoped records meets the condition.
    fn eval_threshold(
        ctx: &ObservationCtx<'_>,
        scope: &str,
        field: &str,
        operator: &Operator,
        value: f64,
    ) -> TriggerResult {
        let ids = ctx.record_ids(scope);
        let matching: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                ctx.get_field_numeric(scope, id, field)
                    .map(|v| compare(v, operator, value))
                    .unwrap_or(false)
            })
            .collect();
        if matching.is_empty() {
            TriggerResult::Idle
        } else {
            debug!(
                "threshold fired: scope={scope} field={field} matched {} records",
                matching.len()
            );
            TriggerResult::Fired {
                scope_ids: matching,
                payload: None,
            }
        }
    }

    /// Ratio: fires when count(numerator)/count(denominator) among children meets the condition.
    fn eval_ratio(
        ctx: &ObservationCtx<'_>,
        scope: &str,
        numerator: &CountQuery,
        denominator: &CountQuery,
        operator: &Operator,
        value: f64,
    ) -> TriggerResult {
        let ids = ctx.record_ids(scope);
        let matching: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                let num = ctx.count_children(id, &numerator.collection, &numerator.filter);
                let den = ctx.count_children(id, &denominator.collection, &denominator.filter);
                if den == 0 {
                    return false;
                }
                let ratio = num as f64 / den as f64;
                compare(ratio, operator, value)
            })
            .collect();
        if matching.is_empty() {
            TriggerResult::Idle
        } else {
            TriggerResult::Fired {
                scope_ids: matching,
                payload: None,
            }
        }
    }

    /// Event: fires when a matching event exists on the event bus.
    fn eval_event(
        ctx: &ObservationCtx<'_>,
        event_type: &str,
        scope: Option<&str>,
        match_filter: &HashMap<String, Value>,
    ) -> TriggerResult {
        let matching: Vec<_> = ctx
            .event_bus
            .iter()
            .filter(|e| {
                e.event == event_type
                    && match_filter
                        .iter()
                        .all(|(k, expected)| e.data.get(k).map(|v| flexible_eq(v, expected)).unwrap_or(false))
            })
            .collect();

        if matching.is_empty() {
            return TriggerResult::Idle;
        }

        // Extract scope_ids from event data. Convention: events include either
        // an `id` field (transition/record events) or a `{scope}_id` field.
        let scope_ids: Vec<String> = if let Some(scope) = scope {
            let scope_key = format!("{scope}_id");
            matching
                .iter()
                .filter_map(|e| {
                    e.data
                        .get("id")
                        .or_else(|| e.data.get(&scope_key))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                })
                .collect::<HashSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };

        let payload = matching.first().map(|e| e.data.clone());
        TriggerResult::Fired { scope_ids, payload }
    }

    /// Timer: fires when elapsed time since a timestamp field exceeds the limit.
    fn eval_timer(ctx: &ObservationCtx<'_>, scope: &str, start_field: &str, max_duration_secs: u64) -> TriggerResult {
        let max_duration_ms = max_duration_secs as i64 * 1000;
        let ids = ctx.record_ids(scope);
        let matching: Vec<String> = ids
            .into_iter()
            .filter(|id| {
                ctx.get_field_timestamp(scope, id, start_field)
                    .map(|ts| ts > 0 && (ctx.now - ts) > max_duration_ms)
                    .unwrap_or(false)
            })
            .collect();
        if matching.is_empty() {
            TriggerResult::Idle
        } else {
            TriggerResult::Fired {
                scope_ids: matching,
                payload: None,
            }
        }
    }

    /// State-query: fires when a named state query returns true for scoped records.
    fn eval_state_query(
        &self,
        ctx: &ObservationCtx<'_>,
        scope: &str,
        query: &str,
        params: &HashMap<String, Value>,
    ) -> TriggerResult {
        let ids = ctx.record_ids(scope);
        let matching: Vec<String> = ids
            .into_iter()
            .filter(|id| self.state_queries.evaluate(query, ctx, scope, id, params))
            .collect();
        if matching.is_empty() {
            TriggerResult::Idle
        } else {
            TriggerResult::Fired {
                scope_ids: matching,
                payload: None,
            }
        }
    }

    /// Composite: combines sub-triggers with boolean logic.
    fn eval_composite(
        &self,
        ctx: &ObservationCtx<'_>,
        operator: &CompositeOperator,
        triggers: &[String],
    ) -> TriggerResult {
        match operator {
            CompositeOperator::And => {
                let results: Vec<TriggerResult> = triggers.iter().map(|name| self.evaluate_raw(name, ctx)).collect();
                if results.iter().any(|r| matches!(r, TriggerResult::Idle)) {
                    return TriggerResult::Idle;
                }
                // Intersection of all scope_ids.
                let mut sets: Vec<HashSet<String>> = results
                    .into_iter()
                    .map(|r| match r {
                        TriggerResult::Fired { scope_ids, .. } => scope_ids.into_iter().collect(),
                        TriggerResult::Idle => HashSet::new(),
                    })
                    .collect();
                if sets.is_empty() {
                    return TriggerResult::Idle;
                }
                let first = sets.remove(0);
                let common: HashSet<String> = sets
                    .into_iter()
                    .fold(first, |acc, s| acc.intersection(&s).cloned().collect());
                if common.is_empty() {
                    TriggerResult::Idle
                } else {
                    TriggerResult::Fired {
                        scope_ids: common.into_iter().collect(),
                        payload: None,
                    }
                }
            }
            CompositeOperator::Or => {
                let mut all_ids = HashSet::new();
                let mut any_fired = false;
                for name in triggers {
                    if let TriggerResult::Fired { scope_ids, .. } = self.evaluate_raw(name, ctx) {
                        any_fired = true;
                        all_ids.extend(scope_ids);
                    }
                }
                if any_fired {
                    TriggerResult::Fired {
                        scope_ids: all_ids.into_iter().collect(),
                        payload: None,
                    }
                } else {
                    TriggerResult::Idle
                }
            }
            CompositeOperator::Not => {
                let name = &triggers[0];
                match self.evaluate_raw(name, ctx) {
                    TriggerResult::Idle => TriggerResult::Fired {
                        scope_ids: Vec::new(),
                        payload: None,
                    },
                    TriggerResult::Fired { .. } => TriggerResult::Idle,
                }
            }
        }
    }

    // ─── Cooldown/throttle ──────────────────────────────────────────────────

    fn apply_cooldown(
        &mut self,
        trigger_name: &str,
        cooldown_secs: Option<u32>,
        now: i64,
        raw: TriggerResult,
    ) -> TriggerResult {
        match raw {
            TriggerResult::Idle => TriggerResult::Idle,
            TriggerResult::Fired { scope_ids, payload } => {
                let cooldown_ms = match cooldown_secs {
                    Some(secs) if secs > 0 => secs as i64 * 1000,
                    _ => {
                        // No cooldown: record fire time and return as-is.
                        for sid in &scope_ids {
                            self.cooldowns.insert((trigger_name.to_owned(), sid.clone()), now);
                        }
                        return TriggerResult::Fired { scope_ids, payload };
                    }
                };
                // Filter out scope_ids still within cooldown window.
                let active: Vec<String> = scope_ids
                    .into_iter()
                    .filter(|sid| {
                        let key = (trigger_name.to_owned(), sid.clone());
                        match self.cooldowns.get(&key) {
                            Some(&last_fired) => now - last_fired >= cooldown_ms,
                            None => true,
                        }
                    })
                    .collect();
                if active.is_empty() {
                    TriggerResult::Idle
                } else {
                    for sid in &active {
                        self.cooldowns.insert((trigger_name.to_owned(), sid.clone()), now);
                    }
                    TriggerResult::Fired {
                        scope_ids: active,
                        payload,
                    }
                }
            }
        }
    }
}

// ─── Free functions ─────────────────────────────────────────────────────────

/// Compare two f64 values with the given operator.
fn compare(lhs: f64, op: &Operator, rhs: f64) -> bool {
    match op {
        Operator::Gte => lhs >= rhs,
        Operator::Gt => lhs > rhs,
        Operator::Lte => lhs <= rhs,
        Operator::Lt => lhs < rhs,
        Operator::Eq => (lhs - rhs).abs() < f64::EPSILON,
        Operator::Ne => (lhs - rhs).abs() >= f64::EPSILON,
    }
}
