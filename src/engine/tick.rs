use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::engine::resolve;
use crate::engine::schema::{ActionStep, StrategyDefinition};
use crate::primitive::registry::PrimitiveRegistry;
use crate::primitive::types::{PrimitiveContext, PrimitiveOutput};
use crate::trigger::evaluate::{TriggerEvaluator, TriggerResult};
use crate::trigger::observe::ObservationCtx;

/// Maximum inner-loop iterations for run-until-stable reconciliation.
const MAX_CONVERGENCE_ITERATIONS: usize = 10;

/// Outcome of a single engine tick.
#[derive(Debug)]
pub struct TickOutcome {
    /// Number of strategies executed this tick.
    pub strategies_fired: usize,
    /// Number of inner-loop convergence iterations (pull triggers).
    pub convergence_iterations: usize,
    /// Whether any strategy failed during execution.
    pub had_failures: bool,
}

/// A matched strategy ready for execution: strategy index + scope ID.
/// Uses an index into CompositionEngine::strategies to avoid borrow conflicts.
struct PendingExecution {
    strategy_idx: usize,
    priority: u32,
    scope_id: String,
    trigger_payload: Option<Value>,
}

/// The composition engine: drives all orchestration by wiring triggers to primitives.
pub struct CompositionEngine {
    strategies: Vec<StrategyDefinition>,
    trigger_evaluator: TriggerEvaluator,
    primitives: PrimitiveRegistry,
    /// Per-(strategy-name, scope-id) cooldown tracking.
    cooldowns: HashMap<(String, String), Instant>,
}

impl CompositionEngine {
    /// Create a new engine with the given strategies, primitives, and trigger evaluator.
    pub fn new(
        strategies: Vec<StrategyDefinition>,
        primitives: PrimitiveRegistry,
        trigger_evaluator: TriggerEvaluator,
    ) -> Self {
        info!(
            "CompositionEngine::new: {} strategies, {} primitives",
            strategies.len(),
            primitives.len()
        );
        Self {
            strategies,
            trigger_evaluator,
            primitives,
            cooldowns: HashMap::new(),
        }
    }

    /// Run one engine tick. Called by the daemon's event loop.
    ///
    /// 1. Evaluate push triggers (event-driven) and execute matching strategies.
    /// 2. Run-until-stable inner loop: evaluate pull triggers (state-driven),
    ///    execute matching strategies, repeat until no triggers fire or
    ///    MAX_CONVERGENCE_ITERATIONS is reached.
    pub async fn tick(&mut self, ctx: &mut EngineContext<'_>) -> eyre::Result<TickOutcome> {
        let mut total_fired = 0;
        let mut had_failures = false;

        // Phase 1: Push triggers (event-driven, single pass)
        let pending = {
            let obs = ObservationCtx::new(ctx.stores, ctx.events, ctx.now);
            let push_results = self.trigger_evaluator.evaluate_push(&obs);
            debug!("tick: {} push triggers fired", push_results.len());
            self.collect_pending(&push_results)
        };
        let (fired, failures) = self.execute_pending(pending, ctx).await;
        total_fired += fired;
        if failures {
            had_failures = true;
        }

        // Phase 2: Pull triggers (state-driven, run-until-stable)
        let mut convergence_iterations = 0;
        loop {
            convergence_iterations += 1;
            if convergence_iterations > MAX_CONVERGENCE_ITERATIONS {
                warn!(
                    "tick: convergence loop hit max iterations ({}), breaking",
                    MAX_CONVERGENCE_ITERATIONS
                );
                break;
            }

            let pending = {
                let obs = ObservationCtx::new(ctx.stores, ctx.events, ctx.now);
                let pull_results = self.trigger_evaluator.evaluate_pull(&obs);
                if pull_results.is_empty() {
                    debug!("tick: convergence reached after {} iterations", convergence_iterations);
                    break;
                }
                debug!(
                    "tick: convergence iteration {}, {} pull triggers fired",
                    convergence_iterations,
                    pull_results.len()
                );
                self.collect_pending(&pull_results)
            };

            if pending.is_empty() {
                debug!("tick: no strategies matched fired triggers, converged");
                break;
            }

            let (fired, failures) = self.execute_pending(pending, ctx).await;
            total_fired += fired;
            if failures {
                had_failures = true;
            }
        }

        Ok(TickOutcome {
            strategies_fired: total_fired,
            convergence_iterations,
            had_failures,
        })
    }

    /// Match fired triggers to strategies and explode scope_ids into individual executions.
    /// Returns fully owned PendingExecution values to avoid borrow conflicts with &mut self.
    fn collect_pending(&self, fired: &[(String, TriggerResult)]) -> Vec<PendingExecution> {
        let mut pending = Vec::new();

        for (trigger_name, result) in fired {
            let TriggerResult::Fired { scope_ids, payload } = result else {
                continue;
            };

            // Find all enabled strategies that reference this trigger (by index)
            let matching: Vec<usize> = self
                .strategies
                .iter()
                .enumerate()
                .filter(|(_, s)| s.enabled && s.trigger == *trigger_name)
                .map(|(i, _)| i)
                .collect();

            if matching.is_empty() {
                debug!("tick: trigger '{}' fired but no strategy references it", trigger_name);
                continue;
            }

            for idx in matching {
                let strategy = &self.strategies[idx];
                if scope_ids.is_empty() {
                    if !self.is_cooled_down(&strategy.name, "") {
                        pending.push(PendingExecution {
                            strategy_idx: idx,
                            priority: strategy.priority,
                            scope_id: String::new(),
                            trigger_payload: payload.clone(),
                        });
                    }
                } else {
                    for sid in scope_ids {
                        if !self.is_cooled_down(&strategy.name, sid) {
                            pending.push(PendingExecution {
                                strategy_idx: idx,
                                priority: strategy.priority,
                                scope_id: sid.clone(),
                                trigger_payload: payload.clone(),
                            });
                        }
                    }
                }
            }
        }

        // Sort by priority (highest first), stable for same-priority ordering
        pending.sort_by(|a, b| b.priority.cmp(&a.priority));
        pending
    }

    /// Execute a list of pending strategy executions in priority order.
    /// Returns (strategies_fired, had_failures).
    async fn execute_pending(&mut self, pending: Vec<PendingExecution>, ctx: &mut EngineContext<'_>) -> (usize, bool) {
        let mut fired = 0;
        let mut had_failures = false;

        for exec in &pending {
            let strategy_name = self.strategies[exec.strategy_idx].name.clone();
            let strategy_scope = self.strategies[exec.strategy_idx].scope.clone();
            let cooldown_secs = self.strategies[exec.strategy_idx].cooldown_secs;

            debug!(
                "executing strategy '{}' for scope_id='{}' (priority={})",
                strategy_name, exec.scope_id, exec.priority
            );

            // Execute the strategy's action sequence
            let action_steps = self.strategies[exec.strategy_idx].action.clone();
            let result = self
                .execute_steps(
                    &action_steps,
                    &strategy_name,
                    &strategy_scope,
                    &exec.scope_id,
                    exec.trigger_payload.as_ref(),
                    ctx,
                )
                .await;

            match result {
                Ok(()) => {
                    debug!("strategy '{}' succeeded for '{}'", strategy_name, exec.scope_id);
                    let on_success = self.strategies[exec.strategy_idx].on_success.clone();
                    if !on_success.is_empty() {
                        let wiring_result = self
                            .execute_steps(
                                &on_success,
                                &strategy_name,
                                &strategy_scope,
                                &exec.scope_id,
                                exec.trigger_payload.as_ref(),
                                ctx,
                            )
                            .await;
                        if let Err(e) = wiring_result {
                            warn!(
                                "strategy '{}' on-success wiring failed for '{}': {}",
                                strategy_name, exec.scope_id, e
                            );
                            had_failures = true;
                        }
                    }
                }
                Err(e) => {
                    warn!("strategy '{}' failed for '{}': {}", strategy_name, exec.scope_id, e);
                    had_failures = true;
                    let on_failure = self.strategies[exec.strategy_idx].on_failure.clone();
                    if !on_failure.is_empty() {
                        let wiring_result = self
                            .execute_steps(
                                &on_failure,
                                &strategy_name,
                                &strategy_scope,
                                &exec.scope_id,
                                exec.trigger_payload.as_ref(),
                                ctx,
                            )
                            .await;
                        if let Err(e) = wiring_result {
                            warn!(
                                "strategy '{}' on-failure wiring also failed for '{}': {}",
                                strategy_name, exec.scope_id, e
                            );
                        }
                    }
                }
            }

            self.record_cooldown(&strategy_name, &exec.scope_id, cooldown_secs);
            fired += 1;
        }

        (fired, had_failures)
    }

    /// Execute a sequence of action steps with guard evaluation and $context threading.
    async fn execute_steps(
        &self,
        steps: &[ActionStep],
        strategy_name: &str,
        strategy_scope: &str,
        scope_id: &str,
        trigger_payload: Option<&Value>,
        ctx: &mut EngineContext<'_>,
    ) -> eyre::Result<()> {
        let mut strategy_ctx: HashMap<String, Value> = HashMap::new();

        for (i, step) in steps.iter().enumerate() {
            // Guard check: skip step (not whole strategy) on guard failure
            if let Some(guard_name) = &step.guard {
                let obs = ObservationCtx::new(ctx.stores, ctx.events, ctx.now);
                let passes = ctx
                    .guard_conditions
                    .as_ref()
                    .map(|gc| gc.evaluate(guard_name, &obs, strategy_scope, scope_id))
                    .unwrap_or(true);
                if !passes {
                    debug!(
                        "strategy '{}' step {} guard '{}' failed, skipping step",
                        strategy_name, i, guard_name
                    );
                    continue;
                }
            }

            let output = self
                .execute_step(step, scope_id, trigger_payload, &strategy_ctx, strategy_name, ctx)
                .await?;

            if let Some(name) = &step.name {
                strategy_ctx.insert(name.clone(), serde_json::to_value(&output.values)?);
            }
        }

        Ok(())
    }

    /// Execute a single action step: resolve params, look up primitive, call execute.
    async fn execute_step(
        &self,
        step: &ActionStep,
        scope_id: &str,
        trigger_payload: Option<&Value>,
        strategy_ctx: &HashMap<String, Value>,
        strategy_name: &str,
        ctx: &mut EngineContext<'_>,
    ) -> eyre::Result<PrimitiveOutput> {
        let primitive = self
            .primitives
            .get(&step.primitive)
            .ok_or_else(|| eyre::eyre!("strategy '{}': primitive '{}' not found", strategy_name, step.primitive))?;

        let resolved_params = resolve::resolve_params(&step.params, scope_id, trigger_payload, strategy_ctx)?;

        debug!(
            "strategy '{}': executing primitive '{}' with params: {}",
            strategy_name,
            step.primitive,
            serde_json::to_string(&resolved_params).unwrap_or_default()
        );

        let mut prim_ctx = PrimitiveContext {
            stores: ctx.stores,
            bridge: ctx.bridge,
            event_tx: ctx.event_tx,
            repo_path: ctx.repo_path,
            worktree_mgr: ctx.worktree_mgr,
            strategy_ctx: &mut HashMap::new(),
        };

        primitive.execute(&mut prim_ctx, resolved_params).await
    }

    // ─── Cooldown tracking ──────────────────────────────────────────────────

    fn is_cooled_down(&self, strategy_name: &str, scope_id: &str) -> bool {
        let key = (strategy_name.to_owned(), scope_id.to_owned());
        if let Some(last_fired) = self.cooldowns.get(&key) {
            let cooldown_secs = self
                .strategies
                .iter()
                .find(|s| s.name == strategy_name)
                .and_then(|s| s.cooldown_secs);
            if let Some(secs) = cooldown_secs {
                return last_fired.elapsed().as_secs() < secs as u64;
            }
        }
        false
    }

    fn record_cooldown(&mut self, strategy_name: &str, scope_id: &str, cooldown_secs: Option<u32>) {
        if cooldown_secs.is_some() {
            self.cooldowns
                .insert((strategy_name.to_owned(), scope_id.to_owned()), Instant::now());
        }
    }

    /// Sweep expired cooldown entries to prevent memory growth.
    pub fn sweep_cooldowns(&mut self) {
        self.cooldowns.retain(|(strategy_name, _), last_fired| {
            let cooldown_secs = self
                .strategies
                .iter()
                .find(|s| s.name == *strategy_name)
                .and_then(|s| s.cooldown_secs)
                .unwrap_or(0);
            last_fired.elapsed().as_secs() < (cooldown_secs as u64 + 3600)
        });
    }
}

/// Context available to the engine during a tick.
pub struct EngineContext<'a> {
    pub stores: &'a crate::daemon::context::Stores,
    pub events: &'a [crate::ipc::protocol::DaemonEvent],
    pub event_tx: &'a tokio::sync::broadcast::Sender<crate::ipc::protocol::DaemonEvent>,
    pub bridge: &'a crate::agents::bridge::AgentIpcBridge,
    pub repo_path: &'a Path,
    pub worktree_mgr: &'a crate::worktree::manager::WorktreeManager,
    pub now: i64,
    /// Guard condition registry for step-level guard evaluation. None in minimal test contexts.
    pub guard_conditions: Option<&'a crate::trigger::observe::GuardConditionRegistry>,
}
