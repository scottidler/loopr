use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, eyre};

use tokio::sync::broadcast;

use crate::agents::agent_logger::AgentLogger;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::lifeguard::{self, Lifeguard, Verdict};
use crate::agents::{Agent, AgentAction, AgentContext, AgentKind};
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::role::Role;
use crate::ipc::protocol::DaemonEvent;

/// A message in a multi-turn conversation (for self-correction loops).
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: &str) -> Self {
        Self {
            role: "user".to_string(),
            content: content.to_string(),
        }
    }
    pub fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
        }
    }
}

/// Trait for LLM calls — allows mocking in tests.
/// Phase 4 provides the real streaming implementation (`AgentLlmClient`).
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Call the LLM with a system prompt and user message, return the full response text.
    async fn call(&self, system_prompt: &str, user_message: &str) -> Result<String>;

    /// Call with multi-turn conversation history for self-correction.
    /// Default implementation extracts the last user message and delegates to `call`.
    async fn call_with_history(&self, system_prompt: &str, messages: &[ChatMessage]) -> Result<String> {
        let last_user = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        self.call(system_prompt, last_user).await
    }
}

/// Strip markdown code fences from LLM responses.
/// Handles ```json, ```, and variants with language tags.
fn strip_markdown_fences(response: &str) -> String {
    let trimmed = response.trim();
    if let Some(rest) = trimmed.strip_prefix("```") {
        // Skip the language tag line (e.g., "json\n")
        let after_tag = rest.find('\n').map(|i| &rest[i + 1..]).unwrap_or(rest);
        // Strip closing fence
        let content = after_tag.trim_end().strip_suffix("```").unwrap_or(after_tag);
        content.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Normalize common LLM key deviations: "type" → "action".
/// LLMs sometimes use "type" instead of "action" as the discriminant key.
fn normalize_action_keys(response: &str) -> String {
    response.replace("\"type\":", "\"action\":")
}

/// Parse the LLM response into a list of agent actions.
/// Tolerates prose before/after the JSON array — finds `[` and its matching `]`.
pub fn parse_actions(response: &str, agent_log: &AgentLogger) -> Result<Vec<AgentAction>> {
    agent_log.debug(&format!("parse_actions(response_len={})", response.len()));
    // Strip markdown fences before any parsing attempts
    let stripped = strip_markdown_fences(response);
    // Normalize "type" → "action" before any parsing attempts
    let normalized = normalize_action_keys(&stripped);
    let response = &normalized;

    // Try direct parse first
    if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(response) {
        return Ok(actions);
    }

    // Find the first `[` and try increasingly larger `]` matches until one parses.
    // This handles: prose before JSON, code fences, multiple arrays (takes first valid one).
    let trimmed = response.trim();
    if let Some(start) = trimmed.find('[') {
        let rest = &trimmed[start..];
        // Try every `]` from nearest to farthest
        for (offset, _) in rest.match_indices(']') {
            let candidate = &rest[..=offset];
            if let Ok(actions) = serde_json::from_str::<Vec<AgentAction>>(candidate) {
                // Accept empty arrays (valid "no actions" response) and non-empty arrays
                return Ok(actions);
            }
            // Also try element-by-element
            if let Ok(values) = serde_json::from_str::<Vec<serde_json::Value>>(candidate) {
                let actions: Vec<AgentAction> = values
                    .into_iter()
                    .filter_map(|v| serde_json::from_value(v).ok())
                    .collect();
                if !actions.is_empty() {
                    return Ok(actions);
                }
            }
        }
    }

    let snippet: String = response.chars().take(500).collect();
    Err(eyre!(
        "failed to parse agent actions from LLM response (response snippet: {})",
        snippet
    ))
}

/// Classify an error as correctable (schema/path errors the LLM can fix with a re-prompt).
/// Non-correctable errors (compilation failure, test failure, network error) require
/// full-iteration reasoning and should NOT trigger intra-turn re-prompting.
pub fn is_correctable_error(error: &str) -> bool {
    error.contains("missing field")
        || error.contains("unknown field")
        || error.contains("invalid type")
        || error.contains("expected array")
        || error.contains("path escapes")
        || error.contains("unknown tool")
        || error.contains("path traversal")
}

/// Build a focused state summary for the implementer: active locks and sibling agents.
pub fn build_implementer_summary(stores: &Stores, work_id: &str, agent_log: &AgentLogger) -> String {
    agent_log.debug(&format!("build_implementer_summary(work_id={})", work_id));
    use crate::domain::lock::LockStatus;

    let mut summary = String::with_capacity(512);

    // Active locks on resources
    {
        let Ok(locks) = stores.read_locks() else { return summary };
        let active: Vec<_> = locks.values().filter(|l| l.status() == LockStatus::Active).collect();
        if !active.is_empty() {
            summary.push_str("### Active Locks\n");
            for l in &active {
                summary.push_str(&format!("- {} (holder: {})\n", l.resource, l.holder_id));
            }
            summary.push('\n');
        }
    }

    // Active agents working on sibling works
    {
        let Ok(sessions) = stores.read_agent_sessions() else {
            return summary;
        };
        let siblings: Vec<_> = sessions
            .values()
            .filter(|s| !s.status().is_terminal() && s.work_id.as_deref() != Some(work_id) && s.work_id.is_some())
            .collect();
        if !siblings.is_empty() {
            summary.push_str("### Sibling Agents\n");
            for s in &siblings {
                summary.push_str(&format!(
                    "- {} {} (wi: {})\n",
                    s.agent_type,
                    s.status(),
                    s.work_id.as_deref().unwrap_or("?")
                ));
            }
            summary.push('\n');
        }
    }

    summary
}

/// Outcome of a single implementer iteration.
#[derive(Debug)]
pub enum IterationOutcome {
    Continue(String),
    Done(String),
    NeedHelp(String),
}

/// The Implementer agent — multi-iteration LLM agent that implements Work items.
pub struct ImplementerAgent {
    pub ctx: AgentContext,
    llm: Box<dyn LlmClient>,
    config: AgentRoleConfig,
    work_id: String,
    worktree_path: PathBuf,
    previous_summary: Option<String>,
    has_proposed: bool,
}

impl ImplementerAgent {
    pub fn new(
        ctx: AgentContext,
        llm: Box<dyn LlmClient>,
        config: AgentRoleConfig,
        work_id: String,
        worktree_path: PathBuf,
    ) -> Self {
        Self {
            ctx,
            llm,
            config,
            work_id,
            worktree_path,
            previous_summary: None,
            has_proposed: false,
        }
    }

    /// Run a single implementer iteration: load context -> prompt -> call LLM -> parse -> execute.
    /// Includes a self-correction loop: if parse_actions fails, the error is fed back to the LLM
    /// within the same iteration for up to `max_requeries` re-prompts.
    pub async fn run_iteration(
        &self,
        iteration: u32,
        staleness_note: Option<String>,
        guard: &mut Lifeguard,
    ) -> Result<IterationOutcome> {
        self.ctx.debug(&format!(
            "run_iteration(iteration={}, has_previous_summary={})",
            iteration,
            self.previous_summary.is_some()
        ));
        let state_summary = build_implementer_summary(&self.ctx.stores, &self.work_id, &self.ctx.log);
        let assembled = ContextBuilder::new(&self.ctx.stores, Role::Implementer)
            .load_work_hierarchy(&self.work_id)?
            .with_guidance(&self.ctx.stores.guidance)
            .with_coordinator_goal()
            .with_state_summary(state_summary)
            .with_tools(&self.ctx.tool_runner)
            .with_previous_summary(self.previous_summary.clone())
            .with_staleness_note(staleness_note)
            .with_iteration(iteration)
            .with_footer("Implement the Work described above. Respond with a JSON array of actions.".into())
            .build(&crate::prompts::store().implementer)?;

        self.ctx.info(&format!("context: ~{} tokens", assembled.token_estimate));

        // Self-correction loop: re-prompt on parse failure up to max_requeries times
        let mut messages = vec![ChatMessage::user(&assembled.user_message)];
        let mut requeries = 0u32;

        let actions = loop {
            let response = self.llm.call_with_history(&assembled.system_prompt, &messages).await?;
            self.ctx.log.write_iter_file(
                iteration,
                Some(&self.work_id),
                &assembled.system_prompt,
                &assembled.user_message,
                &response,
            );
            self.ctx.info(&format!(
                "raw LLM response ({} chars): {}",
                response.len(),
                &response[..response.len().min(800)]
            ));

            match parse_actions(&response, &self.ctx.log) {
                Ok(actions) => break actions,
                Err(parse_err) => {
                    requeries += 1;
                    if requeries > self.config.max_requeries {
                        // Exceeded re-prompt budget — error bubbles up to run() for lifeguard tracking
                        return Err(parse_err);
                    }
                    self.ctx.info(&format!(
                        "parse failed (requery {}/{}): {}",
                        requeries, self.config.max_requeries, parse_err
                    ));
                    // Append the failed response and error as new messages
                    messages.push(ChatMessage::assistant(&response));
                    messages.push(ChatMessage::user(&format!(
                        "Your response could not be parsed as a valid JSON action array.\n\
                         Error: {}\n\n\
                         Please respond with ONLY a valid JSON array of actions. \
                         Do not include any text before or after the JSON.",
                        parse_err
                    )));
                }
            }
        };

        guard.reset_parse_failures();

        if actions.is_empty() {
            return Err(eyre!("LLM returned empty action list"));
        }

        // Tool error correction budget: shared with parse corrections
        let mut remaining_corrections = self.config.max_requeries.saturating_sub(requeries);

        let session_id = &self.ctx.session.id;
        let mut summaries = Vec::new();
        for action in &actions {
            // Lifeguard: check for repeated identical actions
            let action_hash = lifeguard::hash_action(action);
            if let Verdict::Escalate(reason) = guard.check_action(action_hash) {
                self.ctx.warn(&format!("lifeguard: {}", reason));
                return Ok(IterationOutcome::NeedHelp(format!("lifeguard: {}", reason)));
            }

            // Broadcast tool_started event for RunTool actions
            if let AgentAction::RunTool { tool, .. } = action {
                let _ = self
                    .ctx
                    .event_tx
                    .send(DaemonEvent::agent_tool_started(session_id, tool));
            }

            let result = match execute_action(action, &self.ctx, &self.worktree_path, Some(&self.work_id)).await {
                Ok(r) => r,
                Err(e) if is_correctable_error(&e.to_string()) && remaining_corrections > 0 => {
                    // Correctable tool error: re-prompt the LLM for a corrected action
                    remaining_corrections -= 1;
                    let err_msg = e.to_string();
                    self.ctx.info(&format!(
                        "correctable tool error (corrections left: {}): {}",
                        remaining_corrections, err_msg
                    ));

                    let action_json = serde_json::to_string(action).unwrap_or_default();
                    messages.push(ChatMessage::assistant(&format!("[{}]", action_json)));
                    messages.push(ChatMessage::user(&format!(
                        "The action failed with error:\n{}\n\n\
                         Please provide a corrected action as a JSON array with a single action.",
                        err_msg
                    )));

                    match self.llm.call_with_history(&assembled.system_prompt, &messages).await {
                        Ok(corrected_response) => match parse_actions(&corrected_response, &self.ctx.log) {
                            Ok(corrected_actions) => {
                                // Execute corrected action(s)
                                let mut corrected_result = ActionResult::ActionError(err_msg.clone());
                                for ca in &corrected_actions {
                                    corrected_result =
                                        match execute_action(ca, &self.ctx, &self.worktree_path, Some(&self.work_id))
                                            .await
                                        {
                                            Ok(r) => r,
                                            Err(ce) => ActionResult::ActionError(ce.to_string()),
                                        };
                                }
                                corrected_result
                            }
                            Err(_) => {
                                // Correction parse failed — record original error
                                ActionResult::ActionError(err_msg)
                            }
                        },
                        Err(_) => ActionResult::ActionError(err_msg),
                    }
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    self.ctx.warn(&format!("action failed (non-fatal): {err_msg}"));
                    // Lifeguard: check for repeated errors (config errors don't escalate)
                    let (verdict, warning) = guard.record_error(&err_msg);
                    if let Some(w) = warning {
                        self.ctx.warn(&w);
                    }
                    if let Verdict::Escalate(reason) = verdict {
                        self.ctx.warn(&format!("lifeguard: {}", reason));
                        return Ok(IterationOutcome::NeedHelp(format!("lifeguard: {}", reason)));
                    }
                    ActionResult::ActionError(err_msg)
                }
            };

            // Broadcast tool_completed event for RunTool results
            if let ActionResult::ToolRun(ref tr) = result {
                let _ = self.ctx.event_tx.send(DaemonEvent::agent_tool_completed(
                    session_id,
                    &tr.tool,
                    tr.exit_code,
                    tr.duration_ms,
                ));
            }

            let summary = format_action_summary(action, &result);
            let _ = self
                .ctx
                .event_tx
                .send(DaemonEvent::agent_action_completed(session_id, &summary));

            match &result {
                ActionResult::ActionError(_) => {
                    summaries.push(summary);
                    break;
                }
                ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
                ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
                ActionResult::BundleProposed(desc) => {
                    // Auto-complete: proposing a bundle means the work is done
                    summaries.push(summary);
                    let done_summary = if desc.is_empty() {
                        summaries.join("\n")
                    } else {
                        format!("{}\n{}", summaries.join("\n"), desc)
                    };
                    return Ok(IterationOutcome::Done(done_summary));
                }
                _ => {}
            }
            summaries.push(summary);
        }

        Ok(IterationOutcome::Continue(summaries.join("\n")))
    }
}

/// Check a broadcast receiver for `tick.published` events, returning the latest tick ID if found.
fn drain_tick_published(event_rx: &mut broadcast::Receiver<DaemonEvent>, agent_log: &AgentLogger) -> Option<String> {
    agent_log.debug("drain_tick_published()");
    let mut latest_tick_id: Option<String> = None;
    loop {
        match event_rx.try_recv() {
            Ok(event) if event.event == "tick.published" => {
                if let Some(tid) = event.data.get("tick_id").and_then(|v| v.as_str()) {
                    latest_tick_id = Some(tid.to_string());
                }
            }
            Ok(_) => {} // ignore non-tick events
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => break,
            Err(broadcast::error::TryRecvError::Lagged(_)) => {
                // Missed messages — continue draining from current position
                continue;
            }
        }
    }
    latest_tick_id
}

#[async_trait]
impl Agent for ImplementerAgent {
    async fn run(&mut self) -> Result<()> {
        self.ctx.debug(&format!(
            "run(session_id={}, work_id={})",
            self.ctx.session.id, self.work_id
        ));

        let max_iterations = self.config.max_iterations;
        let mut event_rx = self.ctx.event_tx.subscribe();
        let mut guard = Lifeguard::new();

        for i in 1..=max_iterations {
            if self.ctx.is_cancelled() {
                self.ctx.info("cancelled, exiting loop");
                return Ok(());
            }

            self.ctx.session.iteration = i;
            self.ctx.persist_iteration();
            self.ctx.info(&format!("iteration {}/{}", i, max_iterations));

            // F3(A): Budget-exhaustion prompt injection on penultimate iteration
            if i >= max_iterations.saturating_sub(1) {
                let budget_warning = format!(
                    "\n\n## URGENT: Budget Exhausted\n\
                    You have {} iteration(s) remaining. You MUST call `propose_bundle` NOW \
                    with whatever code you have, even if tests fail. Commit first, then propose. \
                    Include a description of what works and what doesn't. \
                    The Reviewer will evaluate quality — your job is to submit.\n",
                    max_iterations - i
                );
                self.previous_summary = Some(
                    self.previous_summary
                        .take()
                        .map_or(budget_warning.clone(), |s| format!("{}\n{}", s, budget_warning)),
                );
            }

            // Check for staleness (tick.published events since last iteration)
            let staleness_note = if let Some(new_tick_id) = drain_tick_published(&mut event_rx, &self.ctx.log) {
                self.ctx
                    .info(&format!("detected stale: new tick {} published", new_tick_id));
                let _ = self.ctx.event_tx.send(DaemonEvent::agent_staleness_detected(
                    &self.ctx.session.id,
                    &new_tick_id,
                ));
                Some(format!(
                    "A new Tick '{}' has been published since your last iteration. \
                         Your worktree may be based on an older Tick. Review your changes \
                         for conflicts and re-run tests before proposing a Bundle. \
                         Use the latest base when proposing.",
                    new_tick_id
                ))
            } else {
                None
            };

            match self.run_iteration(i, staleness_note, &mut guard).await {
                Ok(IterationOutcome::Done(summary)) => {
                    self.ctx.emit_iteration_completed(i, &summary);
                    self.ctx.info(&format!("completed: {}", summary));
                    return Ok(());
                }
                Ok(IterationOutcome::NeedHelp(reason)) => {
                    self.ctx.emit_iteration_completed(i, &reason);
                    self.ctx.warn(&format!("needs help: {}", reason));
                    return Err(eyre!("agent needs help: {}", reason));
                }
                Ok(IterationOutcome::Continue(summary)) => {
                    if summary.contains("proposed bundle:") {
                        self.has_proposed = true;
                    }
                    self.ctx.emit_iteration_completed(i, &summary);
                    self.ctx.info(&format!("iteration {} done: {}", i, summary));
                    // Accumulate history so the LLM knows what it already did
                    let entry = format!("--- Iteration {} ---\n{}", i, summary);
                    self.previous_summary = Some(match self.previous_summary.take() {
                        Some(prev) => format!("{}\n{}", prev, entry),
                        None => entry,
                    });
                }
                Err(e) => {
                    // Lifeguard: track parse failures
                    if let Verdict::Escalate(reason) = guard.record_parse_failure() {
                        self.ctx.warn(&format!("lifeguard: {}", reason));
                        return Err(eyre!("lifeguard: {}", reason));
                    }
                    self.ctx.warn(&format!("iteration {} failed (will retry): {}", i, e));
                    self.previous_summary = Some(format!(
                        "ERROR: Your previous response could not be parsed. \
                         You MUST respond with ONLY a JSON array of action objects. \
                         No prose, no markdown, no explanation. Example: \
                         [{{\"action\": \"read_file\", \"path\": \"src/main.rs\"}}]\n\
                         Parse error: {}",
                        e
                    ));
                    continue;
                }
            }
        }

        // F3(B): Force-propose if the loop exhausted without a proposal.
        // ProposeBundle auto-commits any pending changes, so no separate Commit needed.
        if !self.has_proposed {
            self.ctx.info("force-proposing at iteration cap");
            match execute_action(
                &AgentAction::ProposeBundle {
                    description: format!(
                        "Auto-proposed at iteration cap ({}). Tests may not pass.",
                        max_iterations
                    ),
                    claims: vec!["partial implementation - needs review".to_string()],
                    noop_reason: None,
                },
                &self.ctx,
                &self.worktree_path,
                Some(&self.work_id),
            )
            .await
            {
                Ok(result) => self.ctx.info(&format!("Force-propose result: {:?}", result)),
                Err(e) => self.ctx.warn(&format!("Force-propose failed: {}", e)),
            }
        }

        Err(eyre!("implementer reached max iterations ({})", max_iterations))
    }

    fn agent_type(&self) -> AgentKind {
        AgentKind::Implementer
    }
}

/// Max chars of file/tool output to include in the action summary fed back to the LLM.
const MAX_SUMMARY_CONTENT: usize = 4000;

fn truncate_content(content: &str, max: usize) -> String {
    if content.len() <= max {
        content.to_string()
    } else {
        format!("{}...\n[truncated, {} total bytes]", &content[..max], content.len())
    }
}

fn format_action_summary(action: &AgentAction, result: &ActionResult) -> String {
    match result {
        ActionResult::ToolRun(tr) => {
            let mut s = format!("ran {} (exit {})", tr.tool, tr.exit_code);
            if !tr.stdout.is_empty() {
                s.push_str(&format!(
                    "\nstdout:\n```\n{}\n```",
                    truncate_content(&tr.stdout, MAX_SUMMARY_CONTENT)
                ));
            }
            if !tr.stderr.is_empty() {
                s.push_str(&format!(
                    "\nstderr:\n```\n{}\n```",
                    truncate_content(&tr.stderr, MAX_SUMMARY_CONTENT)
                ));
            }
            s
        }
        ActionResult::FileWritten(p) => format!("wrote {}", p),
        ActionResult::FileEdited(p) => format!("edited {}", p),
        ActionResult::FileRead(content) => {
            let path = if let AgentAction::ReadFile { path, .. } = action {
                path.as_str()
            } else {
                "?"
            };
            format!(
                "read {} ({} bytes):\n```\n{}\n```",
                path,
                content.len(),
                truncate_content(content, MAX_SUMMARY_CONTENT)
            )
        }
        ActionResult::Committed(m) => format!("committed: {}", m),
        ActionResult::BundleProposed(d) => format!("proposed bundle: {}", d),
        ActionResult::Transitioned(d) => format!("transitioned: {}", d),
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::ToolRegistered(n) => format!("registered tool: {}", n),
        ActionResult::LockAcquired(id) => format!("lock acquired: {}", id),
        ActionResult::LockReleased(id) => format!("lock released: {}", id),
        ActionResult::DocumentValidated { verdict, summary, .. } => {
            format!("validated: {} — {}", verdict, summary)
        }
        ActionResult::Done(s) => format!("done: {}", s),
        ActionResult::NeedHelp(r) => format!("need help: {}", r),
        ActionResult::ActionError(e) => format!("ERROR: {}", e),
        ActionResult::RecordCreated { collection, id } => format!("created {}: {}", collection, id),
        ActionResult::AgentSpawned { session_id, agent_type } => {
            format!("spawned {} ({})", agent_type, session_id)
        }
        ActionResult::CoverageEvaluated { verdict, gaps, .. } => format!("coverage: {} ({} gaps)", verdict, gaps.len()),
        ActionResult::DependencyNotMet { work_id, message } => {
            format!("dep not met for {}: {}", work_id, message)
        } // M10-12: DuplicateDetected, PhaseCompleted, GoalCompleted removed — dead variants
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
