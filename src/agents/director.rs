//! The Director agent - top-level thinking agent that bridges the user to the system.
//!
//! Four operating modes (state machine):
//! - PlanIntake: interviews the user, shapes goals into Plans with AC (Phase 3 wires the conversation)
//! - Monitoring: long-lived observer over a plan's lifetime (this file, Phase 2+)
//! - Escalation: diagnoses and acts when mechanical recovery fails (Phase 5 rewrites the one-shot v1 path)
//! - UserIntervention: translates user chat into plan modifications during execution (Phase 6)
//!
//! The Director does NOT schedule agents or drive the mechanical loop - that's the engine's job.
//! It provides judgment for cases the engine can't handle.
//!
//! ## Stores access policy
//!
//! Domain truth (Spec / Phase / Work / Bundle state used for reconciliation or decisions)
//! MUST be read via `AgentIpcBridge` so the Director sees the same filtered view as every
//! other actor. See `reconcile_from_ipc` and `bridge_list`.
//!
//! Direct `self.ctx.stores.*` access in this file is strictly permitted only for
//! *Agent Metadata and Chat Context* (`AgentSession`, `ChatHistory`):
//!   - persisting the Director's current mode on its own `AgentSession` (`persist_mode`)
//!   - resolving session <-> work / chat-session ids
//!     (`resolve_work_id_for_session`, `resolve_chat_session_id`, `planintake_turn`)
//!
//! Routing these through IPC would mean the daemon talking to itself through a socket to
//! read global `AgentSession` / `ChatHistory` metadata - not domain state. If you add a
//! new Stores access, it must fit the metadata bucket above. Anything touching
//! domain records (`Plan` / `Spec` / `Phase` / `Work` / `Bundle`) goes through the bridge
//! (see `is_plan_terminal` for the `plan.get` pattern, `reconcile_from_ipc` for list-style
//! queries).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use eyre::Result;
use futures::future::OptionFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, trace, warn};

use crate::agents::director::actions::{execute_actions, parse_actions};
use crate::agents::implementer::LlmClient;
use crate::agents::lifeguard::Lifeguard;
use crate::agents::llm_client::AgentLlmClient;
use crate::agents::{Agent, AgentContext, AgentEvent, AgentKind};
use crate::config::AgentRoleConfig;
use crate::domain::chat::ChatHistory;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::agentic_loop::run_tool_loop;
use crate::tools::context::ToolContext;
use crate::tools::types::{ContentBlock, Message};

pub mod actions;

/// Heartbeat cadence for the Director run loop.
///
/// Heartbeat is the poll rate for two level-triggered checks: session cancellation and
/// plan-terminal. In steady state the loop is event-driven; heartbeat exists solely so
/// the loop remains responsive to shutdown even when the broadcast stream is quiet.
/// 1 second is cheap (tokio::time::sleep is a microscopic allocation) and responsive
/// enough for user-visible cancel latency.
const HEARTBEAT_INTERVAL_MS: u64 = 1_000;

// Phase 7 thresholds are now configurable via `AgentConfig::director_thresholds`
// (see `src/config.rs::DirectorThresholds`). The Director reads them from
// `self.ctx.stores.config.agents.director_thresholds` at each use site so runtime config
// reloads take effect without restarting the Director. Historical hardcoded constants
// (STALL_THRESHOLD_SECS, WORK_FAILURE_THRESHOLD, WORK_REJECTION_THRESHOLD) were removed
// in the Phase 7 wiring pass - do not reintroduce them.

/// The Director's operating mode.
///
/// Persisted on `AgentSession.director_mode` for observability.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DirectorMode {
    /// Interview the user to shape a goal into a Plan.
    PlanIntake,
    /// Long-lived observer: event-driven broadcast loop over a plan's lifetime.
    Monitoring,
    /// Diagnose and act on a failure escalation.
    Escalation,
    /// Translate a user chat message into plan modifications during execution.
    UserIntervention,
}

/// In-memory cross-session pattern tracker.
///
/// Populated two ways:
/// 1. Event-driven: `process_event` updates counters on relevant broadcast events.
/// 2. IPC reconciliation: `reconcile_from_ipc` rebuilds from persistent `Work`/`Bundle`/`Spec`
///    state (Phase 7 fills this in; Phase 2 provides the hook).
///
/// The tracker is a derived read cache - not a source of truth. Every counter it holds
/// maps to persisted state: `Work.session_failure_count`, `Bundle` rejection records,
/// `Spec.revision_count`. On `RecvError::Lagged` the tracker is flushed and rebuilt
/// from IPC - dropped events are recovered from ground truth. See the design doc's
/// `State Reconciliation` section.
#[derive(Debug, Default)]
pub struct DirectorPatternTracker {
    /// Work IDs that have failed across multiple implementer sessions.
    /// Key: work_id, Value: Vec<(session_id, failure_reason)>.
    pub work_failure_history: HashMap<String, Vec<(String, String)>>,
    /// Bundle rejection reasons grouped by work_id.
    pub rejection_history: HashMap<String, Vec<String>>,
    /// Number of times each spec has been revised via bubble-up.
    pub spec_revision_count: HashMap<String, u32>,
    /// Spec id -> set of work ids observed as Abandoned under that spec. Feeds the
    /// spec-level abandonment-ratio escalation (design doc Phase 7 "abandon-ratio-
    /// escalation" pattern). Using a set means a work reported twice (via event + IPC
    /// reconciliation) counts once.
    pub spec_abandoned_works: HashMap<String, HashSet<String>>,
    /// Spec id -> set of work ids observed at all under that spec (any status). The
    /// denominator for the abandonment ratio.
    pub spec_total_works: HashMap<String, HashSet<String>>,
}

impl DirectorPatternTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all state - used before reconciliation rebuilds.
    pub fn clear(&mut self) {
        self.work_failure_history.clear();
        self.rejection_history.clear();
        self.spec_revision_count.clear();
        self.spec_abandoned_works.clear();
        self.spec_total_works.clear();
    }

    /// Total failure count across all sessions for a given work (any root cause). Use
    /// together with `unique_signature_count` - the raw count catches environmental
    /// flapping, the signature count catches a stuck root cause.
    pub fn failure_count(&self, work_id: &str) -> usize {
        self.work_failure_history.get(work_id).map(Vec::len).unwrap_or(0)
    }

    /// Total rejection count across all bundles for a given work (any reviewer-feedback
    /// theme). Use together with `unique_theme_count`.
    pub fn rejection_count(&self, work_id: &str) -> usize {
        self.rejection_history.get(work_id).map(Vec::len).unwrap_or(0)
    }

    /// Count of distinct error signatures observed for a given work. Multiple sessions
    /// that failed with the same signature are counted once - Phase 7 uses this to
    /// distinguish "same root cause, retried" (1 signature) from "different failures
    /// each time" (N signatures). The latter suggests the work itself is sound and the
    /// symptoms are environmental; the former suggests a true stuck state for Escalation.
    pub fn unique_signature_count(&self, work_id: &str) -> usize {
        let Some(history) = self.work_failure_history.get(work_id) else {
            return 0;
        };
        let sigs: HashSet<String> = history.iter().map(|(_, err)| error_signature(err)).collect();
        sigs.len()
    }

    /// Count of distinct rejection themes observed for a given work's bundles. Themes
    /// are derived from reviewer feedback by `rejection_theme`. Semantics parallel
    /// `unique_signature_count`: N themes means N different kinds of complaint; 1 theme
    /// means the same complaint keeps coming back (likely a real defect the implementer
    /// isn't addressing).
    pub fn unique_theme_count(&self, work_id: &str) -> usize {
        let Some(history) = self.rejection_history.get(work_id) else {
            return 0;
        };
        let themes: HashSet<String> = history.iter().map(|r| rejection_theme(r)).collect();
        themes.len()
    }

    /// Record that `work_id` under `spec_id` was abandoned. Both sets are used by
    /// `abandonment_ratio`. Idempotent - replaying the same abandonment event does not
    /// double-count.
    pub fn observe_abandonment(&mut self, spec_id: &str, work_id: &str) {
        self.spec_total_works
            .entry(spec_id.to_string())
            .or_default()
            .insert(work_id.to_string());
        self.spec_abandoned_works
            .entry(spec_id.to_string())
            .or_default()
            .insert(work_id.to_string());
    }

    /// Record that `work_id` exists under `spec_id` regardless of its current status.
    /// Call this whenever a work event surfaces a spec link, so the denominator of the
    /// abandonment ratio reflects total observed works (not just the abandoned ones).
    pub fn observe_spec_work(&mut self, spec_id: &str, work_id: &str) {
        self.spec_total_works
            .entry(spec_id.to_string())
            .or_default()
            .insert(work_id.to_string());
    }

    /// Fraction of a spec's observed works that are in the abandoned set. Returns
    /// `(ratio, sample_size)`. `sample_size` is the total-works count; callers should
    /// require `sample_size >= min_sample` before treating the ratio as actionable, so
    /// a 2-work spec with 1 abandonment does not read as 50% and escalate prematurely.
    pub fn abandonment_ratio(&self, spec_id: &str) -> (f64, usize) {
        let total = self.spec_total_works.get(spec_id).map(HashSet::len).unwrap_or(0);
        if total == 0 {
            return (0.0, 0);
        }
        let abandoned = self.spec_abandoned_works.get(spec_id).map(HashSet::len).unwrap_or(0);
        (abandoned as f64 / total as f64, total)
    }
}

/// Stable signature for an error message. Phase 7 Lifeguard uses this to group failures
/// by root cause across sessions: LLM outputs vary in whitespace and trailing tokens, so
/// we normalize aggressively before hashing.
///
/// Strategy: lowercase, collapse whitespace, strip the first line after the first colon
/// (which usually isolates the error kind from the stack trace), and take the leading
/// 200 chars. This is deliberately cheap and heuristic - perfect grouping would need a
/// semantic classifier; Phase 7 just needs something better than raw-string equality.
pub fn error_signature(err: &str) -> String {
    let trimmed = err.trim().to_lowercase();
    // If the message has "key: detail" shape, keep the key portion for grouping.
    let base = trimmed.split_once(':').map(|(k, _)| k).unwrap_or(&trimmed);
    let collapsed: String = base.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(200).collect()
}

/// Coarse theme for reviewer rejection feedback. Used to bucket distinct reviewer
/// complaints so the Director can tell "same complaint 4 times" (escalate - implementer
/// isn't addressing it) from "4 different complaints" (implementer is making progress
/// each round, just imperfectly).
///
/// Strategy mirrors `error_signature`: lowercase, collapse whitespace, and take the
/// leading 64 characters of the first-line / first-sentence. Deliberately cheap and
/// heuristic - a future refinement could use embedding similarity, but length-prefix is
/// enough for the threshold decision today.
pub fn rejection_theme(reason: &str) -> String {
    let trimmed = reason.trim().to_lowercase();
    // First sentence or first line, whichever is shorter - reviewers tend to lead with
    // the headline, then elaborate.
    let first = trimmed.split(['.', '\n']).next().unwrap_or(&trimmed);
    let collapsed: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(64).collect()
}

/// The Director agent.
pub struct DirectorAgent<L: LlmClient> {
    pub ctx: AgentContext,
    llm: L,
    config: AgentRoleConfig,
    mode: DirectorMode,
    /// Plan id being monitored. Set when `doc.plan_accepted` arrives or when the session
    /// is spawned with a plan-scoped `target_id` (e.g. supervision restart mid-plan).
    plan_id: Option<String>,
    /// Chat session whose history seeds the PlanIntake conversation. Resolved via reverse
    /// lookup on `ChatHistory.director_session_id == self.ctx.session.id` when PlanIntake
    /// begins.
    chat_session_id: Option<String>,
    pattern_tracker: DirectorPatternTracker,
    /// Within-session Lifeguard. Cross-session detection is Phase 7 via `pattern_tracker`.
    lifeguard: Lifeguard,
    /// Monotonic clock reference used to detect stalls in Monitoring mode.
    last_event_at: Instant,
    /// Last time a `director.stall_detected` event was emitted. Used to throttle
    /// repeat emissions when the stall persists across heartbeats.
    last_stall_emit_at: Option<Instant>,
    /// Total events observed across the session (debug/trace only).
    event_count: u64,
    /// When `check_thresholds` flips to Escalation mid-loop, it records the work_id
    /// that triggered the flip here. The next iteration of `event_loop` notices the
    /// Escalation mode, consumes this target, runs `enter_escalation`, then returns
    /// to Monitoring. `None` when no escalation is pending.
    pending_escalation_target: Option<String>,
    /// Lazily-initialized PlanIntake LLM client. Created on first conversation turn so we
    /// don't spin one up for Directors that never reach PlanIntake (supervision restarts
    /// into Monitoring, legacy Escalation). The Director's generic `llm: L` satisfies the
    /// `LlmClient` trait used by `legacy_escalation`; run_tool_loop needs `AgenticLlm`
    /// which is why we use the concrete `AgentLlmClient` here.
    planintake_llm: Option<Arc<AgentLlmClient>>,
}

impl<L: LlmClient> DirectorAgent<L> {
    pub fn new(ctx: AgentContext, llm: L, config: AgentRoleConfig) -> Self {
        Self {
            ctx,
            llm,
            config,
            mode: DirectorMode::PlanIntake,
            plan_id: None,
            chat_session_id: None,
            pattern_tracker: DirectorPatternTracker::new(),
            lifeguard: Lifeguard::new(),
            last_event_at: Instant::now(),
            last_stall_emit_at: None,
            event_count: 0,
            pending_escalation_target: None,
            planintake_llm: None,
        }
    }

    /// Decide the initial mode from session state.
    ///
    /// - No `target_id`: fresh chat-to-director handoff → PlanIntake (Phase 3 conversation loop).
    /// - `target_id` starts with `pl-`: monitoring an existing plan (supervision restart or
    ///   post-acceptance transition) → Monitoring.
    /// - `target_id` otherwise: legacy escalation spawn (engine primitives still invoke this
    ///   path pre-Phase 5) → Escalation (one-shot LLM call, preserves v1 behavior).
    fn determine_initial_mode(&mut self) {
        match self.ctx.session.target_id.as_deref() {
            None => self.mode = DirectorMode::PlanIntake,
            Some(tid) if tid.starts_with("pl-") => {
                self.plan_id = Some(tid.to_string());
                self.mode = DirectorMode::Monitoring;
            }
            Some(_) => self.mode = DirectorMode::Escalation,
        }
        self.persist_mode();
    }

    /// Write the current `mode` back to the session store for observability.
    fn persist_mode(&self) {
        let Ok(mut sessions) = self.ctx.stores.write_agent_sessions() else {
            warn!(
                "{} director: sessions lock poisoned; mode not persisted",
                self.ctx.log_prefix()
            );
            return;
        };
        if let Some(s) = sessions.get_mut(&self.ctx.session.id) {
            s.director_mode = Some(self.mode);
        }
    }

    /// Serialize `DirectorMode` to the kebab-case string used on the wire.
    /// Keep this in sync with the `#[serde(rename_all = "kebab-case")]` on the enum.
    fn mode_str(mode: DirectorMode) -> &'static str {
        match mode {
            DirectorMode::PlanIntake => "plan-intake",
            DirectorMode::Monitoring => "monitoring",
            DirectorMode::Escalation => "escalation",
            DirectorMode::UserIntervention => "user-intervention",
        }
    }

    /// Transition to `new_mode`: update in-memory mode, persist to session store,
    /// and broadcast `director.mode_changed` so the TUI and audit trail see the flip.
    /// No-op when the mode is unchanged, so callers don't need to pre-check.
    fn set_mode(&mut self, new_mode: DirectorMode) {
        if self.mode == new_mode {
            return;
        }
        let from = self.mode;
        self.mode = new_mode;
        self.persist_mode();
        info!(
            "{} director: mode {:?} -> {:?} (plan_id={:?})",
            self.ctx.log_prefix(),
            from,
            new_mode,
            self.plan_id
        );
        let _ = self.ctx.event_tx.send(DaemonEvent::director_mode_changed(
            &self.ctx.session.id,
            Self::mode_str(new_mode),
            self.plan_id.as_deref(),
        ));
    }

    /// Cross-session threshold check. Flips to Escalation when the tracker has observed
    /// enough of the "same root cause" or "same reviewer complaint" for this work, or
    /// when raw failure / rejection tallies (any signature, any theme) climb past the
    /// total-count safety nets.
    ///
    /// Four escalation conditions, any of which trips the switch:
    ///
    /// 1. `unique_signature_count >= failure_signature_threshold` - the implementer has
    ///    failed with the same root cause that many distinct times (same-root-cause loop).
    /// 2. `failure_count       >= failure_total_threshold`        - raw failure tally,
    ///    catches environmental flapping where each failure looks different.
    /// 3. `unique_theme_count  >= rejection_theme_threshold`      - the reviewer keeps
    ///    writing the same complaint (implementer isn't addressing it).
    /// 4. `rejection_count     >= rejection_total_threshold`      - raw rejection tally,
    ///    safety net for rejections that don't theme-cluster cleanly.
    ///
    /// All thresholds are sourced from `AgentConfig::director_thresholds`.
    fn check_thresholds(&mut self, work_id: &str) {
        if !matches!(self.mode, DirectorMode::Monitoring) {
            return;
        }
        let thresholds = self.ctx.stores.config.agents.director_thresholds;

        let signature_count = self.pattern_tracker.unique_signature_count(work_id);
        let failure_total = self.pattern_tracker.failure_count(work_id);
        let theme_count = self.pattern_tracker.unique_theme_count(work_id);
        let rejection_total = self.pattern_tracker.rejection_count(work_id);

        let signatures_tripped = signature_count >= thresholds.failure_signature_threshold;
        let failures_tripped = failure_total >= thresholds.failure_total_threshold;
        let themes_tripped = theme_count >= thresholds.rejection_theme_threshold;
        let rejections_tripped = rejection_total >= thresholds.rejection_total_threshold;

        if signatures_tripped || failures_tripped || themes_tripped || rejections_tripped {
            warn!(
                "{} director: escalating work_id={} (unique-signatures={}/{} total-failures={}/{} unique-themes={}/{} total-rejections={}/{})",
                self.ctx.log_prefix(),
                work_id,
                signature_count,
                thresholds.failure_signature_threshold,
                failure_total,
                thresholds.failure_total_threshold,
                theme_count,
                thresholds.rejection_theme_threshold,
                rejection_total,
                thresholds.rejection_total_threshold,
            );
            self.pending_escalation_target = Some(work_id.to_string());
            self.set_mode(DirectorMode::Escalation);
        }
    }

    /// Cross-session spec-level threshold check. Fires when a spec has accumulated enough
    /// abandoned works (relative to its total) that the spec itself - not any individual
    /// work - warrants Director judgment.
    ///
    /// Guarded by `spec_min_works_for_ratio` so a tiny spec doesn't escalate on the first
    /// abandonment. Escalation target is the spec id (prefixed for the escalation handler
    /// to distinguish spec-level from work-level targets).
    fn check_spec_thresholds(&mut self, spec_id: &str) {
        if !matches!(self.mode, DirectorMode::Monitoring) {
            return;
        }
        let thresholds = self.ctx.stores.config.agents.director_thresholds;
        let (ratio, sample) = self.pattern_tracker.abandonment_ratio(spec_id);
        if sample < thresholds.spec_min_works_for_ratio {
            trace!(
                "{} director: spec {} abandonment sample too small ({}/{}), skipping",
                self.ctx.log_prefix(),
                spec_id,
                sample,
                thresholds.spec_min_works_for_ratio
            );
            return;
        }
        if ratio >= thresholds.spec_abandonment_ratio {
            warn!(
                "{} director: escalating spec_id={} (abandonment ratio {:.2} >= {:.2}, sample={})",
                self.ctx.log_prefix(),
                spec_id,
                ratio,
                thresholds.spec_abandonment_ratio,
                sample,
            );
            // Spec-level escalation flags the spec itself as the target. Downstream
            // escalation handler inspects the target prefix (`sp-*`) to branch on it.
            self.pending_escalation_target = Some(spec_id.to_string());
            self.set_mode(DirectorMode::Escalation);
        }
    }

    /// Query the plan's current status via the IPC bridge (`plan.get`).
    ///
    /// Returns true when the plan has reached a terminal domain status (Complete /
    /// Superseded / Abandoned), or when the plan no longer exists in the daemon (deleted
    /// out from under us - nothing left to monitor). Returns false on transient bridge
    /// failures or malformed responses, so a broken daemon socket does not silently
    /// euthanize an otherwise-live Director.
    ///
    /// The two error-path defaults are deliberately asymmetric:
    ///
    /// - `RpcError::not_found` (code `-32001`) from `plan.get` → terminal. The plan is
    ///   gone; continuing to poll would hot-loop forever.
    /// - Any other IPC error, or deserialize failure → active. Assume transient.
    fn is_plan_terminal(&self) -> bool {
        let Some(plan_id) = &self.plan_id else {
            // No plan yet (e.g., PlanIntake pre-acceptance). Not terminal - keep running.
            return false;
        };
        let resp = self
            .ctx
            .bridge
            .request("plan.get", serde_json::json!({ "id": plan_id }));
        if resp.is_error() {
            let code = resp.error.as_ref().map(|e| e.code);
            let msg = resp.error.as_ref().map(|e| e.message.as_str()).unwrap_or("");
            if code == Some(crate::ipc::protocol::RpcError::CODE_NOT_FOUND) {
                warn!(
                    "{} director: plan {} no longer exists (not_found); terminating monitoring",
                    self.ctx.log_prefix(),
                    plan_id
                );
                return true;
            }
            warn!(
                "{} director: plan.get failed for {} (code={:?}); treating plan as active to avoid dropping session ({})",
                self.ctx.log_prefix(),
                plan_id,
                code,
                msg
            );
            return false;
        }
        let Some(result) = resp.result else {
            // OK response with null payload - should not happen for plan.get in practice
            // (the handler returns not_found as an error), but guard anyway. Treat as
            // missing-and-terminal rather than active to match the not_found semantic.
            warn!(
                "{} director: plan {} returned null result; terminating monitoring",
                self.ctx.log_prefix(),
                plan_id
            );
            return true;
        };
        match serde_json::from_value::<crate::domain::plan::Plan>(result) {
            Ok(plan) => {
                use crate::domain::plan::HierarchyStatus;
                matches!(
                    plan.status(),
                    HierarchyStatus::Complete | HierarchyStatus::Superseded | HierarchyStatus::Abandoned
                )
            }
            Err(e) => {
                warn!(
                    "{} director: plan.get result deserialize failed for {} ({}); treating as active",
                    self.ctx.log_prefix(),
                    plan_id,
                    e
                );
                false
            }
        }
    }

    /// Rebuild the pattern tracker from persistent state by querying `Stores` through
    /// `AgentIpcBridge`, per design doc "State Reconciliation" section.
    ///
    /// The Director walks Plan -> Spec -> Phase -> Work -> Bundle through the bridge's
    /// `spec.list` / `phase.list` / `work.list` / `bundle.list` methods, so the same
    /// data path used by external agents is exercised here.
    ///
    /// Invoked from three places:
    /// 1. Monitoring entry (seed a freshly spawned Director with full history),
    /// 2. `RecvError::Lagged` (tracker is flushed and rebuilt after dropped events),
    /// 3. Phase 7 tests (property check: event-driven vs reconciled should match).
    async fn reconcile_from_ipc(&mut self) -> Result<()> {
        debug!(
            "{} director: reconcile_from_ipc (plan_id={:?})",
            self.ctx.log_prefix(),
            self.plan_id
        );
        self.pattern_tracker.clear();

        // Without a plan_id we don't have a scope to reconcile against (PlanIntake,
        // legacy escalation). Leave the tracker cleared and return.
        let Some(plan_id) = self.plan_id.clone() else {
            return Ok(());
        };

        // spec.list(parent_id=plan_id) -> Vec<Spec>
        let specs: Vec<crate::domain::spec::Spec> =
            self.bridge_list("spec.list", serde_json::json!({ "parent_id": plan_id }))?;

        // Record spec revision counts (decomposition_attempts is the persistent proxy for
        // the design doc's revision_count).
        for s in &specs {
            self.pattern_tracker
                .spec_revision_count
                .insert(s.id.clone(), s.decomposition_attempts);
        }

        // For each spec, phase.list(parent_id=spec_id) -> Vec<Phase>
        let mut phases: Vec<crate::domain::phase::Phase> = Vec::new();
        for s in &specs {
            let ph: Vec<crate::domain::phase::Phase> =
                self.bridge_list("phase.list", serde_json::json!({ "parent_id": s.id }))?;
            phases.extend(ph);
        }

        // For each phase, work.list(parent_id=phase_id) -> Vec<Work>
        let mut works: Vec<crate::domain::work::Work> = Vec::new();
        for p in &phases {
            let ws: Vec<crate::domain::work::Work> =
                self.bridge_list("work.list", serde_json::json!({ "parent_id": p.id }))?;
            works.extend(ws);
        }

        // Failure history: session_failure_count is the persistent aggregate of failed
        // sessions per work. Synthesize placeholder (session_id, error) entries so the
        // tracker's in-memory shape stays compatible with event-driven updates.
        //
        // Spec-level abandonment tracking (Phase 7): populate the spec totals and
        // abandoned sets so `check_spec_thresholds` has the sample size to evaluate the
        // ratio immediately after reconcile. `work.parent_id` is a phase id, so we walk
        // phase -> spec via the phases vec built above.
        let phase_to_spec: HashMap<&str, &str> = phases.iter().map(|p| (p.id.as_str(), p.parent_id.as_str())).collect();
        for w in &works {
            if w.session_failure_count > 0 {
                let history: Vec<(String, String)> = (0..w.session_failure_count as usize)
                    .map(|i| (format!("reconciled-{}-{}", w.id, i), "reconciled".to_string()))
                    .collect();
                self.pattern_tracker.work_failure_history.insert(w.id.clone(), history);
            }
            if let Some(spec_id) = phase_to_spec.get(w.parent_id.as_str()) {
                self.pattern_tracker.observe_spec_work(spec_id, &w.id);
                use crate::domain::work::WorkStatus;
                if matches!(w.status(), WorkStatus::Abandoned) {
                    self.pattern_tracker.observe_abandonment(spec_id, &w.id);
                }
            }
        }

        // Bundle rejections: bundle.list filters by work_id; aggregate across all works.
        let mut rejection_count = 0usize;
        for w in &works {
            let bundles: Vec<crate::domain::bundle::Bundle> =
                self.bridge_list("bundle.list", serde_json::json!({ "work_id": w.id }))?;
            for b in &bundles {
                if matches!(b.status(), crate::domain::bundle::BundleStatus::Rejected) {
                    self.pattern_tracker
                        .rejection_history
                        .entry(b.work_id.clone())
                        .or_default()
                        .push("rejected".to_string());
                    rejection_count += 1;
                }
            }
        }

        debug!(
            "{} director: reconcile_from_ipc complete (works_in_plan={} failures={} rejections={} spec_revisions={})",
            self.ctx.log_prefix(),
            works.len(),
            self.pattern_tracker.work_failure_history.len(),
            rejection_count,
            self.pattern_tracker.spec_revision_count.len(),
        );
        Ok(())
    }

    /// Dispatch a list-style IPC request through the bridge and deserialize the result
    /// array. Returns an empty vector if the handler replies with a null/missing result.
    fn bridge_list<T: serde::de::DeserializeOwned>(&self, method: &str, params: serde_json::Value) -> Result<Vec<T>> {
        let resp = self.ctx.bridge.request(method, params);
        if resp.is_error() {
            let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
            return Err(eyre::eyre!("{} failed: {}", method, msg));
        }
        let Some(result) = resp.result else {
            return Ok(Vec::new());
        };
        serde_json::from_value::<Vec<T>>(result).map_err(|e| eyre::eyre!("{} result deserialize failed: {}", method, e))
    }

    /// Classify and record a broadcast event. No LLM calls here - judgment lands in
    /// Phase 4 (Monitoring) and Phase 5 (Escalation).
    async fn process_event(&mut self, event: DaemonEvent) -> Result<()> {
        self.event_count += 1;
        self.last_event_at = Instant::now();

        // Work id whose threshold should be (re)checked after recording this event.
        // Set only when the event actually touched a work's failure/rejection history.
        let mut touched_work: Option<String> = None;
        // Spec id whose abandonment ratio should be (re)checked. Set when a work under
        // this spec transitions to Abandoned.
        let mut touched_spec: Option<String> = None;

        match event.event.as_str() {
            // Plan acceptance completes the PlanIntake → Monitoring handoff.
            "doc.plan_accepted" => {
                if let Some(pid) = event.data.get("plan_id").and_then(|v| v.as_str()) {
                    self.plan_id = Some(pid.to_string());
                }
                if matches!(self.mode, DirectorMode::PlanIntake) {
                    self.set_mode(DirectorMode::Monitoring);
                    // Seed the tracker from persistent state now that we have a plan scope.
                    self.reconcile_from_ipc().await?;
                }
            }
            // Track agent failures for cross-session pattern detection. Phase 4 triggers
            // Escalation when `WORK_FAILURE_THRESHOLD` is reached; Phase 7 refines with
            // error-signature hash grouping.
            "agent.status_changed" => {
                if let Some(status) = event.data.get("status").and_then(|v| v.as_str())
                    && status.eq_ignore_ascii_case("failed")
                {
                    let session_id = event
                        .data
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let error = event
                        .data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no error)")
                        .to_string();
                    if let Some(work_id) = self.resolve_work_id_for_session(&session_id) {
                        self.pattern_tracker
                            .work_failure_history
                            .entry(work_id.clone())
                            .or_default()
                            .push((session_id, error));
                        touched_work = Some(work_id);
                    }
                }
            }
            // Bundle rejections are the other primary signal for work-level failures.
            "bundle.rejected_stale" | "bundle.rejected" => {
                if let Some(work_id) = event
                    .data
                    .get("bundle_work_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| event.data.get("work_id").and_then(|v| v.as_str()))
                {
                    let reason = event
                        .data
                        .get("reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("stale")
                        .to_string();
                    let work_id = work_id.to_string();
                    self.pattern_tracker
                        .rejection_history
                        .entry(work_id.clone())
                        .or_default()
                        .push(reason);
                    touched_work = Some(work_id);
                }
            }
            // Work abandonment: update spec-level abandonment tracking so the Director
            // can escalate at the spec level when a threshold fraction of a spec's works
            // have been abandoned. Cheap: only two bridge calls, and abandonments are rare.
            "transition.completed" => {
                let collection = event.data.get("collection").and_then(|v| v.as_str());
                let target = event.data.get("to").and_then(|v| v.as_str());
                if collection == Some("work")
                    && target == Some("Abandoned")
                    && let Some(work_id) = event.data.get("id").and_then(|v| v.as_str())
                {
                    if let Some(spec_id) = self.resolve_spec_for_work(work_id) {
                        self.pattern_tracker.observe_spec_work(&spec_id, work_id);
                        self.pattern_tracker.observe_abandonment(&spec_id, work_id);
                        touched_spec = Some(spec_id);
                    } else {
                        trace!(
                            "{} director: could not resolve spec for abandoned work {}",
                            self.ctx.log_prefix(),
                            work_id
                        );
                    }
                }
            }
            _ => {
                trace!("{} director: observed event {}", self.ctx.log_prefix(), event.event);
            }
        }

        if let Some(work_id) = touched_work {
            self.check_thresholds(&work_id);
        }
        if let Some(spec_id) = touched_spec {
            self.check_spec_thresholds(&spec_id);
        }
        Ok(())
    }

    /// Walk `work.get(work_id) -> phase_id` then `phase.get(phase_id) -> spec_id` via
    /// the bridge. Returns None if either call fails or the response is not a Work/Phase
    /// with a parent id. Used by spec-level abandonment tracking in `process_event`.
    fn resolve_spec_for_work(&self, work_id: &str) -> Option<String> {
        let work_resp = self
            .ctx
            .bridge
            .request("work.get", serde_json::json!({ "id": work_id }));
        if work_resp.is_error() {
            return None;
        }
        let work: crate::domain::work::Work = serde_json::from_value(work_resp.result?).ok()?;

        let phase_resp = self
            .ctx
            .bridge
            .request("phase.get", serde_json::json!({ "id": work.parent_id }));
        if phase_resp.is_error() {
            return None;
        }
        let phase: crate::domain::phase::Phase = serde_json::from_value(phase_resp.result?).ok()?;
        Some(phase.parent_id)
    }

    /// Look up the work_id associated with an agent session by consulting Stores.
    /// Returns None for sessions that don't target a work (Director, Researcher scoped to plan, etc.).
    fn resolve_work_id_for_session(&self, session_id: &str) -> Option<String> {
        let sessions = self.ctx.stores.read_agent_sessions().ok()?;
        sessions.get(session_id).and_then(|s| s.work_id.clone())
    }

    /// Resolve the chat_session_id tied to this Director (if any) by reverse lookup
    /// on `ChatHistory.director_session_id`. Cached after the first call.
    fn resolve_chat_session_id(&mut self) -> Option<String> {
        if self.chat_session_id.is_some() {
            return self.chat_session_id.clone();
        }
        let sessions = self.ctx.stores.chat_sessions.read().ok()?;
        let mine = sessions.iter().find_map(|(sid, h)| {
            if h.director_session_id.as_deref() == Some(&self.ctx.session.id) {
                Some(sid.clone())
            } else {
                None
            }
        });
        self.chat_session_id = mine.clone();
        mine
    }

    /// Lazily create (or reuse) the Director's PlanIntake LLM client.
    fn planintake_llm_client(&mut self) -> Result<Arc<AgentLlmClient>> {
        if let Some(ref llm) = self.planintake_llm {
            return Ok(llm.clone());
        }
        let llm = AgentLlmClient::new(
            self.config.clone(),
            format!("{}:planintake", self.ctx.session.id),
            self.ctx.event_tx.clone(),
        )?;
        let llm = Arc::new(llm);
        self.planintake_llm = Some(llm.clone());
        Ok(llm)
    }

    /// Handle a user message from the `director.user_message` mpsc channel.
    ///
    /// PlanIntake: append to the chat history, run one tool-use turn, persist the
    /// assistant response, stream output via `agent.llm_output`.
    ///
    /// Monitoring: transition to UserIntervention and delegate (Phase 6 fills in the
    /// intent-translation LLM call; Phase 3 logs and immediately returns to Monitoring).
    ///
    /// Escalation: ignored (Escalation doesn't serve user messages directly).
    async fn handle_user_message(&mut self, message: String) -> Result<()> {
        debug!(
            "{} director: user message in {:?} mode ({} chars)",
            self.ctx.log_prefix(),
            self.mode,
            message.len()
        );
        match self.mode {
            DirectorMode::PlanIntake => self.planintake_turn(Some(message)).await,
            DirectorMode::Monitoring => {
                self.set_mode(DirectorMode::UserIntervention);
                let result = self.user_intervention_turn(message).await;
                self.set_mode(DirectorMode::Monitoring);
                result
            }
            DirectorMode::Escalation | DirectorMode::UserIntervention => {
                debug!(
                    "{} director: user message ignored in {:?} mode",
                    self.ctx.log_prefix(),
                    self.mode
                );
                Ok(())
            }
        }
    }

    /// Stream a text response to the TUI via `agent.llm_output` events. Uses the same
    /// event shape that Chat and Implementer emit so the TUI doesn't need Director-specific
    /// rendering.
    fn emit_llm_text(&self, text: &str) {
        let _ = self.ctx.event_tx.send(DaemonEvent::new(
            "agent.llm_output",
            serde_json::json!(AgentEvent::LlmOutput {
                session_id: self.ctx.session.id.clone(),
                chunk: text.to_string(),
                is_final: false,
            }),
        ));
        let _ = self.ctx.event_tx.send(DaemonEvent::new(
            "agent.llm_output",
            serde_json::json!(AgentEvent::LlmOutput {
                session_id: self.ctx.session.id.clone(),
                chunk: String::new(),
                is_final: true,
            }),
        ));
    }

    /// Run one PlanIntake conversation turn.
    ///
    /// If `new_message` is `Some`, append it to the chat history as a user turn.
    /// Build a messages slice from `ChatHistory.messages` and call `run_tool_loop`
    /// with the Director's interview prompt. Persist the assistant response back
    /// to the history.
    ///
    /// Phase 3 uses a minimal tool set (the default `ToolExecutor::standard`) and the
    /// interview system prompt from `prompts::store().director`. Phase 3b refinements
    /// (tailored tool lists, richer initial context, delegate subagents) land when the
    /// TUI-side /plan flow is exercised end-to-end.
    async fn planintake_turn(&mut self, new_message: Option<String>) -> Result<()> {
        let Some(chat_session_id) = self.resolve_chat_session_id() else {
            warn!(
                "{} director: PlanIntake turn requested but no chat_session_id linked; skipping",
                self.ctx.log_prefix()
            );
            return Ok(());
        };

        // Append the user message to ChatHistory (under lock) and clone the message list
        // for the LLM call. Drop the lock before the await to keep from holding the sync
        // RwLock across async points.
        let mut messages = {
            let mut chat_sessions = self
                .ctx
                .stores
                .chat_sessions
                .write()
                .map_err(|_| eyre::eyre!("chat_sessions lock poisoned"))?;
            let history = chat_sessions
                .entry(chat_session_id.clone())
                .or_insert_with(|| ChatHistory::new(chat_session_id.clone()));
            if let Some(msg) = new_message {
                history.messages.push(Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: msg }],
                });
                history.updated_at = chrono::Utc::now().timestamp_millis();
            }
            history.messages.clone()
        };
        if messages.is_empty() {
            // No user input yet - plant a minimal seed so the LLM has something to respond to.
            // The interview prompt is responsible for asking the first clarifying question.
            messages.push(Message {
                role: "user".to_string(),
                content: vec![ContentBlock::Text {
                    text: "(conversation seed: greet the user and ask what they're building)".to_string(),
                }],
            });
        }

        let system_prompt = crate::prompts::store().director.clone();
        if system_prompt.is_empty() {
            warn!(
                "{} director: no interview prompt configured; emitting stub acknowledgement",
                self.ctx.log_prefix()
            );
            self.emit_llm_text(
                "[Director prompt not configured. Configure agents/director.pmt to enable plan intake.]",
            );
            return Ok(());
        }

        let llm = self.planintake_llm_client()?;
        let executor = Arc::new(crate::tools::ToolExecutor::standard(
            &self.ctx.stores.config.agents.tools,
        ));
        let tool_ctx = ToolContext::new(
            self.ctx.stores.config.project.repo_path.clone(),
            self.ctx.session.id.clone(),
        )
        .with_sandbox(false);

        let checkpoint_stores = self.ctx.stores.clone();
        let checkpoint_sid = chat_session_id.clone();
        let checkpoint = move |msgs: &[Message]| {
            if let Ok(mut sessions) = checkpoint_stores.chat_sessions.write()
                && let Some(history) = sessions.get_mut(&checkpoint_sid)
            {
                history.messages = msgs.to_vec();
                history.updated_at = chrono::Utc::now().timestamp_millis();
            }
        };

        let max_iterations = self.config.max_iterations;
        let result = run_tool_loop(
            llm.as_ref(),
            executor.as_ref(),
            &tool_ctx,
            &system_prompt,
            messages,
            max_iterations,
            Some(&self.ctx.event_tx),
            Some(&checkpoint),
        )
        .await;

        match result {
            Ok(agentic) => {
                // Final persist - run_tool_loop already checkpointed per-iteration, but
                // this closes the turn deterministically (in case the last iteration failed
                // to run the checkpoint).
                if let Ok(mut sessions) = self.ctx.stores.chat_sessions.write()
                    && let Some(history) = sessions.get_mut(&chat_session_id)
                {
                    history.messages = agentic.messages;
                    history.updated_at = chrono::Utc::now().timestamp_millis();
                }
                let _ = self.ctx.event_tx.send(DaemonEvent::new(
                    "agent.llm_output",
                    serde_json::json!(AgentEvent::LlmOutput {
                        session_id: self.ctx.session.id.clone(),
                        chunk: String::new(),
                        is_final: true,
                    }),
                ));
            }
            Err(e) => {
                warn!("{} director: PlanIntake LLM error: {}", self.ctx.log_prefix(), e);
                self.emit_llm_text(&format!("[PlanIntake error: {}]", e));
            }
        }
        Ok(())
    }

    /// Periodic heartbeat. Detects stalls (configurable `stall_threshold_secs` during
    /// Monitoring) and provides a level-triggered backstop for terminal plan detection.
    async fn heartbeat(&mut self) -> Result<()> {
        trace!(
            "{} director: heartbeat (events={} mode={:?})",
            self.ctx.log_prefix(),
            self.event_count,
            self.mode
        );
        // Stall check only applies when Monitoring - PlanIntake is driven by user tempo,
        // Escalation is driven by LLM call duration, UserIntervention is transient.
        if matches!(self.mode, DirectorMode::Monitoring) {
            let thresholds = self.ctx.stores.config.agents.director_thresholds;
            let idle_secs = self.last_event_at.elapsed().as_secs();
            if idle_secs >= thresholds.stall_threshold_secs {
                let should_emit = match self.last_stall_emit_at {
                    None => true,
                    Some(last) => last.elapsed().as_secs() >= thresholds.stall_reemit_secs,
                };
                if should_emit {
                    warn!(
                        "{} director: possible stall - no relevant events for {}s (threshold={}s)",
                        self.ctx.log_prefix(),
                        idle_secs,
                        thresholds.stall_threshold_secs
                    );
                    let _ = self.ctx.event_tx.send(DaemonEvent::director_stall_detected(
                        &self.ctx.session.id,
                        self.plan_id.as_deref(),
                        idle_secs,
                    ));
                    self.last_stall_emit_at = Some(Instant::now());
                }
            }
        }
        Ok(())
    }

    /// Run one UserIntervention pass. Builds the same execution-context snapshot as
    /// Escalation, prepends the user's message, calls the LLM for an action plan, and
    /// executes any parsed actions via the shared IPC bridge path.
    ///
    /// Errors from the LLM call and empty/unparseable responses are logged and swallowed -
    /// the user sees an acknowledgement either way and the Director flips back to
    /// Monitoring (flip is handled by the caller).
    async fn user_intervention_turn(&mut self, message: String) -> Result<()> {
        info!(
            "{} director: UserIntervention turn ({} chars)",
            self.ctx.log_prefix(),
            message.len()
        );

        let system_prompt = crate::prompts::store().director.clone();
        if system_prompt.is_empty() {
            warn!(
                "{} director: no director prompt configured; emitting echo acknowledgement",
                self.ctx.log_prefix()
            );
            self.emit_llm_text(&format!(
                "[Director received your message but no prompt is configured: {}]",
                message
            ));
            return Ok(());
        }

        let context = self.build_escalation_context(None);
        let user_message = format!(
            "User message: {}\n\nExecution context (JSON):\n{}\n\n\
             Translate the user's intent into concrete actions. Return a JSON object with \
             an 'actions' array. Each action must have a 'type' field with one of: \
             revise-work, re-decompose, abandon-work, spawn-researcher, message-user.",
            message,
            serde_json::to_string_pretty(&context).unwrap_or_default(),
        );

        let response = match self.llm.call(&system_prompt, &user_message).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "{} director: intervention LLM call failed: {}",
                    self.ctx.log_prefix(),
                    e
                );
                self.emit_llm_text(&format!("[Director LLM call failed: {}]", e));
                return Ok(());
            }
        };

        let actions = match parse_actions(&response) {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    "{} director: intervention response parse failed: {} (response preview: {:.200})",
                    self.ctx.log_prefix(),
                    e,
                    response
                );
                // Show the raw response so the user sees what the Director proposed even
                // when the action parser chokes.
                self.emit_llm_text(&response);
                return Ok(());
            }
        };

        if actions.is_empty() {
            debug!("{} director: intervention produced no actions", self.ctx.log_prefix());
            self.emit_llm_text(&response);
            return Ok(());
        }

        let report = execute_actions(&actions, &self.ctx.bridge, &self.ctx.event_tx, &self.ctx.session.id);
        info!(
            "{} director: intervention executed {} actions (ok={} failed={} skipped={})",
            self.ctx.log_prefix(),
            actions.len(),
            report.ok,
            report.failed,
            report.skipped
        );
        Ok(())
    }

    /// Build a JSON context snapshot summarizing plan hierarchy state + recent failures
    /// and rejections from the pattern tracker. This is handed to the LLM alongside the
    /// escalation prompt; the LLM uses it to decide which actions to recommend.
    fn build_escalation_context(&self, target_work_id: Option<&str>) -> serde_json::Value {
        // Snapshot the pattern tracker under no-ops - everything is Cloneable through JSON.
        let mut failures = serde_json::Map::new();
        for (work_id, history) in &self.pattern_tracker.work_failure_history {
            let sessions: Vec<serde_json::Value> = history
                .iter()
                .map(|(sid, err)| serde_json::json!({ "session_id": sid, "error": err }))
                .collect();
            failures.insert(work_id.clone(), serde_json::Value::Array(sessions));
        }
        let mut rejections = serde_json::Map::new();
        for (work_id, reasons) in &self.pattern_tracker.rejection_history {
            rejections.insert(
                work_id.clone(),
                serde_json::Value::Array(reasons.iter().map(|r| serde_json::Value::String(r.clone())).collect()),
            );
        }

        // Summarize work hierarchy from Stores so the LLM knows what to target.
        // The Director is scoped to one plan (max_pool=1) so we don't filter further here;
        // Phase 7 will add spec/phase-level summaries if bubble-up needs richer context.
        let works_summary = self
            .ctx
            .stores
            .read_works()
            .ok()
            .map(|works| {
                let filtered: Vec<serde_json::Value> = works
                    .values()
                    .map(|w| {
                        serde_json::json!({
                            "id": w.id,
                            "title": w.title,
                            "status": format!("{:?}", w.status()),
                            "session_failure_count": w.session_failure_count,
                            "attempt_count": w.attempt_count,
                        })
                    })
                    .collect();
                serde_json::Value::Array(filtered)
            })
            .unwrap_or(serde_json::Value::Null);

        serde_json::json!({
            "plan_id": self.plan_id,
            "target_work_id": target_work_id,
            "pattern_tracker": {
                "work_failure_history": failures,
                "rejection_history": rejections,
                "spec_revision_count": self.pattern_tracker.spec_revision_count,
            },
            "works": works_summary,
        })
    }

    /// Run one Escalation pass: build context, call the LLM for diagnosis, parse the
    /// JSON action array, execute each action via the IPC bridge. After the pass
    /// completes the caller flips back to Monitoring.
    ///
    /// Returns early (with the mode already flipped back) when the prompt is not
    /// configured, when the LLM call fails, or when no actions were parsed. Action
    /// execution failures are recorded in the emitted `director.action_taken` events
    /// but do not halt the batch.
    async fn enter_escalation(&mut self, target_work_id: Option<String>) -> Result<()> {
        info!(
            "{} director: entering Escalation (target_work_id={:?})",
            self.ctx.log_prefix(),
            target_work_id
        );

        let system_prompt = crate::prompts::store().director.clone();
        if system_prompt.is_empty() {
            warn!(
                "{} director: no escalation prompt configured; skipping",
                self.ctx.log_prefix()
            );
            return Ok(());
        }

        let context = self.build_escalation_context(target_work_id.as_deref());
        let user_message = format!(
            "Escalation context (JSON):\n{}\n\nRecommend corrective actions as a JSON object with an 'actions' array. \
             Each action must have a 'type' field with one of: revise-work, re-decompose, abandon-work, spawn-researcher, message-user.",
            serde_json::to_string_pretty(&context).unwrap_or_default(),
        );

        let response = match self.llm.call(&system_prompt, &user_message).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{} director: escalation LLM call failed: {}", self.ctx.log_prefix(), e);
                return Ok(());
            }
        };

        let actions = match parse_actions(&response) {
            Ok(a) => a,
            Err(e) => {
                warn!(
                    "{} director: escalation response parse failed: {} (response preview: {:.200})",
                    self.ctx.log_prefix(),
                    e,
                    response
                );
                return Ok(());
            }
        };

        if actions.is_empty() {
            debug!(
                "{} director: escalation produced no actions; returning to Monitoring",
                self.ctx.log_prefix()
            );
            return Ok(());
        }

        let report = execute_actions(&actions, &self.ctx.bridge, &self.ctx.event_tx, &self.ctx.session.id);
        info!(
            "{} director: escalation executed {} actions (ok={} failed={} skipped={})",
            self.ctx.log_prefix(),
            actions.len(),
            report.ok,
            report.failed,
            report.skipped
        );
        Ok(())
    }

    /// Shared event-driven loop for PlanIntake, Monitoring, and UserIntervention modes.
    async fn event_loop(&mut self) -> Result<()> {
        let heartbeat = std::time::Duration::from_millis(HEARTBEAT_INTERVAL_MS);

        // Entry-point reconciliation: freshly spawned or supervision-restarted Directors
        // must seed their pattern tracker from ground truth before the first event.
        if matches!(self.mode, DirectorMode::Monitoring) {
            self.reconcile_from_ipc().await?;
        }

        loop {
            if self.ctx.is_cancelled() {
                info!(
                    "{} director: exiting event loop (cancelled={})",
                    self.ctx.log_prefix(),
                    self.ctx.is_cancelled()
                );
                break;
            }

            // Destructure for disjoint borrows: event_rx and user_message_rx are separate
            // fields so we can hold mutable references to both across the select!.
            let AgentContext {
                ref mut event_rx,
                ref mut user_message_rx,
                ..
            } = self.ctx;

            let event_rx = event_rx
                .as_mut()
                .expect("Director AgentContext must carry an event broadcast receiver");
            let msg_fut: OptionFuture<_> = user_message_rx.as_mut().map(|rx| rx.recv()).into();

            let recv_result: LoopTick = tokio::select! {
                event = event_rx.recv() => LoopTick::Event(event),
                Some(msg) = msg_fut => LoopTick::User(msg),
                _ = tokio::time::sleep(heartbeat) => LoopTick::Heartbeat,
            };

            match recv_result {
                LoopTick::Event(Ok(ev)) => self.process_event(ev).await?,
                LoopTick::Event(Err(RecvError::Lagged(n))) => {
                    warn!(
                        "{} director: event stream lagged by {} events; reconciling from IPC",
                        self.ctx.log_prefix(),
                        n
                    );
                    self.reconcile_from_ipc().await?;
                }
                LoopTick::Event(Err(RecvError::Closed)) => {
                    info!("{} director: event channel closed; exiting", self.ctx.log_prefix());
                    break;
                }
                LoopTick::User(Some(msg)) => self.handle_user_message(msg).await?,
                LoopTick::User(None) => {
                    // Sender dropped. Drop the receiver so OptionFuture falls through in
                    // future iterations rather than yielding None forever.
                    self.ctx.user_message_rx = None;
                }
                LoopTick::Heartbeat => {
                    self.heartbeat().await?;
                    if self.is_plan_terminal() {
                        info!(
                            "{} director: plan is terminal, exiting event loop",
                            self.ctx.log_prefix()
                        );
                        break;
                    }
                }
            }

            // Post-handler: if pattern detection flipped the mode to Escalation, run
            // one escalation pass and flip back. Keeping this outside the select! branches
            // keeps the dispatch uniform - any handler that sets Escalation triggers it.
            if matches!(self.mode, DirectorMode::Escalation) {
                let target = self.pending_escalation_target.take();
                self.enter_escalation(target).await?;
                // Return to Monitoring so the loop keeps watching for new patterns.
                self.set_mode(DirectorMode::Monitoring);
            }
        }
        Ok(())
    }
}

/// Outcome of one tick of the event loop. Prevents overlapping mutable borrows of `self`
/// by separating the select! branches from the handler calls.
enum LoopTick {
    Event(std::result::Result<DaemonEvent, RecvError>),
    User(Option<String>),
    Heartbeat,
}

impl<L: LlmClient> Agent for DirectorAgent<L> {
    fn agent_type(&self) -> AgentKind {
        AgentKind::Director
    }

    async fn run(&mut self) -> Result<()> {
        self.ctx.session.iteration = 1;
        self.determine_initial_mode();
        debug!(
            "{} director: starting mode={:?} plan_id={:?} model={} (lifeguard enabled)",
            self.ctx.log_prefix(),
            self.mode,
            self.plan_id,
            self.config.llm.model,
        );
        // Silence unused-field warning until Phase 5/7 wire the lifeguard in.
        let _ = &self.lifeguard;

        match self.mode {
            // Legacy escalation spawns (non-pl-* target_id) run one escalation pass and exit.
            // The event loop isn't engaged because these sessions have no plan context to
            // monitor - they exist solely to diagnose a single failure and propose actions.
            DirectorMode::Escalation => {
                let target = self.ctx.session.target_id.clone();
                self.enter_escalation(target).await
            }
            DirectorMode::PlanIntake | DirectorMode::Monitoring | DirectorMode::UserIntervention => {
                self.event_loop().await
            }
        }
    }
}

#[cfg(test)]
mod tests;
