//! Deterministic Integrator task — no LLM.
//!
//! The Integrator automates the Tick lifecycle: find Accepted Bundles, create a Tick,
//! seal it, run validation commands, then publish or fail. Every decision is an
//! if/then/else on data from the stores — no prompts, no parsing, no temperature.

use std::time::Duration;

use async_trait::async_trait;
use eyre::{Result, eyre};

use crate::agents::{Agent, AgentContext, AgentType};
use crate::config::IntegratorConfig;
use crate::domain::bundle::BundleStatus;
use crate::domain::tick::TickStatus;
use crate::ipc::protocol::DaemonEvent;

/// The Integrator agent — wraps AgentContext + IntegratorConfig.
pub struct IntegratorAgent {
    pub ctx: AgentContext,
    config: IntegratorConfig,
}

impl IntegratorAgent {
    pub fn new(ctx: AgentContext, config: IntegratorConfig) -> Self {
        Self { ctx, config }
    }
}

#[async_trait]
impl Agent for IntegratorAgent {
    async fn run(&mut self) -> Result<()> {
        self.ctx.debug(&format!("run(session_id={})", self.ctx.session.id));
        self.ctx.info(&format!(
            "Integrator started (interval: {}s)",
            self.config.interval_secs
        ));

        let interval = Duration::from_secs(self.config.interval_secs);

        loop {
            if self.ctx.is_cancelled() {
                self.ctx.info("cancelled, exiting loop");
                return Ok(());
            }

            self.ctx.session.iteration = self.ctx.session.iteration.saturating_add(1);

            match self.run_cycle() {
                Ok(result) => {
                    let summary = match &result {
                        IntegratorCycleResult::Idle => "idle".to_string(),
                        IntegratorCycleResult::Published { tick_id } => {
                            format!("published tick {}", tick_id)
                        }
                        IntegratorCycleResult::ValidationFailed { tick_id, .. } => {
                            format!("tick {} validation failed", tick_id)
                        }
                        IntegratorCycleResult::StaleRejected { count } => {
                            format!("rejected {} stale bundles", count)
                        }
                        IntegratorCycleResult::Recovered { count } => {
                            format!("recovered {} stuck ticks", count)
                        }
                    };

                    if result != IntegratorCycleResult::Idle {
                        let _ = self.ctx.event_tx.send(DaemonEvent::new(
                            "integrator.cycle",
                            serde_json::json!({
                                "session_id": self.ctx.session.id,
                                "iteration": self.ctx.session.iteration,
                                "result": summary,
                            }),
                        ));
                    }
                }
                Err(e) => {
                    self.ctx.error(&format!("cycle error: {}", e));
                    let _ = self.ctx.event_tx.send(DaemonEvent::new(
                        "integrator.error",
                        serde_json::json!({
                            "session_id": self.ctx.session.id,
                            "iteration": self.ctx.session.iteration,
                            "error": e.to_string(),
                        }),
                    ));
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Integrator
    }
}

impl IntegratorAgent {
    /// Find the latest published Tick number from the stores.
    fn latest_published_tick_id(&self) -> Option<String> {
        self.ctx.debug("latest_published_tick_id()");
        let ticks = self.ctx.stores.read_ticks().ok()?;
        ticks
            .values()
            .filter(|t| t.status == TickStatus::Published)
            .max_by_key(|t| t.number)
            .map(|t| t.id.clone())
    }

    /// Get the next tick number (max existing + 1, or 1 if none).
    fn next_tick_number(&self) -> Result<u32> {
        self.ctx.debug("next_tick_number()");
        let ticks = self.ctx.stores.read_ticks()?;
        Ok(ticks.values().map(|t| t.number).max().unwrap_or(0) + 1)
    }

    /// Check if there's already a Tick in a non-terminal state (Open, Sealing, Validating).
    fn has_tick_in_progress(&self) -> Result<bool> {
        self.ctx.debug("has_tick_in_progress()");
        let ticks = self.ctx.stores.read_ticks()?;
        Ok(ticks.values().any(|t| {
            matches!(
                t.status,
                TickStatus::Open | TickStatus::Sealing | TickStatus::Validating
            )
        }))
    }

    /// Recover stuck Ticks (Open/Sealing/Validating) from a previous crash.
    /// Returns the number of ticks recovered.
    fn recover_stuck_ticks(&self) -> Result<u32> {
        self.ctx.debug("recover_stuck_ticks()");
        let stuck_tick_ids: Vec<String> = {
            let ticks = self.ctx.stores.read_ticks()?;
            ticks
                .values()
                .filter(|t| {
                    t.status == TickStatus::Open
                        || t.status == TickStatus::Sealing
                        || t.status == TickStatus::Validating
                })
                .map(|t| t.id.clone())
                .collect()
        };

        let mut recovered = 0;
        for tick_id in &stuck_tick_ids {
            let current_status = {
                let ticks = self.ctx.stores.read_ticks()?;
                ticks.get(tick_id.as_str()).map(|t| t.status)
            };

            let Some(status) = current_status else {
                continue;
            };

            let mut ok = true;

            if status == TickStatus::Open {
                let resp = self.ctx.bridge.request(
                    "tick.transition",
                    serde_json::json!({
                        "id": tick_id,
                        "target_status": "Failed",
                        "role": "integrator",
                    }),
                );
                if resp.is_error() {
                    self.ctx.warn(&format!(
                        "failed to recover stuck tick {} (Open→Failed): {:?}",
                        tick_id, resp.error
                    ));
                    ok = false;
                }
            } else {
                if status == TickStatus::Sealing {
                    let resp = self.ctx.bridge.request(
                        "tick.transition",
                        serde_json::json!({
                            "id": tick_id,
                            "target_status": "Validating",
                            "role": "integrator",
                        }),
                    );
                    if resp.is_error() {
                        self.ctx.warn(&format!(
                            "failed to recover stuck tick {} (Sealing→Validating): {:?}",
                            tick_id, resp.error
                        ));
                        ok = false;
                    }
                }

                if ok {
                    let resp = self.ctx.bridge.request(
                        "tick.transition",
                        serde_json::json!({
                            "id": tick_id,
                            "target_status": "Failed",
                            "role": "integrator",
                        }),
                    );
                    if resp.is_error() {
                        self.ctx.warn(&format!(
                            "failed to recover stuck tick {} (→Failed): {:?}",
                            tick_id, resp.error
                        ));
                        ok = false;
                    }
                }
            }

            if ok {
                self.ctx.info(&format!("recovered stuck tick {} → Failed", tick_id));
                recovered += 1;

                let _ = self.ctx.bridge.request(
                    "learning.create",
                    serde_json::json!({
                        "content": format!("Tick {} was stuck after crash, recovered to Failed", tick_id),
                        "scope": "global",
                        "source_id": tick_id,
                    }),
                );
            }
        }
        Ok(recovered)
    }
}

/// Result of a single integrator cycle.
#[derive(Debug, PartialEq)]
pub enum IntegratorCycleResult {
    /// No work to do (no accepted bundles or tick already in progress).
    Idle,
    /// A Tick was created, validated, and published successfully.
    Published { tick_id: String },
    /// A Tick was created but validation failed.
    ValidationFailed { tick_id: String, log: String },
    /// Some bundles were rejected as stale.
    StaleRejected { count: usize },
    /// Stuck ticks were recovered from a crash.
    Recovered { count: u32 },
}

impl IntegratorAgent {
    /// Run a single integrator cycle. This is the core deterministic logic.
    ///
    /// 1. Recover any stuck Ticks.
    /// 2. Find Accepted Bundles.
    /// 3. Validate preconditions (base_tick_id matches latest published Tick).
    /// 4. Reject stale bundles.
    /// 5. Create Tick, seal it, run validation, publish or fail.
    pub fn run_cycle(&self) -> Result<IntegratorCycleResult> {
        self.ctx.debug("run_cycle()");
        // 1. Recover stuck ticks from crash
        let recovered = self.recover_stuck_ticks()?;
        if recovered > 0 {
            return Ok(IntegratorCycleResult::Recovered { count: recovered });
        }

        // 2. Check for in-progress ticks — don't create a new one if one exists
        if self.has_tick_in_progress()? {
            return Ok(IntegratorCycleResult::Idle);
        }

        // 3. Find all Accepted bundles
        let accepted_bundles: Vec<(String, Option<String>)> = {
            let bundles = self.ctx.stores.read_bundles()?;
            bundles
                .values()
                .filter(|b| b.status == BundleStatus::Accepted)
                .map(|b| (b.id.clone(), b.base_tick_id.clone()))
                .collect()
        };

        if accepted_bundles.is_empty() {
            return Ok(IntegratorCycleResult::Idle);
        }

        // 4. Validate-then-mutate: check preconditions before any mutations
        let latest_tick_id = self.latest_published_tick_id();

        // Partition into valid (matching base_tick_id) and stale
        let mut valid_bundle_ids: Vec<String> = Vec::new();
        let mut stale_bundle_ids: Vec<String> = Vec::new();
        for (id, base) in accepted_bundles {
            if base == latest_tick_id {
                valid_bundle_ids.push(id);
            } else {
                stale_bundle_ids.push(id);
            }
        }

        // 5. Handle stale bundles per StalePolicy (Gaps #19, #20)
        let stale_policy = self.ctx.stores.config.strategy.stale_policy;
        for stale_id in &stale_bundle_ids {
            match stale_policy {
                crate::config::StalePolicy::RejectIfStale => {
                    let resp = self.ctx.bridge.request(
                        "bundle.transition",
                        serde_json::json!({
                            "id": stale_id,
                            "target_status": "Rejected",
                            "role": "integrator",
                        }),
                    );
                    if resp.is_error() {
                        self.ctx
                            .warn(&format!("failed to reject stale bundle {}: {:?}", stale_id, resp.error));
                    } else {
                        self.ctx.info(&format!("rejected stale bundle {}", stale_id));
                        let wi_id = self
                            .ctx
                            .stores
                            .read_bundles()?
                            .get(stale_id.as_str())
                            .map(|b| b.work_id.clone())
                            .unwrap_or_default();
                        self.reset_work_after_bundle_rejection(&wi_id, "stale base tick");
                    }
                }
                crate::config::StalePolicy::ReplanAtSafePoint => {
                    let resp = self.ctx.bridge.request(
                        "bundle.transition",
                        serde_json::json!({
                            "id": stale_id,
                            "target_status": "Rejected",
                            "role": "integrator",
                        }),
                    );
                    if !resp.is_error() {
                        let wi_id = self
                            .ctx
                            .stores
                            .read_bundles()?
                            .get(stale_id.as_str())
                            .map(|b| b.work_id.clone())
                            .unwrap_or_default();
                        let _ = self.ctx.bridge.event_tx().send(DaemonEvent::new(
                            "bundle.stale_replan_needed",
                            serde_json::json!({"bundle_id": stale_id, "work_id": wi_id, "reason": "stale_base_tick"}),
                        ));
                        self.reset_work_after_bundle_rejection(&wi_id, "stale base tick");
                        self.ctx
                            .info(&format!("rejected stale bundle {} (replan at safe point)", stale_id));
                    }
                }
                crate::config::StalePolicy::AutoReplayAndVerify => {
                    let wi_id = {
                        let bundles = self.ctx.stores.read_bundles()?;
                        bundles
                            .get(stale_id.as_str())
                            .map(|b| b.work_id.clone())
                            .unwrap_or_default()
                    };
                    let new_base_ref = latest_tick_id
                        .as_ref()
                        .and_then(|tid| {
                            let ticks = self.ctx.stores.read_ticks().ok()?;
                            ticks
                                .values()
                                .find(|t| t.id == *tid)
                                .and_then(|t| t.integration_sha.clone())
                        })
                        .unwrap_or_else(|| "HEAD".to_string());
                    let refresh_resp = self.ctx.bridge.request(
                        "worktree.refresh",
                        serde_json::json!({
                            "work_id": wi_id,
                            "new_base_ref": new_base_ref,
                        }),
                    );
                    if refresh_resp.is_error() {
                        let _ = self.ctx.bridge.request(
                            "bundle.transition",
                            serde_json::json!({"id": stale_id, "target_status": "Rejected", "role": "integrator"}),
                        );
                        self.reset_work_after_bundle_rejection(&wi_id, "auto-replay failed");
                        self.ctx
                            .warn(&format!("auto-replay failed for bundle {}, rejected", stale_id));
                    } else {
                        let _ = self.ctx.bridge.request(
                            "bundle.update",
                            serde_json::json!({"id": stale_id, "base_tick_id": latest_tick_id}),
                        );
                        valid_bundle_ids.push(stale_id.clone());
                        self.ctx.info(&format!("auto-replayed stale bundle {}", stale_id));
                    }
                }
            }
        }

        if !stale_bundle_ids.is_empty() && valid_bundle_ids.is_empty() {
            return Ok(IntegratorCycleResult::StaleRejected {
                count: stale_bundle_ids.len(),
            });
        }

        if valid_bundle_ids.is_empty() {
            return Ok(IntegratorCycleResult::Idle);
        }

        // Gap #21: TickCadence::Batched check
        match &self.ctx.stores.config.strategy.tick_cadence {
            crate::config::TickCadence::Continuous => {
                // Process immediately — current behavior
            }
            crate::config::TickCadence::Batched {
                min_bundles,
                timeout_secs,
            } => {
                if (valid_bundle_ids.len() as u32) < *min_bundles {
                    let earliest = {
                        let bundles = self.ctx.stores.read_bundles()?;
                        valid_bundle_ids
                            .iter()
                            .filter_map(|id| bundles.get(id.as_str()).map(|b| b.updated_at))
                            .min()
                            .unwrap_or(0)
                    };
                    let elapsed_secs = (crate::id::now_millis() - earliest) / 1000;
                    if elapsed_secs < *timeout_secs as i64 {
                        return Ok(IntegratorCycleResult::Idle);
                    }
                }
            }
        }

        // 6. Create Tick
        let tick_number = self.next_tick_number()?;
        let create_resp = self
            .ctx
            .bridge
            .request("tick.create", serde_json::json!({ "number": tick_number }));
        if create_resp.is_error() {
            return Err(eyre!("Failed to create tick: {:?}", create_resp.error));
        }
        let tick_id = create_resp
            .result
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("tick.create did not return id"))?
            .to_string();

        self.ctx.info(&format!(
            "created Tick {} (number {}) with {} bundles",
            tick_id,
            tick_number,
            valid_bundle_ids.len()
        ));

        // 7. Transition Tick: Open → Sealing
        let seal_resp = self.ctx.bridge.request(
            "tick.transition",
            serde_json::json!({
                "id": tick_id,
                "target_status": "Sealing",
                "role": "integrator",
            }),
        );
        if seal_resp.is_error() {
            return Err(eyre!("Failed to seal tick {}: {:?}", tick_id, seal_resp.error));
        }

        // 8. Transition bundles: Accepted → Integrating
        for bundle_id in &valid_bundle_ids {
            let resp = self.ctx.bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Integrating",
                    "role": "integrator",
                }),
            );
            if resp.is_error() {
                self.ctx.warn(&format!(
                    "failed to transition bundle {} to Integrating: {:?}",
                    bundle_id, resp.error
                ));
            }
        }

        // Update tick with bundle IDs and attempted bundle IDs.
        let tick_to_persist = {
            let mut ticks = self.ctx.stores.write_ticks()?;
            if let Some(tick) = ticks.get_mut(&tick_id) {
                tick.bundle_ids = valid_bundle_ids.clone();
                tick.attempted_bundle_ids = valid_bundle_ids.clone();
                Some(tick.clone())
            } else {
                None
            }
        };

        if let Some(tick) = tick_to_persist
            && let Some(ref store) = self.ctx.stores.store
            && let Ok(mut s) = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
            && let Err(e) = s.update(tick)
        {
            self.ctx.warn(&format!("Failed to persist tick bundle_ids: {}", e));
        }

        // Gap #14: Merge bundle branches into integration branch
        let branches: Vec<String> = {
            let bundles = self.ctx.stores.read_bundles()?;
            valid_bundle_ids
                .iter()
                .filter_map(|id| bundles.get(id.as_str()))
                .filter(|b| !b.branch_name.is_empty())
                .map(|b| b.branch_name.clone())
                .collect()
        };
        let repo_path = &self.ctx.stores.config.project.repo_path;
        let is_git_repo = repo_path.join(".git").exists();
        if !branches.is_empty() && is_git_repo {
            let _git_guard = self.ctx.stores.lock_git()?;
            match merge_bundle_branches(repo_path, &branches) {
                Ok(sha) => {
                    let mut ticks = self.ctx.stores.write_ticks()?;
                    if let Some(tick) = ticks.get_mut(&tick_id) {
                        tick.integration_sha = Some(sha);
                    }
                }
                Err(e) => {
                    self.ctx.warn(&format!("merge failed: {}", e));
                    let fail_resp = self.ctx.bridge.request(
                        "tick.transition",
                        serde_json::json!({
                            "id": tick_id,
                            "target_status": "Failed",
                            "role": "integrator",
                        }),
                    );
                    if fail_resp.is_error() {
                        self.ctx
                            .error(&format!("failed to fail tick {}: {:?}", tick_id, fail_resp.error));
                    }

                    for bundle_id in &valid_bundle_ids {
                        let resp = self.ctx.bridge.request(
                            "bundle.transition",
                            serde_json::json!({
                                "id": bundle_id,
                                "target_status": "Rejected",
                                "role": "integrator",
                            }),
                        );
                        if resp.is_error() {
                            self.ctx.warn(&format!(
                                "failed to reject bundle {} after merge failure: {:?}",
                                bundle_id, resp.error
                            ));
                        } else {
                            let wi_id = self
                                .ctx
                                .stores
                                .read_bundles()?
                                .get(bundle_id.as_str())
                                .map(|b| b.work_id.clone())
                                .unwrap_or_default();
                            self.reset_work_after_bundle_rejection(&wi_id, "merge conflict");
                        }
                    }

                    return Ok(IntegratorCycleResult::ValidationFailed {
                        tick_id,
                        log: format!("Merge failed: {}", e),
                    });
                }
            }
        }

        // 9. Transition Tick: Sealing → Validating
        let validate_resp = self.ctx.bridge.request(
            "tick.transition",
            serde_json::json!({
                "id": tick_id,
                "target_status": "Validating",
                "role": "integrator",
            }),
        );
        if validate_resp.is_error() {
            return Err(eyre!(
                "Failed to transition tick {} to Validating: {:?}",
                tick_id,
                validate_resp.error
            ));
        }

        // 10. Emit validation.started and run validation commands
        let _ = self
            .ctx
            .bridge
            .event_tx()
            .send(DaemonEvent::validation_started(&tick_id));
        let (passed, validation_log) = run_validation_commands(&self.config.validation_commands);
        let _ = self
            .ctx
            .bridge
            .event_tx()
            .send(DaemonEvent::validation_completed(&tick_id, passed, &validation_log));

        // Update tick with validation log
        let tick_to_persist = {
            let mut ticks = self.ctx.stores.write_ticks()?;
            if let Some(tick) = ticks.get_mut(&tick_id) {
                tick.validation_log = validation_log.clone();
                Some(tick.clone())
            } else {
                None
            }
        };

        if let Some(tick) = tick_to_persist
            && let Some(ref store) = self.ctx.stores.store
            && let Ok(mut s) = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
            && let Err(e) = s.update(tick)
        {
            self.ctx.warn(&format!("Failed to persist tick validation_log: {}", e));
        }

        // 11. Publish or Fail
        if passed {
            let sha =
                get_git_head_sha(&self.ctx.stores.config.project.repo_path).unwrap_or_else(|| "unknown".to_string());

            let pub_resp = self.ctx.bridge.request(
                "tick.transition",
                serde_json::json!({
                    "id": tick_id,
                    "target_status": "Published",
                    "role": "integrator",
                }),
            );
            if pub_resp.is_error() {
                return Err(eyre!("Failed to publish tick {}: {:?}", tick_id, pub_resp.error));
            }

            let tick_to_persist = {
                let mut ticks = self.ctx.stores.write_ticks()?;
                if let Some(tick) = ticks.get_mut(&tick_id) {
                    tick.integration_sha = Some(sha);
                    Some(tick.clone())
                } else {
                    None
                }
            };

            if let Some(tick) = tick_to_persist
                && let Some(ref store) = self.ctx.stores.store
                && let Ok(mut s) = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                && let Err(e) = s.update(tick)
            {
                self.ctx.warn(&format!("Failed to persist tick integration_sha: {}", e));
            }

            for bundle_id in &valid_bundle_ids {
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": bundle_id,
                        "target_status": "Merged",
                        "role": "integrator",
                    }),
                );
                if resp.is_error() {
                    self.ctx.warn(&format!(
                        "failed to transition bundle {} to Merged: {:?}",
                        bundle_id, resp.error
                    ));
                }
            }

            // C1: Transition parent Works InReview → Integrated
            let merged_wi_ids: Vec<String> = {
                let bundles = self.ctx.stores.read_bundles()?;
                valid_bundle_ids
                    .iter()
                    .filter_map(|id| bundles.get(id.as_str()))
                    .map(|b| b.work_id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect()
            };

            for wi_id in &merged_wi_ids {
                let should_transition = {
                    let wis = self.ctx.stores.read_works()?;
                    wis.get(wi_id)
                        .map(|w| w.status == crate::domain::work::WorkStatus::InReview)
                        .unwrap_or(false)
                };
                if should_transition {
                    let resp = self.ctx.bridge.request(
                        "work.transition",
                        serde_json::json!({
                            "id": wi_id,
                            "target_status": "Integrated",
                            "role": "integrator",
                        }),
                    );
                    if resp.is_error() {
                        self.ctx.warn(&format!(
                            "failed to transition WI {} to Integrated: {:?}",
                            wi_id, resp.error
                        ));
                    } else {
                        self.ctx.info(&format!("Work {} transitioned to Integrated", wi_id));
                    }
                }
            }

            self.ctx.info(&format!("Tick {} published successfully", tick_id));
            Ok(IntegratorCycleResult::Published {
                tick_id: tick_id.clone(),
            })
        } else {
            let fail_resp = self.ctx.bridge.request(
                "tick.transition",
                serde_json::json!({
                    "id": tick_id,
                    "target_status": "Failed",
                    "role": "integrator",
                }),
            );
            if fail_resp.is_error() {
                self.ctx
                    .error(&format!("failed to fail tick {}: {:?}", tick_id, fail_resp.error));
            }

            for bundle_id in &valid_bundle_ids {
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": bundle_id,
                        "target_status": "Rejected",
                        "role": "integrator",
                    }),
                );
                if resp.is_error() {
                    self.ctx.warn(&format!(
                        "failed to reject bundle {} after validation failure: {:?}",
                        bundle_id, resp.error
                    ));
                } else {
                    let wi_id = self
                        .ctx
                        .stores
                        .read_bundles()?
                        .get(bundle_id.as_str())
                        .map(|b| b.work_id.clone())
                        .unwrap_or_default();
                    self.reset_work_after_bundle_rejection(&wi_id, "validation failure");
                }
            }

            let _ = self.ctx.bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": format!("Tick {} validation failed: {}", tick_id, validation_log),
                    "scope": "global",
                    "source_id": tick_id,
                }),
            );

            self.ctx.info(&format!("Tick {} validation failed", tick_id));
            Ok(IntegratorCycleResult::ValidationFailed {
                tick_id: tick_id.clone(),
                log: validation_log,
            })
        }
    }

    /// After rejecting a bundle, reset the parent Work to Ready so it can be re-assigned.
    fn reset_work_after_bundle_rejection(&self, work_id: &str, reason: &str) {
        if work_id.is_empty() {
            return;
        }
        let resp = self.ctx.bridge.request(
            "work.transition",
            serde_json::json!({
                "id": work_id,
                "target_status": "Ready",
                "role": "coordinator",
                "override": true,
            }),
        );
        if resp.is_error() {
            self.ctx.warn(&format!(
                "failed to reset Work {} to Ready after bundle rejection: {:?}",
                work_id, resp.error
            ));
        } else {
            self.ctx.info(&format!(
                "Work {} reset to Ready after bundle rejection ({})",
                work_id, reason
            ));
        }
        let _ = self.ctx.bridge.request(
            "learning.create",
            serde_json::json!({
                "content": format!("Bundle rejected ({}). Work reset to Ready for retry with updated main branch.", reason),
                "scope": "phase",
                "source_id": work_id,
            }),
        );
    }
}

/// Gap #14: Merge bundle branches into the integration branch.
/// Returns the HEAD SHA after all merges succeed.
fn merge_bundle_branches(repo_path: &std::path::Path, bundle_branches: &[String]) -> Result<String> {
    log::debug!(
        "merge_bundle_branches(repo={}, branches={:?})",
        repo_path.display(),
        bundle_branches,
    );
    for branch in bundle_branches {
        let output = std::process::Command::new("git")
            .args([
                "merge",
                "--no-ff",
                branch,
                "-m",
                &format!("Merge bundle branch {}", branch),
            ])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git merge {} failed to execute: {}", branch, e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Fix #3: Clean up half-merged state before returning error
            let _ = std::process::Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(repo_path)
                .output();
            return Err(eyre!("git merge {} failed (aborted): {}", branch, stderr));
        }
    }

    // Get HEAD SHA after merges
    let sha_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| eyre!("git rev-parse HEAD failed: {}", e))?;

    Ok(String::from_utf8_lossy(&sha_output.stdout).trim().to_string())
}

/// Run validation commands synchronously (same pattern as handlers.rs).
fn run_validation_commands(commands: &[String]) -> (bool, String) {
    let mut log = String::new();
    for cmd in commands {
        log.push_str(&format!("=== Running: {cmd} ===\n"));
        let output = std::process::Command::new("sh").arg("-c").arg(cmd).output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.is_empty() {
                    log.push_str(&stdout);
                    if !stdout.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !stderr.is_empty() {
                    log.push_str(&stderr);
                    if !stderr.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !out.status.success() {
                    log.push_str(&format!("=== FAILED (exit code {:?}) ===\n", out.status.code()));
                    return (false, log);
                }
                log.push_str("=== PASSED ===\n");
            }
            Err(e) => {
                log.push_str(&format!("=== FAILED to execute: {e} ===\n"));
                return (false, log);
            }
        }
    }
    (true, log)
}

/// Get the current git HEAD SHA in the given repo path.
fn get_git_head_sha(repo_path: &std::path::Path) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent_logger::AgentLogger;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentContext, AgentSession, AgentStatus, AgentType};
    use crate::config::{Config, ProjectConfig};
    use crate::daemon::context::Stores;
    use crate::domain::bundle::Bundle;
    use crate::domain::tick::Tick;
    use crate::domain::work::{Work, WorkStatus};
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use crate::worktree::manager::WorktreeManager;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};
    use taskstore::Store;
    use tokio::sync::broadcast;

    fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    fn test_agent_logger(dir: &std::path::Path) -> AgentLogger {
        let file_path = dir.join("test-integrator.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Integrator, "test-session", file, file_path)
    }

    fn test_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["true".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        }
    }

    fn failing_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["false".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        }
    }

    /// Create an IntegratorAgent for testing. The session is inserted into stores.
    fn test_integrator(dir: &std::path::Path, stores: Arc<Stores>, intg_config: IntegratorConfig) -> IntegratorAgent {
        let (event_tx, _) = broadcast::channel(64);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(AgentType::Integrator, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        IntegratorAgent::new(ctx, intg_config)
    }

    /// Create an IntegratorAgent with a custom config for Stores.
    fn test_integrator_with_stores_config(
        dir: &std::path::Path,
        stores: Arc<Stores>,
        intg_config: IntegratorConfig,
    ) -> (IntegratorAgent, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel(64);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(AgentType::Integrator, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx: event_tx.clone(),
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        (IntegratorAgent::new(ctx, intg_config), event_tx)
    }

    // --- is_cancelled tests (via AgentContext) ---

    #[test]
    fn test_is_cancelled_false() {
        let dir = TestDir::new("loopr-intg-canc1");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Integrator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let agent = test_integrator(&dir, stores, test_config());
        assert!(!agent.ctx.is_cancelled());
    }

    #[test]
    fn test_is_cancelled_true() {
        let dir = TestDir::new("loopr-intg-canc2");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Integrator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let agent = test_integrator(&dir, stores, test_config());
        // The agent's own session is Running (created by test_integrator), but the
        // pre-inserted cancelled session is a different ID. Test via AgentContext's
        // general is_cancelled which checks the agent's own session.
        // For this test, we need to cancel the agent's own session.
        {
            let mut sessions = agent.ctx.stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&agent.ctx.session.id) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        }
        assert!(agent.ctx.is_cancelled());
    }

    #[test]
    fn test_is_cancelled_missing() {
        let dir = TestDir::new("loopr-intg-canc3");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores.clone(), test_config());
        // Remove the session so it's "missing"
        stores.agent_sessions.write().unwrap().remove(&agent.ctx.session.id);
        assert!(agent.ctx.is_cancelled());
    }

    // --- Helper method tests ---

    #[test]
    fn test_latest_published_tick_id_none() {
        let dir = TestDir::new("loopr-intg-latest1");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores, test_config());
        assert!(agent.latest_published_tick_id().is_none());
    }

    #[test]
    fn test_latest_published_tick_id_some() {
        let dir = TestDir::new("loopr-intg-latest2");
        let stores = test_stores(&dir);

        let mut tick1 = Tick::new(1);
        tick1.status = TickStatus::Published;
        let mut tick2 = Tick::new(2);
        tick2.status = TickStatus::Published;
        let tick2_id = tick2.id.clone();
        let mut tick3 = Tick::new(3);
        tick3.status = TickStatus::Failed;

        stores.ticks.write().unwrap().insert(tick1.id.clone(), tick1);
        stores.ticks.write().unwrap().insert(tick2.id.clone(), tick2);
        stores.ticks.write().unwrap().insert(tick3.id.clone(), tick3);

        let agent = test_integrator(&dir, stores, test_config());
        assert_eq!(agent.latest_published_tick_id(), Some(tick2_id));
    }

    #[test]
    fn test_next_tick_number_empty() {
        let dir = TestDir::new("loopr-intg-next1");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores, test_config());
        assert_eq!(agent.next_tick_number().unwrap(), 1);
    }

    #[test]
    fn test_next_tick_number_with_existing() {
        let dir = TestDir::new("loopr-intg-next2");
        let stores = test_stores(&dir);

        let tick = Tick::new(5);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores, test_config());
        assert_eq!(agent.next_tick_number().unwrap(), 6);
    }

    #[test]
    fn test_has_tick_in_progress_false() {
        let dir = TestDir::new("loopr-intg-tip1");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores.clone(), test_config());
        assert!(!agent.has_tick_in_progress().unwrap());

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
        assert!(!agent.has_tick_in_progress().unwrap());
    }

    #[test]
    fn test_has_tick_in_progress_true() {
        let dir = TestDir::new("loopr-intg-tip2");
        let stores = test_stores(&dir);

        let tick = Tick::new(1);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores, test_config());
        assert!(agent.has_tick_in_progress().unwrap());
    }

    // --- recover_stuck_ticks tests ---

    #[test]
    fn test_recover_stuck_ticks_none() {
        let dir = TestDir::new("loopr-intg-recov1");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores, test_config());
        assert_eq!(agent.recover_stuck_ticks().unwrap(), 0);
    }

    #[test]
    fn test_recover_stuck_ticks_sealing() {
        let dir = TestDir::new("loopr-intg-recov2");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        assert_eq!(agent.recover_stuck_ticks().unwrap(), 1);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_recover_stuck_ticks_validating() {
        let dir = TestDir::new("loopr-intg-recov3");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(2);
        tick.status = TickStatus::Validating;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        assert_eq!(agent.recover_stuck_ticks().unwrap(), 1);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    // --- run_cycle tests ---

    #[test]
    fn test_cycle_idle_no_bundles() {
        let dir = TestDir::new("loopr-intg-cycle1");
        let stores = test_stores(&dir);
        let agent = test_integrator(&dir, stores, test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::Idle);
    }

    #[test]
    fn test_cycle_recovers_open_tick() {
        let dir = TestDir::new("loopr-intg-cycle2");
        let stores = test_stores(&dir);

        let tick = Tick::new(1);
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::Recovered { count: 1 });

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    #[test]
    fn test_cycle_recovers_stuck_tick() {
        let dir = TestDir::new("loopr-intg-cycle3");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores, test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::Recovered { count: 1 });
    }

    #[test]
    fn test_cycle_publishes_tick() {
        let dir = TestDir::new("loopr-intg-cycle4");
        let stores = test_stores(&dir);

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Merged);
    }

    #[test]
    fn test_cycle_validation_failure() {
        let dir = TestDir::new("loopr-intg-cycle5");
        let stores = test_stores(&dir);

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), failing_config());
        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::ValidationFailed { .. }),
            "expected ValidationFailed, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);
    }

    #[test]
    fn test_cycle_stale_bundle_rejected() {
        let dir = TestDir::new("loopr-intg-cycle6");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("wrong-tick-id".into()),
            "feature/x".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks.len(), 1);
        assert_eq!(ticks[&tick_id].status, TickStatus::Published);
    }

    #[test]
    fn test_cycle_mixed_stale_and_valid() {
        let dir = TestDir::new("loopr-intg-cycle7");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        let published_tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let mut valid_bundle = Bundle::new(
            "wi-1".into(),
            Some(published_tick_id.clone()),
            "feature/valid".into(),
            vec!["claims".into()],
        );
        valid_bundle.status = BundleStatus::Accepted;
        let valid_id = valid_bundle.id.clone();
        stores
            .bundles
            .write()
            .unwrap()
            .insert(valid_bundle.id.clone(), valid_bundle);

        let mut stale_bundle = Bundle::new(
            "wi-2".into(),
            Some("old-tick-id".into()),
            "feature/stale".into(),
            vec!["claims".into()],
        );
        stale_bundle.status = BundleStatus::Accepted;
        let stale_id = stale_bundle.id.clone();
        stores
            .bundles
            .write()
            .unwrap()
            .insert(stale_bundle.id.clone(), stale_bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&valid_id].status, BundleStatus::Merged);
        assert_eq!(bundles[&stale_id].status, BundleStatus::Rejected);
    }

    fn test_stores_with_config(dir: &std::path::Path, config: Config) -> Arc<Stores> {
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    // --- has_tick_in_progress: all non-terminal states ---

    #[test]
    fn test_has_tick_in_progress_all_states() {
        let dir = TestDir::new("loopr-intg-tipall");

        let stores = test_stores(&dir);
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
        let agent = test_integrator(&dir, stores, test_config());
        assert!(agent.has_tick_in_progress().unwrap());

        let stores = test_stores(&dir);
        let mut tick = Tick::new(2);
        tick.status = TickStatus::Validating;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
        let agent = test_integrator(&dir, stores, test_config());
        assert!(agent.has_tick_in_progress().unwrap());

        let stores = test_stores(&dir);
        let mut tick = Tick::new(3);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
        let agent = test_integrator(&dir, stores, test_config());
        assert!(!agent.has_tick_in_progress().unwrap());

        let stores = test_stores(&dir);
        let mut tick = Tick::new(4);
        tick.status = TickStatus::Failed;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
        let agent = test_integrator(&dir, stores, test_config());
        assert!(!agent.has_tick_in_progress().unwrap());
    }

    // --- Stale policy tests ---

    #[test]
    fn test_stale_policy_replan_at_safe_point() {
        let dir = TestDir::new("loopr-intg-replan");

        let mut config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        config.strategy.stale_policy = crate::config::StalePolicy::ReplanAtSafePoint;
        let stores = test_stores_with_config(&dir, config);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("wrong-id".into()),
            "feature/x".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let (agent, event_tx) = test_integrator_with_stores_config(&dir, stores.clone(), test_config());
        let mut event_rx = event_tx.subscribe();
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);

        let mut found_replan = false;
        while let Ok(event) = event_rx.try_recv() {
            if event.event == "bundle.stale_replan_needed" {
                found_replan = true;
                break;
            }
        }
        assert!(found_replan, "expected bundle.stale_replan_needed event");
    }

    #[test]
    fn test_stale_rejection_resets_work_to_ready() {
        let dir = TestDir::new("loopr-intg-stale-reset");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Create a Work in InReview status (with acceptance_criteria for Ready precondition)
        let mut wi = Work::new("ph-1".into(), "Task A".into(), "desc".into());
        wi.status = WorkStatus::InReview;
        wi.acceptance_criteria = vec!["tests pass".into()];
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Create a stale bundle referencing the work
        let mut bundle = Bundle::new(
            wi_id.clone(),
            Some("wrong-tick-id".into()),
            "feature/x".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        // Work should be reset to Ready
        let works = stores.works.read().unwrap();
        assert_eq!(
            works[&wi_id].status,
            WorkStatus::Ready,
            "Work should be reset to Ready after stale bundle rejection"
        );
    }

    #[test]
    fn test_stale_rejection_creates_learning() {
        let dir = TestDir::new("loopr-intg-stale-learn");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let mut wi = Work::new("ph-1".into(), "Task B".into(), "desc".into());
        wi.status = WorkStatus::InReview;
        wi.acceptance_criteria = vec!["tests pass".into()];
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut bundle = Bundle::new(
            wi_id.clone(),
            Some("wrong-tick-id".into()),
            "feature/y".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let _ = agent.run_cycle().unwrap();

        // A learning should be created about the rejection
        let learnings = stores.learnings.read().unwrap();
        assert!(
            learnings
                .values()
                .any(|l| l.content.contains("Bundle rejected") && l.content.contains("stale")),
            "expected a learning about bundle rejection, found: {:?}",
            learnings.values().map(|l| &l.content).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_stale_rejection_handles_terminal_work() {
        let dir = TestDir::new("loopr-intg-stale-term");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        // Create a Work that's already Done (terminal)
        let mut wi = Work::new("ph-1".into(), "Task C".into(), "desc".into());
        wi.status = WorkStatus::Done;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut bundle = Bundle::new(
            wi_id.clone(),
            Some("wrong-tick-id".into()),
            "feature/z".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        // Should not panic even if work transition fails
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        // Work stays in Done (terminal state, transition should fail gracefully)
        let works = stores.works.read().unwrap();
        assert_eq!(works[&wi_id].status, WorkStatus::Done);
    }

    #[test]
    fn test_stale_policy_auto_replay_and_verify() {
        let dir = TestDir::new("loopr-intg-replay");

        let mut config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        config.strategy.stale_policy = crate::config::StalePolicy::AutoReplayAndVerify;
        let stores = test_stores_with_config(&dir, config);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Published;
        let published_tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let mut bundle = Bundle::new(
            "wi-1".into(),
            Some("wrong-id".into()),
            "feature/x".into(),
            vec!["claims".into()],
        );
        bundle.status = BundleStatus::Accepted;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].status, BundleStatus::Rejected);
        drop(bundles);

        let mut valid_bundle = Bundle::new(
            "wi-2".into(),
            Some(published_tick_id.clone()),
            "feature/valid".into(),
            vec!["claims".into()],
        );
        valid_bundle.status = BundleStatus::Accepted;
        let valid_id = valid_bundle.id.clone();
        stores
            .bundles
            .write()
            .unwrap()
            .insert(valid_bundle.id.clone(), valid_bundle);

        stores.bundles.write().unwrap().get_mut(&bundle_id).unwrap().status = BundleStatus::Accepted;

        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&valid_id].status, BundleStatus::Merged);
    }

    // --- recover_stuck_ticks learning creation ---

    #[test]
    fn test_recover_stuck_ticks_learning_creation() {
        let dir = TestDir::new("loopr-intg-recovlearn");
        let stores = test_stores(&dir);

        let mut tick = Tick::new(1);
        tick.status = TickStatus::Validating;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let recovered = agent.recover_stuck_ticks().unwrap();
        assert_eq!(recovered, 1);

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);

        let learnings = stores.learnings.read().unwrap();
        assert!(
            learnings
                .values()
                .any(|l| l.content.contains(&tick_id) && l.content.contains("stuck")),
            "expected a learning about stuck tick recovery, found: {:?}",
            learnings.values().map(|l| &l.content).collect::<Vec<_>>()
        );
    }

    // --- Tick creation and sealing error handling ---

    #[test]
    fn test_cycle_tick_creation_error_handling() {
        let dir = TestDir::new("loopr-intg-tcreate");
        let stores = test_stores(&dir);
        let config = IntegratorConfig {
            validation_commands: vec!["echo stderr_msg >&2; false".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        };

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores, config);
        let result = agent.run_cycle().unwrap();
        match result {
            IntegratorCycleResult::ValidationFailed { log, .. } => {
                assert!(log.contains("stderr_msg"), "log should contain stderr: {}", log);
                assert!(log.contains("FAILED"), "log should contain FAILED: {}", log);
            }
            other => panic!("expected ValidationFailed, got {:?}", other),
        }
    }

    #[test]
    fn test_cycle_bundle_sealing_error_handling() {
        let dir = TestDir::new("loopr-intg-bseal");
        let stores = test_stores(&dir);

        let mut b1 = Bundle::new("wi-1".into(), None, "feature/a".into(), vec!["claims".into()]);
        b1.status = BundleStatus::Accepted;
        let b1_id = b1.id.clone();
        stores.bundles.write().unwrap().insert(b1.id.clone(), b1);

        let agent = test_integrator(&dir, stores.clone(), test_config());
        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&b1_id].status, BundleStatus::Merged);
    }

    // --- Validation with multiple commands ---

    #[test]
    fn test_cycle_validation_multi_command_sequence() {
        let dir = TestDir::new("loopr-intg-multi");
        let stores = test_stores(&dir);

        let config = IntegratorConfig {
            validation_commands: vec!["echo step1".to_string(), "echo step2".to_string(), "false".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        };

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), config);
        let result = agent.run_cycle().unwrap();
        match result {
            IntegratorCycleResult::ValidationFailed { log, .. } => {
                assert!(log.contains("step1"), "should have step1 output");
                assert!(log.contains("step2"), "should have step2 output");
                assert!(log.contains("PASSED"), "first commands should PASS");
                assert!(log.contains("FAILED"), "third command should FAIL");
            }
            other => panic!("expected ValidationFailed, got {:?}", other),
        }

        let config_pass = IntegratorConfig {
            validation_commands: vec![
                "echo check1".to_string(),
                "echo check2".to_string(),
                "echo check3".to_string(),
            ],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        };

        let mut bundle2 = Bundle::new("wi-2".into(), None, "feature/y".into(), vec!["claims".into()]);
        bundle2.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle2.id.clone(), bundle2);

        // Need a new agent with the pass config
        let agent = test_integrator(&dir, stores, config_pass);
        let result = agent.run_cycle().unwrap();
        assert!(
            matches!(result, IntegratorCycleResult::Published { .. }),
            "expected Published, got {:?}",
            result
        );
    }

    // --- Tick publish creates learning on failure ---

    #[test]
    fn test_cycle_tick_publish_learning_creation() {
        let dir = TestDir::new("loopr-intg-publearn");
        let stores = test_stores(&dir);

        let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Accepted;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent = test_integrator(&dir, stores.clone(), failing_config());
        let result = agent.run_cycle().unwrap();
        let tick_id = match &result {
            IntegratorCycleResult::ValidationFailed { tick_id, .. } => tick_id.clone(),
            other => panic!("expected ValidationFailed, got {:?}", other),
        };

        let learnings = stores.learnings.read().unwrap();
        assert!(
            learnings
                .values()
                .any(|l| l.content.contains(&tick_id) && l.content.contains("validation failed")),
            "expected a learning about validation failure for tick {}, found: {:?}",
            tick_id,
            learnings.values().map(|l| &l.content).collect::<Vec<_>>()
        );
    }

    // --- Agent::run() async loop tests ---

    #[tokio::test]
    async fn test_run_integrator_cancellation() {
        let dir = TestDir::new("loopr-intg-cancel");
        let stores = test_stores(&dir);
        let config = IntegratorConfig {
            validation_commands: vec!["true".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: None,
        };

        let mut agent = test_integrator(&dir, stores.clone(), config);

        // Pre-cancel the agent's own session so run() exits immediately
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&agent.ctx.session.id) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        }

        let result = agent.run().await;
        assert!(result.is_ok(), "cancelled integrator should return Ok: {:?}", result);
    }

    #[tokio::test]
    async fn test_run_integrator_timeout() {
        let dir = TestDir::new("loopr-intg-timeout");
        let stores = test_stores(&dir);
        let config = IntegratorConfig {
            validation_commands: vec!["true".to_string()],
            interval_secs: 1,
            enabled: true,
            session_timeout_secs: Some(1),
        };

        let mut agent = test_integrator(&dir, stores.clone(), config);
        let sid = agent.ctx.session.id.clone();

        let stores_clone = stores.clone();
        let cancel_handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let mut sessions = stores_clone.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&sid) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        });

        let result = agent.run().await;
        assert!(
            result.is_ok(),
            "integrator should exit cleanly on cancellation: {:?}",
            result
        );
        cancel_handle.await.unwrap();
    }

    // --- run_validation_commands tests ---

    #[test]
    fn test_validation_commands_pass() {
        let (passed, log) = run_validation_commands(&["true".to_string()]);
        assert!(passed);
        assert!(log.contains("PASSED"));
    }

    #[test]
    fn test_validation_commands_fail() {
        let (passed, log) = run_validation_commands(&["false".to_string()]);
        assert!(!passed);
        assert!(log.contains("FAILED"));
    }

    #[test]
    fn test_validation_commands_empty() {
        let (passed, log) = run_validation_commands(&[]);
        assert!(passed);
        assert!(log.is_empty());
    }

    #[test]
    fn test_validation_commands_multiple_pass() {
        let (passed, _) = run_validation_commands(&["true".to_string(), "true".to_string()]);
        assert!(passed);
    }

    #[test]
    fn test_validation_commands_first_fails() {
        let (passed, log) = run_validation_commands(&["false".to_string(), "true".to_string()]);
        assert!(!passed);
        assert!(log.contains("FAILED"));
    }

    // --- Fix #3: merge_bundle_branches cleanup tests ---

    #[test]
    fn test_merge_bundle_branches_success() {
        fn git(dir: &Path, args: &[&str]) -> std::process::Output {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }

        let dir = TestDir::new("loopr-intg-merge-ok");

        // Initialize git repo with initial commit
        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@test.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("main.txt"), "main").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);

        // Record the default branch name
        let out = git(&dir, &["branch", "--show-current"]);
        let default_branch = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // Create a feature branch with a commit
        git(&dir, &["checkout", "-b", "feature-1"]);
        std::fs::write(dir.join("feature.txt"), "feature").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "feature"]);
        git(&dir, &["checkout", &default_branch]);

        let result = merge_bundle_branches(&dir, &["feature-1".to_string()]);
        assert!(result.is_ok(), "merge should succeed: {:?}", result);
    }

    #[test]
    fn test_merge_bundle_branches_failure_cleans_up() {
        fn git(dir: &Path, args: &[&str]) -> std::process::Output {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }

        let dir = TestDir::new("loopr-intg-merge-abort");

        // Initialize git repo
        git(&dir, &["init"]);
        git(&dir, &["config", "user.email", "test@test.com"]);
        git(&dir, &["config", "user.name", "Test"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("conflict.txt"), "main-content").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "initial"]);

        // Record the default branch name (main or master)
        let out = git(&dir, &["branch", "--show-current"]);
        let default_branch = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // Create a feature branch with conflicting content
        git(&dir, &["checkout", "-b", "conflict-branch"]);
        std::fs::write(dir.join("conflict.txt"), "branch-content").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "branch change"]);

        // Go back to the default branch and make a conflicting change
        git(&dir, &["checkout", &default_branch]);
        std::fs::write(dir.join("conflict.txt"), "main-different-content").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "main diverge"]);

        // Merge should fail due to conflict
        let result = merge_bundle_branches(&dir, &["conflict-branch".to_string()]);
        assert!(result.is_err(), "merge should fail with conflict");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("aborted"), "error should mention aborted: {}", err);

        // Verify repo is NOT in a half-merged state (no .git/MERGE_HEAD)
        assert!(
            !dir.join(".git/MERGE_HEAD").exists(),
            "MERGE_HEAD should not exist after cleanup"
        );
    }
}
