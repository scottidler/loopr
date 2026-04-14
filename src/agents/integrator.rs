//! Deterministic Integrator task — no LLM.
//!
//! The Integrator automates the Tick lifecycle: find Accepted Bundles, create a Tick,
//! seal it, run validation commands, then publish or fail. Every decision is an
//! if/then/else on data from the stores — no prompts, no parsing, no temperature.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Duration;

use eyre::{Result, eyre};
use tracing::error;

use crate::agents::{Agent, AgentContext, AgentKind};
use crate::config::IntegratorConfig;
use crate::daemon::context::Stores;
use crate::domain::bundle::BundleStatus;
use crate::domain::tick::TickStatus;
use crate::ipc::protocol::{
    DaemonEvent, REASON_MERGE_NOT_ANCESTOR, REASON_MISSING_BRANCH, REASON_SHA_MISSING, REASON_SHA_UNREACHABLE,
};

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

    fn agent_type(&self) -> AgentKind {
        AgentKind::Integrator
    }
}

impl IntegratorAgent {
    /// Find the latest published Tick number from the stores.
    fn latest_published_tick_id(&self) -> Option<String> {
        self.ctx.debug("latest_published_tick_id()");
        let ticks = self.ctx.stores.read_ticks().ok()?;
        ticks
            .values()
            .filter(|t| t.status() == TickStatus::Published)
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
                t.status(),
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
                    t.status() == TickStatus::Open
                        || t.status() == TickStatus::Sealing
                        || t.status() == TickStatus::Validating
                })
                .map(|t| t.id.clone())
                .collect()
        };

        let mut recovered = 0;
        for tick_id in &stuck_tick_ids {
            let current_status = {
                let ticks = self.ctx.stores.read_ticks()?;
                ticks.get(tick_id.as_str()).map(|t| t.status())
            };

            let Some(status) = current_status else {
                continue;
            };

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
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    self.ctx.error(&format!(
                        "failed to recover stuck tick {} (Open->Failed): {}",
                        tick_id, msg
                    ));
                    continue;
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
                        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                        self.ctx.error(&format!(
                            "failed to recover stuck tick {} (Sealing->Validating): {}",
                            tick_id, msg
                        ));
                        continue;
                    }
                }

                let resp = self.ctx.bridge.request(
                    "tick.transition",
                    serde_json::json!({
                        "id": tick_id,
                        "target_status": "Failed",
                        "role": "integrator",
                    }),
                );
                if resp.is_error() {
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    self.ctx
                        .error(&format!("failed to recover stuck tick {} (->Failed): {}", tick_id, msg));
                    continue;
                }
            }

            // If we reach here, all transitions succeeded (failures use continue above)
            self.ctx.info(&format!("recovered stuck tick {} -> Failed", tick_id));
            recovered += 1;

            let learn_resp = self.ctx.bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": format!("Tick {} was stuck after crash, recovered to Failed", tick_id),
                    "scope": "global",
                    "source_id": tick_id,
                }),
            );
            if learn_resp.is_error() {
                self.ctx
                    .warn(&format!("failed to create recovery learning for tick {}", tick_id));
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
        // 0. Git state audit — detects divergence between DB and git history
        self.audit_git_state();
        if self.ctx.stores.degraded.load(Ordering::Relaxed) {
            self.ctx
                .warn("Integrator skipping Tick creation: system is DEGRADED (git state fracture detected)");
            return Ok(IntegratorCycleResult::Idle);
        }

        // 1. Recover stuck ticks from crash
        let recovered = self.recover_stuck_ticks()?;
        if recovered > 0 {
            return Ok(IntegratorCycleResult::Recovered { count: recovered });
        }

        // 2. Check for in-progress ticks — don't create a new one if one exists
        if self.has_tick_in_progress()? {
            return Ok(IntegratorCycleResult::Idle);
        }

        // 3. Find all Accepted bundles, partition by plan_id, pick one plan per cycle.
        let (cycle_plan_id, accepted_bundles): (Option<String>, Vec<(String, Option<String>)>) = {
            let bundles = self.ctx.stores.read_bundles()?;
            let mut by_plan: std::collections::BTreeMap<String, Vec<(String, Option<String>)>> =
                std::collections::BTreeMap::new();

            for b in bundles.values().filter(|b| b.status() == BundleStatus::Accepted) {
                let pid = resolve_plan_id(&self.ctx.stores, &b.work_id).unwrap_or_default();
                by_plan
                    .entry(pid)
                    .or_default()
                    .push((b.id.clone(), b.base_tick_id.clone()));
            }

            // Pick one plan's bundles to process per cycle; remaining plans get subsequent cycles.
            if let Some((pid, plan_bundles)) = by_plan.into_iter().next() {
                let resolved = if pid.is_empty() { None } else { Some(pid) };
                (resolved, plan_bundles)
            } else {
                (None, Vec::new())
            }
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
                        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                        return Err(eyre!("failed to reject stale bundle {}: {}", stale_id, msg));
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
                        if let Ok(_guard) = self.ctx.stores.lock_git() {
                            self.rebase_agent_branch(&wi_id);
                        }
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
                    if resp.is_error() {
                        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                        return Err(eyre!("failed to reject stale bundle {} (replan): {}", stale_id, msg));
                    } else {
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
                        if let Ok(_guard) = self.ctx.stores.lock_git() {
                            self.rebase_agent_branch(&wi_id);
                        }
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
                        let rej_resp = self.ctx.bridge.request(
                            "bundle.transition",
                            serde_json::json!({"id": stale_id, "target_status": "Rejected", "role": "integrator"}),
                        );
                        if rej_resp.is_error() {
                            let msg = rej_resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Err(eyre!(
                                "failed to reject bundle {} after auto-replay failure: {}",
                                stale_id,
                                msg
                            ));
                        }
                        self.reset_work_after_bundle_rejection(&wi_id, "auto-replay failed");
                        if let Ok(_guard) = self.ctx.stores.lock_git() {
                            self.rebase_agent_branch(&wi_id);
                        }
                        self.ctx
                            .warn(&format!("auto-replay failed for bundle {}, rejected", stale_id));
                    } else {
                        let upd_resp = self.ctx.bridge.request(
                            "bundle.update",
                            serde_json::json!({"id": stale_id, "base_tick_id": latest_tick_id}),
                        );
                        if upd_resp.is_error() {
                            let msg = upd_resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Err(eyre!(
                                "failed to update stale bundle {} base_tick_id: {}",
                                stale_id,
                                msg
                            ));
                        }
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

        // plan_id was resolved during partition (step 3) and threaded here.
        let plan_id = cycle_plan_id;

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

        // Update tick with bundle IDs, attempted bundle IDs, and plan_id.
        let tick_to_persist = {
            let mut ticks = self.ctx.stores.write_ticks()?;
            if let Some(tick) = ticks.get_mut(&tick_id) {
                tick.bundle_ids = valid_bundle_ids.clone();
                tick.attempted_bundle_ids = valid_bundle_ids.clone();
                if let Some(ref pid) = plan_id {
                    tick.plan_id = pid.clone();
                }
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
            let noop_count = valid_bundle_ids
                .iter()
                .filter(|id| bundles.get(id.as_str()).is_some_and(|b| b.noop_reason.is_some()))
                .count();
            if noop_count > 0 {
                self.ctx
                    .info(&format!("Skipping merge for {} noop bundle(s)", noop_count));
            }
            valid_bundle_ids
                .iter()
                .filter_map(|id| bundles.get(id.as_str()))
                .filter(|b| !b.branch_name.is_empty())
                .map(|b| b.branch_name.clone())
                .collect()
        };
        let repo_path = &self.ctx.stores.config.project.repo_path;
        let is_git_repo = repo_path.join(".git").exists();

        let integ_branch = plan_id.as_ref().map(|pid| integration_branch_name(pid));

        // Record pre-merge SHA for validation rollback (set after branch checkout).
        let mut pre_merge_sha: Option<String> = None;

        if is_git_repo {
            let _git_guard = self.ctx.stores.lock_git()?;

            // Always checkout integration branch (needed for validation + SHA).
            if let Some(ref branch) = integ_branch {
                let verify = std::process::Command::new("git")
                    .args(["rev-parse", "--verify", branch])
                    .current_dir(repo_path)
                    .output();
                if let Ok(o) = verify
                    && o.status.success()
                {
                    let checkout = std::process::Command::new("git")
                        .args(["checkout", branch])
                        .current_dir(repo_path)
                        .output();
                    if let Ok(co) = checkout
                        && !co.status.success()
                    {
                        let stderr = String::from_utf8_lossy(&co.stderr);
                        return Err(eyre!("failed to checkout integration branch {}: {}", branch, stderr));
                    }
                } else {
                    return Err(eyre!(
                        "integration branch {} does not exist (deleted or not yet created)",
                        branch
                    ));
                }
            }
            // Record pre-merge SHA for validation rollback.
            pre_merge_sha = get_git_head_sha(repo_path);

            if !branches.is_empty() {
                match merge_bundle_branches(repo_path, &branches) {
                    Ok(sha) => {
                        let mut ticks = self.ctx.stores.write_ticks()?;
                        if let Some(tick) = ticks.get_mut(&tick_id) {
                            tick.integration_sha = Some(sha);
                        }
                    }
                    Err(e) => {
                        self.ctx.warn(&format!("merge failed: {}", e));

                        // Reset integration branch to pre-tick HEAD, rolling back any
                        // partial merges from earlier bundles in this tick.
                        if let Some(ref sha) = pre_merge_sha {
                            // Abort any in-progress merge first.
                            let _ = std::process::Command::new("git")
                                .args(["merge", "--abort"])
                                .current_dir(repo_path)
                                .output();
                            let reset = std::process::Command::new("git")
                                .args(["reset", "--hard", sha])
                                .current_dir(repo_path)
                                .output();
                            match reset {
                                Ok(o) if o.status.success() => {
                                    self.ctx
                                        .info(&format!("reset integration branch to pre-tick HEAD {}", sha));
                                }
                                _ => {
                                    self.ctx.warn(&format!("failed to reset integration branch to {}", sha));
                                }
                            }
                        }

                        let fail_resp = self.ctx.bridge.request(
                            "tick.transition",
                            serde_json::json!({
                                "id": tick_id,
                                "target_status": "Failed",
                                "role": "integrator",
                            }),
                        );
                        if fail_resp.is_error() {
                            let msg = fail_resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                            return Err(eyre!(
                                "failed to transition tick {} to Failed after merge failure: {}",
                                tick_id,
                                msg
                            ));
                        }

                        // Classify the conflict before rejecting bundles (while we still have context).
                        let conflict_kind = classify_conflict(&self.ctx.stores, &valid_bundle_ids);

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
                                let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                                return Err(eyre!(
                                    "failed to reject bundle {} after merge failure: {}",
                                    bundle_id,
                                    msg
                                ));
                            }
                        }

                        // Handle work reset based on conflict type.
                        match conflict_kind {
                            Some((conflicting_files, conflicting_work_ids)) => {
                                self.ctx.warn(&format!(
                                    "structural merge conflict detected: works={:?} files={:?}",
                                    conflicting_work_ids, conflicting_files
                                ));
                                self.combine_conflicting_works(
                                    &conflicting_work_ids,
                                    &conflicting_files,
                                    &valid_bundle_ids,
                                );
                            }
                            None => {
                                // Retryable conflict - reset all works to Ready.
                                // Collect work IDs first, drop the read guard, then iterate.
                                // This avoids holding a RwLockReadGuard across IPC calls.
                                let work_ids: Vec<String> = match self.ctx.stores.read_bundles() {
                                    Ok(bundles) => valid_bundle_ids
                                        .iter()
                                        .filter_map(|bid| bundles.get(bid.as_str()).map(|b| b.work_id.clone()))
                                        .collect(),
                                    Err(_) => Vec::new(),
                                };
                                for wi_id in &work_ids {
                                    self.reset_work_after_bundle_rejection(wi_id, "merge conflict");
                                    // git lock already held by outer _git_guard - no re-acquisition
                                    self.rebase_agent_branch(wi_id);
                                }
                            }
                        }

                        return Ok(IntegratorCycleResult::ValidationFailed {
                            tick_id,
                            log: format!("Merge failed: {}", e),
                        });
                    }
                }
            } else {
                // Noop tick: no merge needed, but record integration branch HEAD
                self.ctx
                    .info("All bundles are noop - skipping merge, recording HEAD SHA");
                let sha = get_git_head_sha(repo_path);
                let tick_to_persist = {
                    let mut ticks = self.ctx.stores.write_ticks()?;
                    if let Some(tick) = ticks.get_mut(&tick_id) {
                        tick.integration_sha = sha;
                        Some(tick.clone())
                    } else {
                        None
                    }
                };
                // Persist immediately so crash between here and publish doesn't lose SHA
                if let Some(tick) = tick_to_persist
                    && let Some(ref store) = self.ctx.stores.store
                    && let Ok(mut s) = store.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                {
                    let _ = s.update(tick);
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
        let effective_cmds =
            effective_validation_commands(&self.config.validation_commands, &valid_bundle_ids, &self.ctx.stores);
        let (passed, validation_log) = run_validation_commands(&effective_cmds);
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
            // Defensive: ensure integration_sha is set before publish transition.
            // Normal ticks already have it from merge. Noop ticks have it from
            // checkout block above. This catches any remaining gap.
            //
            // IMPORTANT: Fetch SHA outside the write lock to avoid blocking the
            // daemon's state access with a synchronous subprocess spawn.
            let missing_sha = {
                let ticks = self.ctx.stores.read_ticks()?;
                ticks.get(&tick_id).is_some_and(|t| t.integration_sha.is_none())
            };

            if missing_sha {
                let sha = get_git_head_sha(repo_path).unwrap_or_else(|| "unknown".to_string());
                self.ctx.warn(&format!(
                    "integration_sha was None before publish - setting to {} (defensive)",
                    sha
                ));
                let mut ticks = self.ctx.stores.write_ticks()?;
                if let Some(tick) = ticks.get_mut(&tick_id) {
                    tick.integration_sha = Some(sha);
                }
            }

            // Pre-publish state check: verify tick is still Validating before attempting publish.
            // If recover_stuck_ticks or a racing call already moved it to Failed, skip publish
            // gracefully instead of triggering a hard FSM error (Failed -> Published).
            {
                let tick_state = self
                    .ctx
                    .stores
                    .read_ticks()
                    .ok()
                    .and_then(|ticks| ticks.get(&tick_id).map(|t| t.status()));
                if tick_state != Some(TickStatus::Validating) {
                    self.ctx.warn(&format!(
                        "Tick {} not in Validating state before publish (was {:?}), skipping publish",
                        tick_id, tick_state
                    ));
                    return Ok(IntegratorCycleResult::ValidationFailed {
                        tick_id: tick_id.clone(),
                        log: format!(
                            "Tick {} not in Validating state before publish (was {:?})",
                            tick_id, tick_state
                        ),
                    });
                }
            }

            // NOW transition to Published - the handler persists the tick with SHA populated
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

            // SHA was set before publish - verify invariant holds
            debug_assert!(
                self.ctx
                    .stores
                    .read_ticks()
                    .is_ok_and(|ts| ts.get(&tick_id).is_some_and(|t| t.integration_sha.is_some())),
                "integration_sha should be set after publish"
            );

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
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    self.ctx.error(&format!(
                        "CRITICAL: bundle {} merged in git but failed to transition to Merged: {}",
                        bundle_id, msg
                    ));
                    let _ = self.ctx.bridge.request(
                        "learning.create",
                        serde_json::json!({
                            "content": format!("Bundle {} merged in git but FSM transition to Merged failed: {}. Coordinator must reconcile.", bundle_id, msg),
                            "scope": format!("bundle/{}", bundle_id),
                            "source_id": bundle_id,
                        }),
                    );
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
                        .map(|w| w.status() == crate::domain::work::WorkStatus::InReview)
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
                        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                        self.ctx.error(&format!(
                            "CRITICAL: work {} has merged bundle but failed to transition to Integrated: {}",
                            wi_id, msg
                        ));
                        let _ = self.ctx.bridge.request(
                            "learning.create",
                            serde_json::json!({
                                "content": format!("Work {} has merged bundle but failed to transition to Integrated: {}. Coordinator must reconcile.", wi_id, msg),
                                "scope": format!("work/{}", wi_id),
                                "source_id": wi_id,
                            }),
                        );
                    } else {
                        self.ctx.info(&format!("Work {} transitioned to Integrated", wi_id));
                    }
                }
            }

            // Emit bundle.merged event so implementers can rebase.
            let integration_sha = get_git_head_sha(repo_path).unwrap_or_default();
            let _ = self.ctx.bridge.event_tx().send(DaemonEvent::bundle_merged(
                &tick_id,
                &integration_sha,
                &valid_bundle_ids,
            ));

            self.ctx.info(&format!("Tick {} published successfully", tick_id));
            Ok(IntegratorCycleResult::Published {
                tick_id: tick_id.clone(),
            })
        } else {
            // Validation failed: rollback integration branch to pre-merge state.
            // This keeps the integration branch in a known-good state at all times.
            if let Some(ref sha) = pre_merge_sha {
                let _git_guard = self.ctx.stores.lock_git().ok();
                let reset = std::process::Command::new("git")
                    .args(["reset", "--hard", sha])
                    .current_dir(repo_path)
                    .output();
                match reset {
                    Ok(o) if o.status.success() => {
                        self.ctx.info(&format!(
                            "Rolled back integration branch to pre-merge SHA {} after validation failure",
                            sha
                        ));
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        self.ctx
                            .error(&format!("Failed to rollback integration branch to {}: {}", sha, stderr));
                    }
                    Err(e) => {
                        self.ctx
                            .error(&format!("Failed to execute git reset --hard {}: {}", sha, e));
                    }
                }
            }

            let fail_resp = self.ctx.bridge.request(
                "tick.transition",
                serde_json::json!({
                    "id": tick_id,
                    "target_status": "Failed",
                    "role": "integrator",
                }),
            );
            if fail_resp.is_error() {
                let msg = fail_resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                return Err(eyre!("failed to transition tick {} to Failed: {}", tick_id, msg));
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
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Err(eyre!(
                        "failed to reject bundle {} after validation failure: {}",
                        bundle_id,
                        msg
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
                    if let Ok(_guard) = self.ctx.stores.lock_git() {
                        self.rebase_agent_branch(&wi_id);
                    }
                }
            }

            let learn_resp = self.ctx.bridge.request(
                "learning.create",
                serde_json::json!({
                    "content": format!("Tick {} validation failed: {}", tick_id, validation_log),
                    "scope": "global",
                    "source_id": tick_id,
                }),
            );
            if learn_resp.is_error() {
                self.ctx.warn(&format!(
                    "failed to create validation failure learning for tick {}",
                    tick_id
                ));
            }

            self.ctx.info(&format!("Tick {} validation failed", tick_id));
            Ok(IntegratorCycleResult::ValidationFailed {
                tick_id: tick_id.clone(),
                log: validation_log,
            })
        }
    }

    /// Git state audit: verifies DB state matches git history. Sets degraded flag on catastrophic
    /// fractures so run_cycle() can skip Tick creation. Safe to call every cycle — fast for
    /// small repos and limits SHA reachability checks to recent bundles.
    fn audit_git_state(&self) {
        let repo_path = &self.ctx.stores.config.project.repo_path;
        if !repo_path.join(".git").exists() {
            return;
        }
        let mut catastrophic = false;
        self.audit_branches(repo_path, &mut catastrophic);
        self.audit_tick_shas(repo_path, &mut catastrophic);
        self.audit_merge_ancestry(repo_path, &mut catastrophic);
        if catastrophic {
            self.ctx.stores.degraded.store(true, Ordering::Relaxed);
            error!("Integrator entering DEGRADED mode: catastrophic git state fracture detected");
        }
    }

    /// Check that every non-terminal Bundle still has its agent branch in git.
    /// Recoverable: missing branch -> force Bundle to Rejected.
    /// Noop bundles (with noop_reason set) are skipped - they have no agent branch.
    fn audit_branches(&self, repo_path: &std::path::Path, catastrophic: &mut bool) {
        let _ = catastrophic; // branch audit is recoverable only
        let bundles: Vec<(String, String, String)> = {
            match self.ctx.stores.read_bundles() {
                Ok(bs) => bs
                    .values()
                    .filter(|b| {
                        !matches!(
                            b.status(),
                            BundleStatus::Merged | BundleStatus::Rejected | BundleStatus::Superseded
                        )
                    })
                    // Skip noop bundles - they have no agent branch by design
                    .filter(|b| b.noop_reason.is_none())
                    .map(|b| (b.id.clone(), b.work_id.clone(), format!("{:?}", b.status())))
                    .collect(),
                Err(_) => return,
            }
        };
        for (bundle_id, work_id, from_status) in &bundles {
            let branch = format!("agent/{}", work_id);
            let exists = std::process::Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", branch)])
                .current_dir(repo_path)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !exists {
                self.ctx.warn(&format!(
                    "Reconciliation: Bundle {} branch {} missing (status={})",
                    bundle_id, branch, from_status
                ));
                if let Ok(mut bundles_w) = self.ctx.stores.write_bundles()
                    && let Some(bundle) = bundles_w.get_mut(bundle_id.as_str())
                {
                    bundle.force_status(BundleStatus::Rejected);
                    bundle.updated_at = crate::id::now_millis();
                    if let Some(store_arc) = self.ctx.stores.store.as_ref()
                        && let Ok(mut s) = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))
                        && let Err(e) = s.update(bundle.clone())
                    {
                        self.ctx.warn(&format!(
                            "audit_branches: failed to persist bundle {}: {}",
                            bundle_id, e
                        ));
                    }
                    let _ = self.ctx.event_tx.send(DaemonEvent::reconciled(
                        "bundle",
                        bundle_id,
                        from_status,
                        "Rejected",
                        REASON_MISSING_BRANCH,
                    ));
                }
            }
        }
    }

    /// Verify Published Tick integration SHAs are reachable from HEAD.
    /// Catastrophic: SHA unreachable or missing → enter degraded mode.
    fn audit_tick_shas(&self, repo_path: &std::path::Path, catastrophic: &mut bool) {
        let published: Vec<(String, Option<String>)> = {
            match self.ctx.stores.read_ticks() {
                Ok(ticks) => ticks
                    .values()
                    .filter(|t| t.status() == TickStatus::Published)
                    .map(|t| (t.id.clone(), t.integration_sha.clone()))
                    .collect(),
                Err(_) => return,
            }
        };
        for (tick_id, sha_opt) in &published {
            match sha_opt {
                None => {
                    error!(
                        "Reconciliation CATASTROPHIC: Tick {} Published with no integration_sha",
                        tick_id
                    );
                    let _ = self.ctx.event_tx.send(DaemonEvent::reconciliation_failed(
                        "tick",
                        tick_id,
                        "Published",
                        REASON_SHA_MISSING,
                    ));
                    *catastrophic = true;
                }
                Some(sha) => {
                    let reachable = std::process::Command::new("git")
                        .args(["merge-base", "--is-ancestor", sha, "HEAD"])
                        .current_dir(repo_path)
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                    if !reachable {
                        error!(
                            "Reconciliation CATASTROPHIC: Tick {} integration_sha {} unreachable from HEAD",
                            tick_id, sha
                        );
                        let _ = self.ctx.event_tx.send(DaemonEvent::reconciliation_failed(
                            "tick",
                            tick_id,
                            "Published",
                            REASON_SHA_UNREACHABLE,
                        ));
                        *catastrophic = true;
                    }
                }
            }
        }
    }

    /// Verify recently Merged bundles: head_commit must be ancestor of the Tick's integration_sha.
    /// Catastrophic: ancestry broken → enter degraded mode.
    /// Limited to last 100 merged bundles within 30 days to avoid full-history scans.
    fn audit_merge_ancestry(&self, repo_path: &std::path::Path, catastrophic: &mut bool) {
        let cutoff_ms = crate::id::now_millis() - (30 * 24 * 60 * 60 * 1000_i64);
        let recent_merged: Vec<(String, String)> = {
            match self.ctx.stores.read_bundles() {
                Ok(bs) => {
                    let mut v: Vec<(String, String, i64)> = bs
                        .values()
                        .filter(|b| {
                            b.status() == BundleStatus::Merged && b.updated_at >= cutoff_ms && b.head_commit.is_some()
                        })
                        .map(|b| (b.id.clone(), b.head_commit.clone().unwrap_or_default(), b.updated_at))
                        .collect();
                    v.sort_by(|a, b| b.2.cmp(&a.2)); // most recent first
                    v.truncate(100);
                    v.into_iter().map(|(id, hc, _)| (id, hc)).collect()
                }
                Err(_) => return,
            }
        };
        for (bundle_id, head_commit) in &recent_merged {
            let tick_sha: Option<String> = self.ctx.stores.read_ticks().ok().and_then(|ticks| {
                ticks
                    .values()
                    .find(|t| t.status() == TickStatus::Published && t.bundle_ids.contains(bundle_id))
                    .and_then(|t| t.integration_sha.clone())
            });
            let Some(tick_sha) = tick_sha else { continue };
            let is_ancestor = std::process::Command::new("git")
                .args(["merge-base", "--is-ancestor", head_commit, &tick_sha])
                .current_dir(repo_path)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !is_ancestor {
                error!(
                    "Reconciliation CATASTROPHIC: Bundle {} head_commit {} not ancestor of Tick integration_sha {}",
                    bundle_id, head_commit, tick_sha
                );
                let _ = self.ctx.event_tx.send(DaemonEvent::reconciliation_failed(
                    "bundle",
                    bundle_id,
                    "Merged",
                    REASON_MERGE_NOT_ANCESTOR,
                ));
                *catastrophic = true;
            }
        }
    }

    /// Combine conflicting works into a single new Work after a structural merge conflict.
    ///
    /// Instead of escalating to the coordinator (which can't reliably formulate
    /// override_work + create_work payloads), the integrator mechanically combines
    /// the conflicting works into one. A fresh implementer writes the file coherently
    /// in one pass.
    ///
    /// Steps:
    /// 1. Read conflicting Work items
    /// 2. Create ONE combined Work (title, AC union, deps union minus self-refs)
    /// 3. Rewire same-Phase sibling dependencies pointing at originals
    /// 4. Abandon original conflicting Works
    /// 5. Reset non-conflicting works from the same tick to Ready
    /// 6. Create Learning documenting the resolution
    fn combine_conflicting_works(
        &self,
        conflicting_work_ids: &HashSet<String>,
        conflicting_files: &HashSet<String>,
        all_bundle_ids: &[String],
    ) {
        // 1. Read all conflicting Work items.
        struct WorkSnapshot {
            title: String,
            parent_id: String,
            ac: Vec<String>,
            deps: Vec<String>,
        }
        let works_data: Vec<WorkSnapshot> = {
            let Ok(works) = self.ctx.stores.read_works() else {
                self.ctx.warn("combine_conflicting_works: cannot read works");
                return;
            };
            let mut sorted_ids: Vec<&String> = conflicting_work_ids.iter().collect();
            sorted_ids.sort();
            sorted_ids
                .iter()
                .filter_map(|wid| {
                    works.get(wid.as_str()).map(|w| WorkSnapshot {
                        title: w.title.clone(),
                        parent_id: w.parent_id.clone(),
                        ac: w.acceptance_criteria.0.clone(),
                        deps: w.dependencies.clone(),
                    })
                })
                .collect()
        };

        if works_data.is_empty() {
            self.ctx.warn("combine_conflicting_works: no conflicting works found");
            return;
        }

        let parent_id = works_data[0].parent_id.clone();

        // 2. Build combined Work fields.
        let combined_title = works_data
            .iter()
            .map(|w| w.title.as_str())
            .collect::<Vec<_>>()
            .join(" + ");

        // Union of all AC lists, cap at 20.
        let mut combined_ac: Vec<String> = Vec::new();
        for w in &works_data {
            for item in &w.ac {
                if !combined_ac.contains(item) {
                    combined_ac.push(item.clone());
                }
            }
        }
        if combined_ac.len() > 20 {
            combined_ac.truncate(20);
        }

        // Union of all deps, MINUS the IDs being combined (prevent self-ref cycles).
        let mut combined_deps: Vec<String> = Vec::new();
        for w in &works_data {
            for dep in &w.deps {
                if !conflicting_work_ids.contains(dep) && !combined_deps.contains(dep) {
                    combined_deps.push(dep.clone());
                }
            }
        }

        // 3. Create the combined Work via IPC.
        let create_resp = self.ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "parent_id": parent_id,
                "title": combined_title,
                "acceptance_criteria": combined_ac,
                "dependencies": combined_deps,
            }),
        );

        let new_work_id = if let Some(id) = create_resp
            .result
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
        {
            id.to_string()
        } else {
            self.ctx.warn(&format!(
                "combine_conflicting_works: failed to create combined work: {:?}",
                create_resp.error
            ));
            return;
        };

        self.ctx.info(&format!(
            "combine_conflicting_works: created {} from {:?}",
            new_work_id, conflicting_work_ids
        ));

        // 4. Rewire dependencies: for each Work in the same Phase that depends on
        //    any original, replace the dep with the new combined Work ID.
        {
            let sibling_ids: Vec<String> = match self.ctx.stores.read_works() {
                Err(_) => {
                    self.ctx
                        .warn("combine_conflicting_works: cannot read works for dep rewiring");
                    Vec::new()
                }
                Ok(works) => works
                    .values()
                    .filter(|w| {
                        w.parent_id == parent_id
                            && !conflicting_work_ids.contains(&w.id)
                            && w.id != new_work_id
                            && w.dependencies.iter().any(|d| conflicting_work_ids.contains(d))
                    })
                    .map(|w| w.id.clone())
                    .collect(),
            };

            for sibling_id in sibling_ids {
                let new_deps: Vec<String> = {
                    let Ok(works) = self.ctx.stores.read_works() else {
                        continue;
                    };
                    works
                        .get(sibling_id.as_str())
                        .map(|w| {
                            w.dependencies
                                .iter()
                                .map(
                                    |d| {
                                        if conflicting_work_ids.contains(d) { new_work_id.clone() } else { d.clone() }
                                    },
                                )
                                .collect()
                        })
                        .unwrap_or_default()
                };

                let resp = self.ctx.bridge.request(
                    "work.update",
                    serde_json::json!({
                        "id": sibling_id,
                        "dependencies": new_deps,
                    }),
                );
                if resp.is_error() {
                    self.ctx.warn(&format!(
                        "combine_conflicting_works: failed to rewire deps for {}: {:?}",
                        sibling_id, resp.error
                    ));
                } else {
                    self.ctx.info(&format!(
                        "combine_conflicting_works: rewired deps for {} -> {}",
                        sibling_id, new_work_id
                    ));
                }
            }
        }

        // 5. Abandon all original conflicting Works.
        for wi_id in conflicting_work_ids {
            let resp = self.ctx.bridge.request(
                "work.transition",
                serde_json::json!({
                    "id": wi_id,
                    "target_status": "Abandoned",
                    "role": "coordinator",
                }),
            );
            if resp.is_error() {
                self.ctx.warn(&format!(
                    "combine_conflicting_works: failed to abandon {}: {:?}",
                    wi_id, resp.error
                ));
            } else {
                self.ctx.info(&format!(
                    "combine_conflicting_works: abandoned {} (combined into {})",
                    wi_id, new_work_id
                ));
            }
        }

        // Reset non-conflicting works from the same tick to Ready for normal retry.
        // Collect work IDs first, drop the read guard, then iterate to avoid
        // holding a RwLockReadGuard across IPC calls.
        let unrelated_work_ids: Vec<String> = match self.ctx.stores.read_bundles() {
            Ok(bundles) => all_bundle_ids
                .iter()
                .filter_map(|bid| bundles.get(bid.as_str()))
                .filter(|b| !conflicting_work_ids.contains(&b.work_id))
                .map(|b| b.work_id.clone())
                .collect(),
            Err(_) => Vec::new(),
        };
        for wi_id in &unrelated_work_ids {
            self.reset_work_after_bundle_rejection(wi_id, "merge conflict (unrelated)");
            // git lock already held by caller's _git_guard in process_tick
            self.rebase_agent_branch(wi_id);
        }

        // 6. Create Learning documenting the resolution.
        let mut files_sorted: Vec<&String> = conflicting_files.iter().collect();
        files_sorted.sort();
        let mut works_sorted: Vec<&String> = conflicting_work_ids.iter().collect();
        works_sorted.sort();
        let content = format!(
            "STRUCTURAL CONFLICT RESOLVED: {} combined into {} due to overlapping paths: [{}]",
            works_sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
            new_work_id,
            files_sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
        );
        let learn_resp = self.ctx.bridge.request(
            "learning.create",
            serde_json::json!({
                "content": content,
                "scope": "phase",
                "source_id": parent_id,
            }),
        );
        if learn_resp.is_error() {
            self.ctx.warn(&format!(
                "combine_conflicting_works: failed to create learning: {:?}",
                learn_resp.error
            ));
        } else {
            self.ctx
                .info("combine_conflicting_works: created STRUCTURAL CONFLICT RESOLVED learning");
        }
    }

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
        let learn_resp = self.ctx.bridge.request(
            "learning.create",
            serde_json::json!({
                "content": format!("Bundle rejected ({}). Work reset to Ready for retry with updated main branch.", reason),
                "scope": "phase",
                "source_id": work_id,
            }),
        );
        if learn_resp.is_error() {
            self.ctx.warn(&format!(
                "failed to create bundle rejection learning for work {}",
                work_id
            ));
        }
    }

    /// Rebase the agent branch for `work_id` onto the current integration HEAD.
    /// If the rebase fails (conflict), delete the branch so the next session starts clean.
    ///
    /// **The caller must hold (or explicitly acquire) the git lock before calling this.**
    /// This function performs git operations but does NOT call `lock_git()` itself,
    /// to avoid re-entrancy deadlocks when called from within a `_git_guard` scope.
    fn rebase_agent_branch(&self, work_id: &str) {
        let branch = format!("agent/{}", work_id);
        let repo_path = &self.ctx.stores.config.project.repo_path;

        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/heads/{}", branch)])
            .current_dir(repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if !branch_exists {
            return;
        }

        let Some(plan_id) = resolve_plan_id(&self.ctx.stores, work_id) else {
            self.ctx.warn(&format!(
                "rebase_agent_branch: cannot resolve plan_id for {} - skipping rebase",
                work_id
            ));
            return;
        };

        let integ_ref = format!("integration/{}", plan_id);

        let rebase_ok = std::process::Command::new("git")
            .args(["rebase", &integ_ref, &branch])
            .current_dir(repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if rebase_ok {
            self.ctx
                .info(&format!("Rebased {} onto {} after bundle rejection", branch, integ_ref));
        } else {
            // Abort the failed rebase, then delete the branch
            let _ = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(repo_path)
                .output();
            let _ = std::process::Command::new("git")
                .args(["branch", "-D", &branch])
                .current_dir(repo_path)
                .output();
            self.ctx.warn(&format!(
                "Rebase of {} failed (conflict) - deleted branch; next session starts clean",
                branch
            ));
        }

        // Rebase (or abort) leaves the agent branch checked out. Restore
        // the integration branch so worktree creation works for the next session.
        let _ = std::process::Command::new("git")
            .args(["checkout", &integ_ref])
            .current_dir(repo_path)
            .output();
    }
}

/// Traverse the parent chain from a Work to discover its Plan ID.
///
/// Bundle -> Work -> Phase -> Spec -> Plan. Returns `None` if any link is broken.
fn resolve_plan_id(stores: &Stores, work_id: &str) -> Option<String> {
    let works = stores.read_works().ok()?;
    let work = works.get(work_id)?;
    let phases = stores.read_phases().ok()?;
    let phase = phases.get(work.parent_id.as_str())?;
    let specs = stores.read_specs().ok()?;
    let spec = specs.get(phase.parent_id.as_str())?;
    Some(spec.parent_id.clone()) // Plan ID
}

/// Derive the integration branch name from a plan ID.
fn integration_branch_name(plan_id: &str) -> String {
    format!("integration/{}", plan_id)
}

/// Classify whether a merge failure is structural (multiple bundles in the same tick
/// touched the same file) or retryable.
///
/// Returns `Some((conflicting_files, conflicting_work_ids))` for structural conflicts,
/// or `None` when no file overlap is detected (treat as retryable).
///
/// Uses `bundle.paths` (actual git diff output) rather than the old `work.files`
/// (LLM-predicted, often empty or inaccurate).
fn classify_conflict(stores: &Stores, bundle_ids: &[String]) -> Option<(HashSet<String>, HashSet<String>)> {
    let bundles = stores.read_bundles().ok()?;

    // Map file → first work_id that touched it.
    let mut file_to_work: HashMap<String, String> = HashMap::new();
    let mut conflicting_files: HashSet<String> = HashSet::new();
    let mut conflicting_works: HashSet<String> = HashSet::new();

    for bid in bundle_ids {
        let Some(bundle) = bundles.get(bid.as_str()) else {
            continue;
        };
        for file in &bundle.paths {
            if let Some(first_work) = file_to_work.get(file) {
                conflicting_files.insert(file.clone());
                conflicting_works.insert(first_work.clone());
                conflicting_works.insert(bundle.work_id.clone());
            } else {
                file_to_work.insert(file.clone(), bundle.work_id.clone());
            }
        }
    }

    if conflicting_files.is_empty() {
        None
    } else {
        Some((conflicting_files, conflicting_works))
    }
}

/// Merge bundle branches into the current HEAD.
///
/// The caller is responsible for checking out the target branch (integration branch
/// or main) before calling this function. Returns the HEAD SHA after all merges succeed.
fn merge_bundle_branches(repo_path: &std::path::Path, bundle_branches: &[String]) -> Result<String> {
    tracing::debug!(
        "merge_bundle_branches(repo={}, branches={:?})",
        repo_path.display(),
        bundle_branches,
    );
    for branch in bundle_branches {
        // Verify the branch has commits beyond the merge base.
        // Without this, `git merge --no-ff` on a branch pointing to the same
        // commit as HEAD silently succeeds (exit 0, "Already up to date")
        // with no merge commit - a no-op that passes but produces no changes.
        let merge_base = std::process::Command::new("git")
            .args(["merge-base", "HEAD", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git merge-base HEAD {} failed: {}", branch, e))?;

        let branch_head = std::process::Command::new("git")
            .args(["rev-parse", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| eyre!("git rev-parse {} failed: {}", branch, e))?;

        if merge_base.status.success() && branch_head.status.success() {
            let base_sha = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();
            let head_sha = String::from_utf8_lossy(&branch_head.stdout).trim().to_string();
            if base_sha == head_sha {
                return Err(eyre!(
                    "branch {} has no commits beyond merge base (both at {}). \
                     The implementer's commits may have been lost.",
                    branch,
                    base_sha
                ));
            }
        }

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

/// Resolve effective validation commands: global + phase-scoped (deduplicated).
/// Returns the effective set of validation commands to run for a set of bundles.
/// Phase-level validation_commands were removed in domain-model-cleanup Phase 3;
/// only global (IntegratorConfig) commands remain.
pub fn effective_validation_commands(
    global_commands: &[String],
    _bundle_ids: &[String],
    _stores: &Stores,
) -> Vec<String> {
    global_commands.to_vec()
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

// Phase 4 cutover: integrator tests disabled pending engine integration
// #[allow(clippy::unwrap_used)]
// #[cfg(test)]
// mod tests;
