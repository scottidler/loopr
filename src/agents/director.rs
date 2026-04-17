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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use eyre::Result;
use futures::future::OptionFuture;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, trace, warn};

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

/// Heartbeat cadence for the Director run loop.
///
/// Heartbeat is the poll rate for two level-triggered checks: session cancellation and
/// plan-terminal. In steady state the loop is event-driven; heartbeat exists solely so
/// the loop remains responsive to shutdown even when the broadcast stream is quiet.
/// 1 second is cheap (tokio::time::sleep is a microscopic allocation) and responsive
/// enough for user-visible cancel latency.
const HEARTBEAT_INTERVAL_MS: u64 = 1_000;

/// Stall threshold: if no relevant events arrive for this long during Monitoring,
/// the heartbeat emits a `director.stall_detected` event. Phase 5 consumes the event
/// and decides whether to enter Escalation.
const STALL_THRESHOLD_SECS: u64 = 300;

/// Re-emission throttle for stall detection. Once a stall has been announced, suppress
/// repeat emissions until the idle interval doubles, so the TUI isn't spammed once per
/// heartbeat after the threshold is first crossed.
const STALL_REEMIT_SECS: u64 = 300;

/// Failure-count threshold for cross-session escalation. When the same work_id has
/// accumulated this many failures (across any implementer sessions), the Director
/// transitions to Escalation. Design doc §Edge Cases codifies the default at 3.
const WORK_FAILURE_THRESHOLD: usize = 3;

/// Rejection-count threshold for cross-session escalation. Same semantics as
/// `WORK_FAILURE_THRESHOLD` but counts bundle rejections for the work.
const WORK_REJECTION_THRESHOLD: usize = 3;

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
    }

    /// Count of distinct failures observed for a given work.
    pub fn failure_count(&self, work_id: &str) -> usize {
        self.work_failure_history.get(work_id).map(Vec::len).unwrap_or(0)
    }

    /// Count of distinct rejections observed for a given work's bundles.
    pub fn rejection_count(&self, work_id: &str) -> usize {
        self.rejection_history.get(work_id).map(Vec::len).unwrap_or(0)
    }
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

    /// Cross-session threshold check: if the tracker has accumulated enough failure or
    /// rejection history for this work, flip to Escalation mode (observational in Phase 4;
    /// Phase 5 re-dispatches the loop to the escalation handler). Safe to call after any
    /// pattern-tracker mutation.
    fn check_thresholds(&mut self, work_id: &str) {
        if !matches!(self.mode, DirectorMode::Monitoring) {
            return;
        }
        let failures = self.pattern_tracker.failure_count(work_id);
        let rejections = self.pattern_tracker.rejection_count(work_id);
        if failures >= WORK_FAILURE_THRESHOLD || rejections >= WORK_REJECTION_THRESHOLD {
            warn!(
                "{} director: threshold exceeded for work_id={} (failures={}, rejections={}) -> Escalation",
                self.ctx.log_prefix(),
                work_id,
                failures,
                rejections
            );
            // Phase 4 flips the mode for observability; the event loop continues running
            // and the Director will return to Monitoring on the next Phase 5 pass. The
            // `run()` dispatcher keys off the initial mode, not mid-loop transitions, so
            // we don't re-enter legacy_escalation here.
            self.set_mode(DirectorMode::Escalation);
        }
    }

    /// Query the plan's current status via the IPC bridge. Returns true for Complete /
    /// Superseded / Abandoned. Also returns true if plan_id is set but the plan can no
    /// longer be found (defensive: treat missing plan as terminal to avoid spinning).
    fn is_plan_terminal(&self) -> bool {
        let Some(plan_id) = &self.plan_id else {
            // No plan yet (e.g., PlanIntake pre-acceptance). Not terminal - keep running.
            return false;
        };
        let Ok(plans) = self.ctx.stores.read_plans() else {
            warn!(
                "{} director: plans lock poisoned; treating plan as terminal",
                self.ctx.log_prefix()
            );
            return true;
        };
        match plans.get(plan_id) {
            Some(plan) => {
                use crate::domain::plan::HierarchyStatus;
                matches!(
                    plan.status(),
                    HierarchyStatus::Complete | HierarchyStatus::Superseded | HierarchyStatus::Abandoned
                )
            }
            None => {
                warn!(
                    "{} director: plan {} no longer exists; terminating monitoring",
                    self.ctx.log_prefix(),
                    plan_id
                );
                true
            }
        }
    }

    /// Rebuild the pattern tracker from persistent state.
    ///
    /// Phase 2 provides the hook; Phase 7 fills in the query logic to read
    /// `Work.session_failure_count`, rejected `Bundle` records, and `Spec.revision_count`.
    /// The stub clears the tracker so subsequent event-driven updates start fresh
    /// rather than accumulating alongside dropped events.
    async fn reconcile_from_ipc(&mut self) -> Result<()> {
        debug!(
            "{} director: reconcile_from_ipc (Phase 7 fills this in)",
            self.ctx.log_prefix()
        );
        self.pattern_tracker.clear();
        // Phase 7: query bridge.work_list(plan_id), bundle_list, spec_list and repopulate.
        Ok(())
    }

    /// Classify and record a broadcast event. No LLM calls here - judgment lands in
    /// Phase 4 (Monitoring) and Phase 5 (Escalation).
    async fn process_event(&mut self, event: DaemonEvent) -> Result<()> {
        self.event_count += 1;
        self.last_event_at = Instant::now();

        // Work id whose threshold should be (re)checked after recording this event.
        // Set only when the event actually touched a work's failure/rejection history.
        let mut touched_work: Option<String> = None;

        match event.event.as_str() {
            // Plan acceptance completes the PlanIntake → Monitoring handoff.
            "doc.plan_accepted" => {
                if let Some(pid) = event.data.get("plan_id").and_then(|v| v.as_str()) {
                    self.plan_id = Some(pid.to_string());
                }
                if matches!(self.mode, DirectorMode::PlanIntake) {
                    self.set_mode(DirectorMode::Monitoring);
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
            _ => {
                trace!("{} director: observed event {}", self.ctx.log_prefix(), event.event);
            }
        }

        if let Some(work_id) = touched_work {
            self.check_thresholds(&work_id);
        }
        Ok(())
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
                // Phase 6 implements UserIntervention. Phase 3 just records the transition
                // for observability and emits a user-visible acknowledgement. `set_mode`
                // handles persistence + mode_changed event emission.
                self.set_mode(DirectorMode::UserIntervention);
                self.emit_llm_text(&format!(
                    "[Director accepted user message during Monitoring; UserIntervention handling lands in Phase 6. Message: {}]",
                    message
                ));
                self.set_mode(DirectorMode::Monitoring);
                Ok(())
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

    /// Periodic heartbeat. Detects stalls (no events for STALL_THRESHOLD_SECS during
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
            let idle_secs = self.last_event_at.elapsed().as_secs();
            if idle_secs >= STALL_THRESHOLD_SECS {
                let should_emit = match self.last_stall_emit_at {
                    None => true,
                    Some(last) => last.elapsed().as_secs() >= STALL_REEMIT_SECS,
                };
                if should_emit {
                    warn!(
                        "{} director: possible stall - no relevant events for {}s",
                        self.ctx.log_prefix(),
                        idle_secs
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

    /// Legacy one-shot Escalation behavior, preserved from v1 while Phase 5 pending.
    /// This is the path engine primitives take today when they call `spawn-agent` with
    /// `role: director` and a non-plan `target_id`.
    async fn legacy_escalation(&mut self) -> Result<()> {
        info!(
            "{} director: escalation mode for target {:?}",
            self.ctx.log_prefix(),
            self.ctx.session.target_id
        );
        let system_prompt = crate::prompts::store().director.clone();
        if system_prompt.is_empty() {
            info!(
                "{} director: no prompt configured, completing without LLM call",
                self.ctx.log_prefix()
            );
            return Ok(());
        }
        let user_message = format!(
            "Escalation: mechanical recovery failed for target {:?}. Diagnose and recommend action.",
            self.ctx.session.target_id
        );
        match self.llm.call(&system_prompt, &user_message).await {
            Ok(response) => info!(
                "{} director: escalation diagnosis complete ({}B)",
                self.ctx.log_prefix(),
                response.len()
            ),
            Err(e) => info!(
                "{} director: LLM call failed (expected in test): {}",
                self.ctx.log_prefix(),
                e
            ),
        }
        Ok(())
    }

    /// Shared event-driven loop for PlanIntake, Monitoring, and UserIntervention modes.
    async fn event_loop(&mut self) -> Result<()> {
        let heartbeat = std::time::Duration::from_millis(HEARTBEAT_INTERVAL_MS);

        loop {
            if self.is_plan_terminal() || self.ctx.is_cancelled() {
                info!(
                    "{} director: exiting event loop (plan_terminal={} cancelled={})",
                    self.ctx.log_prefix(),
                    self.is_plan_terminal(),
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
                LoopTick::Heartbeat => self.heartbeat().await?,
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
            DirectorMode::Escalation => self.legacy_escalation().await,
            DirectorMode::PlanIntake | DirectorMode::Monitoring | DirectorMode::UserIntervention => {
                self.event_loop().await
            }
        }
    }
}

#[cfg(test)]
mod tests;
