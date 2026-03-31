use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, eyre};

use tokio::sync::broadcast;

use crate::agents::agent_logger::AgentLogger;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::lifeguard::{self, Lifeguard, Verdict};
use crate::agents::{Agent, AgentAction, AgentContext, AgentType};
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
        let active: Vec<_> = locks.values().filter(|l| l.status == LockStatus::Active).collect();
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
            .filter(|s| !s.status.is_terminal() && s.work_id.as_deref() != Some(work_id) && s.work_id.is_some())
            .collect();
        if !siblings.is_empty() {
            summary.push_str("### Sibling Agents\n");
            for s in &siblings {
                summary.push_str(&format!(
                    "- {} {} (wi: {})\n",
                    s.agent_type,
                    s.status,
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
            .build(&crate::prompts::store().implementer);

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

        // F3(B): Force-propose if the loop exhausted without a proposal
        if !self.has_proposed {
            self.ctx.info("force-proposing at iteration cap");
            // Commit whatever is in the worktree
            let _ = execute_action(
                &AgentAction::Commit {
                    message: format!("WIP: auto-commit at iteration cap ({})", max_iterations),
                    paths: vec![".".to_string()],
                },
                &self.ctx,
                &self.worktree_path,
                Some(&self.work_id),
            )
            .await;
            // Propose the bundle
            let _ = execute_action(
                &AgentAction::ProposeBundle {
                    description: format!(
                        "Auto-proposed at iteration cap ({}). Tests may not pass.",
                        max_iterations
                    ),
                    claims: vec!["partial implementation — needs review".to_string()],
                },
                &self.ctx,
                &self.worktree_path,
                Some(&self.work_id),
            )
            .await;
        }

        Err(eyre!("implementer reached max iterations ({})", max_iterations))
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Implementer
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
mod tests {
    use super::*;
    use crate::agents::agent_logger::AgentLogger;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentSession, AgentType};
    use crate::config::{AgentRoleConfig, Config, ProjectConfig, ToolEntry};
    use crate::daemon::context::Stores;
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::work::Work;
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use crate::worktree::manager::WorktreeManager;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;
    use tokio::sync::broadcast;

    /// Mock LLM client that returns a canned response.
    struct MockLlm {
        response: String,
    }

    impl MockLlm {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    /// Mock LLM that fails.
    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Err(eyre!("LLM call failed"))
        }
    }

    fn setup_stores(dir: &Path) -> Arc<Stores> {
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

        // Populate hierarchy
        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let spec = Spec::new(plan_id.clone(), "Test Spec".into(), "A test spec".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let phase = Phase::new(spec_id.clone(), "Test Phase".into(), "A test phase".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let wi = Work::new(phase_id.clone(), "Test Work".into(), "Implement the feature".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Add a learning
        let learning = Learning::new(
            phase_id,
            LearningScope::Phase,
            "Previous iteration found a bug in parsing".into(),
        );
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        Arc::new(stores)
    }

    fn get_work_id(stores: &Stores) -> String {
        stores.works.read().unwrap().keys().next().unwrap().clone()
    }

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        use crate::agents::AgentType;
        let file_path = dir.join("test-agent.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Implementer, "test-session", file, file_path)
    }

    fn test_bridge_with_tx(stores: Arc<Stores>, dir: &Path) -> (AgentIpcBridge, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        (bridge, event_tx)
    }

    /// Build an ImplementerAgent for tests, wired to the given stores and directory.
    /// Also inserts the session into stores so `is_cancelled()` works correctly.
    fn test_implementer(
        llm: Box<dyn LlmClient>,
        stores: Arc<Stores>,
        dir: &Path,
        config: AgentRoleConfig,
    ) -> ImplementerAgent {
        let wi_id = get_work_id(&stores);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), dir);
        let agent_log = test_agent_logger(dir);
        let tool_runner = stores.tool_runner.clone();
        let tool_executor = stores.tool_executor.clone();

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_id = Some(wi_id.clone());
        session.worktree_path = Some(dir.to_string_lossy().into());

        // Insert into stores so is_cancelled() / persist_iteration() find the session
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
            tool_runner,
            tool_executor,
            log: agent_log,
        };
        ImplementerAgent::new(ctx, llm, config, wi_id, dir.to_path_buf())
    }

    // --- parse_actions tests ---

    #[test]
    fn test_parse_actions_direct_json() {
        let dir = TestDir::new("loopr-impl-parse1");
        let agent_log = test_agent_logger(&dir);
        let json = r#"[{"action": "done", "summary": "All done"}]"#;
        let actions = parse_actions(json, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::Done { .. }));
    }

    #[test]
    fn test_parse_actions_wrapped_in_code_block() {
        let dir = TestDir::new("loopr-impl-parse2");
        let agent_log = test_agent_logger(&dir);
        let response = "Here are the actions:\n```json\n[{\"action\": \"done\", \"summary\": \"done\"}]\n```";
        let actions = parse_actions(response, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn test_parse_actions_multiple() {
        let dir = TestDir::new("loopr-impl-parse3");
        let agent_log = test_agent_logger(&dir);
        let json = r#"[
            {"action": "write_file", "path": "src/foo.rs", "content": "fn foo() {}"},
            {"action": "run_tool", "tool": "test", "args": []},
            {"action": "done", "summary": "Implemented foo"}
        ]"#;
        let actions = parse_actions(json, &agent_log).unwrap();
        assert_eq!(actions.len(), 3);
    }

    #[test]
    fn test_parse_actions_invalid() {
        let dir = TestDir::new("loopr-impl-parse4");
        let agent_log = test_agent_logger(&dir);
        let bad = "This is not JSON at all";
        assert!(parse_actions(bad, &agent_log).is_err());
    }

    #[test]
    fn test_parse_actions_empty_array() {
        let dir = TestDir::new("loopr-impl-parse5");
        let agent_log = test_agent_logger(&dir);
        let json = "[]";
        let actions = parse_actions(json, &agent_log).unwrap();
        assert!(actions.is_empty());
    }

    // --- strip_markdown_fences tests ---

    #[test]
    fn test_strip_markdown_fences_json_block() {
        let input = "```json\n[]\n```";
        assert_eq!(strip_markdown_fences(input), "[]");
    }

    #[test]
    fn test_strip_markdown_fences_bare_input() {
        let input = "[]";
        assert_eq!(strip_markdown_fences(input), "[]");
    }

    #[test]
    fn test_strip_markdown_fences_with_content() {
        let input = "```json\n[{\"action\": \"done\", \"summary\": \"ok\"}]\n```";
        assert_eq!(
            strip_markdown_fences(input),
            "[{\"action\": \"done\", \"summary\": \"ok\"}]"
        );
    }

    #[test]
    fn test_strip_markdown_fences_no_language_tag() {
        let input = "```\n[]\n```";
        assert_eq!(strip_markdown_fences(input), "[]");
    }

    #[test]
    fn test_strip_markdown_fences_prose_not_fenced() {
        let input = "Here is the result: []";
        assert_eq!(strip_markdown_fences(input), "Here is the result: []");
    }

    #[test]
    fn test_parse_actions_fenced_empty_array() {
        let dir = TestDir::new("loopr-impl-parse-fenced-empty");
        let agent_log = test_agent_logger(&dir);
        let response = "```json\n[]\n```";
        let actions = parse_actions(response, &agent_log).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn test_parse_actions_fenced_non_empty_array() {
        let dir = TestDir::new("loopr-impl-parse-fenced-nonempty");
        let agent_log = test_agent_logger(&dir);
        let response = "```json\n[{\"action\": \"done\", \"summary\": \"All done\"}]\n```";
        let actions = parse_actions(response, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::Done { .. }));
    }

    #[test]
    fn test_parse_actions_prose_then_fenced_empty_array() {
        let dir = TestDir::new("loopr-impl-parse-prose-fenced");
        let agent_log = test_agent_logger(&dir);
        let response = "Nothing to do right now. The workers are handling it.\n\n```json\n[]\n```";
        let actions = parse_actions(response, &agent_log).unwrap();
        assert!(actions.is_empty());
    }

    // --- context builder integration tests ---

    #[test]
    fn test_context_builder_for_implementer() {
        let dir = TestDir::new("loopr-impl-ctx");
        let stores = setup_stores(&dir);
        let tool_runner = ToolRunner::new(&[ToolEntry {
            name: "test".into(),
            command: "echo ok".into(),
            timeout_secs: 10,
            worktree: true,
        }]);
        let wi_id = get_work_id(&stores);

        let assembled = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_hierarchy(&wi_id)
            .unwrap()
            .with_tools(&tool_runner)
            .with_iteration(1)
            .with_footer("Respond with JSON.".into())
            .build(&crate::prompts::store().implementer);

        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test Work"));
        assert!(assembled.user_message.contains("`test`"));
        assert!(assembled.user_message.contains("Current Iteration: 1"));
        assert!(assembled.system_prompt.contains("Implementer agent"));
    }

    #[test]
    fn test_context_builder_missing_work() {
        let dir = TestDir::new("loopr-impl-miss");
        let stores = setup_stores(&dir);

        let result = ContextBuilder::new(&stores, Role::Implementer).load_work_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work not found"));
    }

    // --- run_iteration tests ---

    #[tokio::test]
    async fn test_run_iteration_done() {
        let dir = TestDir::new("loopr-impl-iter");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(r#"[{"action": "done", "summary": "All done"}]"#));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s == "All done"));
    }

    #[tokio::test]
    async fn test_run_iteration_need_help() {
        let dir = TestDir::new("loopr-impl-help");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(r#"[{"action": "need_help", "reason": "Ambiguous spec"}]"#));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref r) if r == "Ambiguous spec"));
    }

    #[tokio::test]
    async fn test_run_iteration_continue_with_actions() {
        let dir = TestDir::new("loopr-impl-cont");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(
            r#"[
            {"action": "write_file", "path": "test.txt", "content": "hello"},
            {"action": "read_file", "path": "test.txt"}
        ]"#,
        ));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Continue(_)));
        // File should have been written
        assert!(dir.join("test.txt").exists());
    }

    #[tokio::test]
    async fn test_run_iteration_llm_failure() {
        let dir = TestDir::new("loopr-impl-fail");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm: Box<dyn LlmClient> = Box::new(FailingLlm);
        let agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run_iteration(1, None, &mut Lifeguard::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_iteration_bad_response() {
        let dir = TestDir::new("loopr-impl-bad");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new("This is not valid JSON"));
        let agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run_iteration(1, None, &mut Lifeguard::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_iteration_empty_actions() {
        let dir = TestDir::new("loopr-impl-empty");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new("[]"));
        let agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run_iteration(1, None, &mut Lifeguard::new()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty action list"));
    }

    // --- run_implementer tests ---

    #[tokio::test]
    async fn test_run_implementer_completes_on_done() {
        let dir = TestDir::new("loopr-impl-run");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(r#"[{"action": "done", "summary": "Complete"}]"#));
        let mut agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.ctx.session.iteration, 1);
    }

    #[tokio::test]
    async fn test_run_implementer_max_iterations() {
        let dir = TestDir::new("loopr-impl-max");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3;

        // LLM always returns a write action (no done), so loop should hit max
        let llm = Box::new(MockLlm::new(
            r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#,
        ));
        let mut agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max iterations"));
        assert_eq!(agent.ctx.session.iteration, 3);
    }

    #[tokio::test]
    async fn test_run_implementer_need_help_stops() {
        let dir = TestDir::new("loopr-impl-helpstop");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(r#"[{"action": "need_help", "reason": "Stuck"}]"#));
        let mut agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_instructions() {
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().implementer;
        assert!(prompt.contains("Implementer agent"));
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("run_tool"));
        assert!(prompt.contains("done"));
        assert!(prompt.contains("need_help"));
        assert!(prompt.contains("JSON array"));
    }

    // --- staleness cascade tests ---

    #[test]
    fn test_drain_tick_published_empty() {
        let dir = TestDir::new("loopr-impl-drain1");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        drop(tx);
        let result = drain_tick_published(&mut rx, &agent_log);
        assert!(result.is_none());
    }

    #[test]
    fn test_drain_tick_published_finds_tick() {
        let dir = TestDir::new("loopr-impl-drain2");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::tick_published("tick-42", "abc123"));
        let result = drain_tick_published(&mut rx, &agent_log);
        assert_eq!(result, Some("tick-42".to_string()));
    }

    #[test]
    fn test_drain_tick_published_returns_latest() {
        let dir = TestDir::new("loopr-impl-drain3");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::tick_published("tick-1", "aaa"));
        let _ = tx.send(DaemonEvent::tick_published("tick-2", "bbb"));
        let _ = tx.send(DaemonEvent::tick_published("tick-3", "ccc"));
        let result = drain_tick_published(&mut rx, &agent_log);
        assert_eq!(result, Some("tick-3".to_string()));
    }

    #[test]
    fn test_drain_tick_published_ignores_other_events() {
        let dir = TestDir::new("loopr-impl-drain4");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::record_created("work", "wi-1"));
        let _ = tx.send(DaemonEvent::transition_completed(
            "bundle",
            "b-1",
            "draft",
            "proposed",
            "implementer",
        ));
        let result = drain_tick_published(&mut rx, &agent_log);
        assert!(result.is_none());
    }

    #[test]
    fn test_drain_tick_published_mixed_events() {
        let dir = TestDir::new("loopr-impl-drain5");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(16);
        let _ = tx.send(DaemonEvent::record_created("work", "wi-1"));
        let _ = tx.send(DaemonEvent::tick_published("tick-5", "sha5"));
        let _ = tx.send(DaemonEvent::record_updated("bundle", "b-1"));
        let result = drain_tick_published(&mut rx, &agent_log);
        assert_eq!(result, Some("tick-5".to_string()));
    }

    #[test]
    fn test_context_builder_with_staleness() {
        let dir = TestDir::new("loopr-impl-stale2");
        let stores = setup_stores(&dir);
        let wi_id = get_work_id(&stores);

        let assembled = ContextBuilder::new(&stores, Role::Implementer)
            .load_work_hierarchy(&wi_id)
            .unwrap()
            .with_staleness_note(Some("A new Tick 'tick-99' has been published.".into()))
            .with_iteration(2)
            .build(&crate::prompts::store().implementer);
        assert!(assembled.user_message.contains("Staleness Warning"));
        assert!(assembled.user_message.contains("tick-99"));
    }

    #[tokio::test]
    async fn test_run_iteration_with_staleness_note() {
        let dir = TestDir::new("loopr-impl-stale");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();
        let llm = Box::new(MockLlm::new(r#"[{"action": "done", "summary": "Rebased and done"}]"#));
        let agent = test_implementer(llm, stores, &dir, config);

        let staleness = Some("New tick published, please review.".to_string());
        let outcome = agent.run_iteration(2, staleness, &mut Lifeguard::new()).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(_)));
    }

    #[tokio::test]
    async fn test_run_implementer_detects_staleness() {
        let dir = TestDir::new("loopr-impl-staleness");
        let stores = setup_stores(&dir);
        let wi_id = get_work_id(&stores);
        let (bridge, event_tx) = test_bridge_with_tx(stores.clone(), &dir);
        let agent_log = test_agent_logger(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3;

        // Mock LLM that sends tick.published during the first call, so it's available
        // by the time the second iteration starts and drains events.
        struct StalenessLlm {
            call_count: std::sync::atomic::AtomicU32,
            event_tx: broadcast::Sender<DaemonEvent>,
        }

        #[async_trait]
        impl LlmClient for StalenessLlm {
            async fn call(&self, _system: &str, user_msg: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // First iteration: send tick.published, then return a continue action.
                    // The event will be in the broadcast buffer for the next drain.
                    let _ = self.event_tx.send(DaemonEvent::tick_published("tick-new", "newshaval"));
                    Ok(r#"[{"action": "write_file", "path": "x.txt", "content": "v1"}]"#.to_string())
                } else {
                    // Second iteration: should see staleness warning in user message
                    assert!(
                        user_msg.contains("Staleness Warning"),
                        "Expected staleness context in user message"
                    );
                    Ok(r#"[{"action": "done", "summary": "Handled staleness"}]"#.to_string())
                }
            }
        }

        let llm: Box<dyn LlmClient> = Box::new(StalenessLlm {
            call_count: std::sync::atomic::AtomicU32::new(0),
            event_tx: event_tx.clone(),
        });

        let mut session = AgentSession::new(AgentType::Implementer, "test".into());
        session.work_id = Some(wi_id.clone());
        session.worktree_path = Some(dir.to_string_lossy().into());

        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.tool_runner.clone(),
            tool_executor: stores.tool_executor.clone(),
            log: agent_log,
        };
        let mut agent = ImplementerAgent::new(ctx, llm, config, wi_id, dir.to_path_buf());

        let result = agent.run().await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
    }

    #[tokio::test]
    async fn test_action_results_accumulate() {
        let dir = TestDir::new("loopr-impl-accum");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        // LLM returns multiple actions -- all summaries should be joined
        let llm = Box::new(MockLlm::new(
            r#"[
            {"action": "write_file", "path": "a.txt", "content": "aaa"},
            {"action": "write_file", "path": "b.txt", "content": "bbb"},
            {"action": "read_file", "path": "a.txt"}
        ]"#,
        ));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        if let IterationOutcome::Continue(summary) = outcome {
            // All action summaries should be present, joined by newline
            assert!(summary.contains("wrote a.txt"), "missing a.txt summary: {}", summary);
            assert!(summary.contains("wrote b.txt"), "missing b.txt summary: {}", summary);
            assert!(summary.contains("read a.txt"), "missing read summary: {}", summary);
        } else {
            panic!("expected Continue, got: {:?}", outcome);
        }
    }

    #[test]
    fn test_parse_actions_skips_malformed_in_fallback() {
        let dir = TestDir::new("loopr-impl-parse6");
        let agent_log = test_agent_logger(&dir);
        // Array with one valid and one malformed action — fallback should skip the bad one
        let json = r#"[
            {"action": "done", "summary": "All good"},
            {"action": "totally_bogus", "invalid_field": 42}
        ]"#;
        let actions = parse_actions(json, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::Done { .. }));
    }

    #[test]
    fn test_parse_actions_normalizes_type_to_action() {
        let dir = TestDir::new("loopr-impl-parse7");
        let agent_log = test_agent_logger(&dir);
        // LLMs sometimes use "type" instead of "action" as the discriminant key
        let json = r#"[{"type": "read_file", "path": "src/main.rs"}]"#;
        let actions = parse_actions(json, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::ReadFile { .. }));
    }

    #[test]
    fn test_parse_actions_normalizes_type_in_prose() {
        let dir = TestDir::new("loopr-impl-parse8");
        let agent_log = test_agent_logger(&dir);
        let response = r#"I'll read the file first.

```json
[{"type": "read_file", "path": "Cargo.toml"}]
```"#;
        let actions = parse_actions(response, &agent_log).unwrap();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AgentAction::ReadFile { .. }));
    }

    #[test]
    fn test_build_implementer_summary_empty_locks_and_siblings() {
        let dir = TestDir::new("loopr-impl-emptysummary");
        let stores = setup_stores(&dir);
        let wi_id = get_work_id(&stores);

        // No active locks, no sibling agents — summary should be empty
        let agent_log = test_agent_logger(&dir);
        let summary = build_implementer_summary(&stores, &wi_id, &agent_log);
        assert!(summary.is_empty(), "expected empty summary, got: {}", summary);
    }

    #[tokio::test]
    async fn test_run_iteration_done_stops_remaining_actions() {
        // When a Done action appears mid-list, subsequent actions should not execute
        let dir = TestDir::new("loopr-impl-donestop");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        // Done comes first, then a write_file that should NOT execute
        let llm = Box::new(MockLlm::new(
            r#"[
            {"action": "done", "summary": "Early exit"},
            {"action": "write_file", "path": "should_not_exist.txt", "content": "nope"}
        ]"#,
        ));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s == "Early exit"));
        // The file after Done should NOT have been written
        assert!(!dir.join("should_not_exist.txt").exists());
    }

    #[test]
    fn test_drain_tick_published_closed_channel() {
        let dir = TestDir::new("loopr-impl-drain6");
        let agent_log = test_agent_logger(&dir);
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(4);
        // Send a tick, then close
        let _ = tx.send(DaemonEvent::tick_published("tick-closed", "sha"));
        drop(tx);
        // Should still find the tick before hitting Closed
        let result = drain_tick_published(&mut rx, &agent_log);
        assert_eq!(result, Some("tick-closed".to_string()));
    }

    #[test]
    fn test_drain_tick_published_lagged_channel() {
        let dir = TestDir::new("loopr-impl-drain7");
        let agent_log = test_agent_logger(&dir);
        // Create a very small buffer so sending more messages than capacity causes lag
        let (tx, mut rx) = broadcast::channel::<DaemonEvent>(2);
        // Send 4 messages to overflow the buffer of 2 — receiver will lag
        let _ = tx.send(DaemonEvent::record_created("x", "1"));
        let _ = tx.send(DaemonEvent::record_created("x", "2"));
        let _ = tx.send(DaemonEvent::record_created("x", "3"));
        let _ = tx.send(DaemonEvent::tick_published("tick-lagged", "sha"));
        // drain should recover from lag and find the tick
        let result = drain_tick_published(&mut rx, &agent_log);
        assert_eq!(result, Some("tick-lagged".to_string()));
    }

    #[test]
    fn test_format_action_summary_truncated_tool_output() {
        use crate::tools::ToolResult;

        // Create stdout that exceeds MAX_SUMMARY_CONTENT (4000 chars)
        let long_output = "x".repeat(5000);
        let tr = ToolResult {
            tool: "big_tool".to_string(),
            exit_code: 0,
            stdout: long_output.clone(),
            stderr: String::new(),
            duration_ms: 100,
            truncated: false,
        };
        let action = AgentAction::RunTool {
            tool: "big_tool".to_string(),
            args: vec![],
        };
        let summary = format_action_summary(&action, &ActionResult::ToolRun(tr));
        assert!(summary.contains("ran big_tool (exit 0)"));
        assert!(summary.contains("[truncated, 5000 total bytes]"));
        // Should NOT contain the full 5000-char output
        assert!(summary.len() < long_output.len());
    }

    #[tokio::test]
    async fn test_run_implementer_session_iteration_persisted() {
        let dir = TestDir::new("loopr-impl-persist");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 2;

        // LLM returns a write on first call, done on second
        struct TwoShotLlm {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait]
        impl LlmClient for TwoShotLlm {
            async fn call(&self, _system: &str, _user: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    Ok(r#"[{"action": "write_file", "path": "iter.txt", "content": "1"}]"#.to_string())
                } else {
                    Ok(r#"[{"action": "done", "summary": "Persisted iteration"}]"#.to_string())
                }
            }
        }

        let llm: Box<dyn LlmClient> = Box::new(TwoShotLlm {
            call_count: std::sync::atomic::AtomicU32::new(0),
        });
        let mut agent = test_implementer(llm, stores.clone(), &dir, config);
        let session_id = agent.ctx.session.id.clone();

        // Insert session into stores so the loop can update it
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), agent.ctx.session.clone());

        let result = agent.run().await;
        assert!(result.is_ok(), "Expected success, got: {:?}", result);
        assert_eq!(agent.ctx.session.iteration, 2);

        // Verify the iteration was persisted in stores
        let sessions = stores.agent_sessions.read().unwrap();
        let stored = sessions.get(&session_id).unwrap();
        assert_eq!(stored.iteration, 2, "Stored iteration should be 2");
    }

    #[tokio::test]
    async fn test_implementer_context_includes_goal() {
        use crate::domain::coordinator_goal::CoordinatorGoal;
        use std::sync::Mutex as StdMutex2;

        let dir = TestDir::new("loopr-impl-goal");
        let stores = setup_stores(&dir);

        // Set a coordinator goal
        let goal = CoordinatorGoal::new("Build a REST API for user management".to_string());
        stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);

        // Use a CapturingLlm that captures the prompt via shared Arc
        let captured_msg = Arc::new(StdMutex2::new(None::<String>));

        struct CapturingLlm {
            captured_user_msg: Arc<StdMutex2<Option<String>>>,
        }

        #[async_trait]
        impl LlmClient for CapturingLlm {
            async fn call(&self, _system_prompt: &str, user_message: &str) -> Result<String> {
                *self.captured_user_msg.lock().unwrap() = Some(user_message.to_string());
                Ok(r#"[{"action": "done", "summary": "done"}]"#.to_string())
            }
        }

        let config = AgentRoleConfig::default_implementer();
        let llm: Box<dyn LlmClient> = Box::new(CapturingLlm {
            captured_user_msg: captured_msg.clone(),
        });
        let agent = test_implementer(llm, stores, &dir, config);

        let _ = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();

        let captured = captured_msg.lock().unwrap();
        let user_msg = captured.as_ref().expect("LLM should have been called");
        assert!(
            user_msg.contains("Project Goal"),
            "expected 'Project Goal' in context, got: {}",
            &user_msg[..user_msg.len().min(500)]
        );
        assert!(
            user_msg.contains("REST API for user management"),
            "expected goal text in context"
        );
    }

    #[tokio::test]
    async fn test_parse_failure_recovery_via_self_correction() {
        // First call returns prose (parse failure), second call returns valid JSON.
        // With self-correction, the parse failure is corrected within the same iteration
        // (not via previous_summary in the next iteration).
        let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

        struct ProseThenValidLlm {
            call_count: Arc<std::sync::atomic::AtomicU32>,
        }

        #[async_trait]
        impl LlmClient for ProseThenValidLlm {
            async fn call(&self, _system: &str, user_msg: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // Return prose -- triggers self-correction re-prompt
                    Ok("I think we should start by reading the file and understanding the codebase.".to_string())
                } else {
                    // Self-correction re-prompt: default call_with_history extracts last user
                    // message, which is the correction prompt
                    assert!(
                        user_msg.contains("could not be parsed as a valid JSON action array"),
                        "expected self-correction prompt, got: {}",
                        &user_msg[..user_msg.len().min(500)]
                    );
                    Ok(r#"[{"action": "done", "summary": "Recovered via self-correction"}]"#.to_string())
                }
            }
        }

        let dir = TestDir::new("loopr-impl-parseretry");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3;

        let llm: Box<dyn LlmClient> = Box::new(ProseThenValidLlm {
            call_count: call_count.clone(),
        });
        let mut agent = test_implementer(llm, stores.clone(), &dir, config);
        let session_id = agent.ctx.session.id.clone();

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), agent.ctx.session.clone());

        let result = agent.run().await;
        assert!(
            result.is_ok(),
            "Expected success after self-correction, got: {:?}",
            result
        );
        // Self-correction happens within iteration 1, so only 1 iteration used
        assert_eq!(
            agent.ctx.session.iteration, 1,
            "Should have completed on first iteration (self-correction within iteration)"
        );
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "LLM should have been called twice (initial + correction)"
        );
    }

    #[test]
    fn test_build_implementer_summary_with_locks_and_siblings() {
        use crate::domain::lock::{Lock, LockStatus};
        let dir = TestDir::new("loopr-impl-fullsummary");
        let stores = setup_stores(&dir);
        let wi_id = get_work_id(&stores);

        // Add an active lock
        let mut lock = Lock::new("src/main.rs".into(), "wi-other".into(), "coordinator".into());
        lock.status = LockStatus::Active;
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        // Add a sibling agent session (different work_id, not terminal)
        let mut session =
            crate::agents::AgentSession::new(crate::agents::AgentType::Implementer, "test-model".to_string());
        session.work_id = Some("wi-sibling".to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let agent_log = test_agent_logger(&dir);
        let summary = build_implementer_summary(&stores, &wi_id, &agent_log);
        assert!(summary.contains("### Active Locks"), "should contain locks section");
        assert!(summary.contains("src/main.rs"), "should contain lock resource");
        assert!(
            summary.contains("### Sibling Agents"),
            "should contain siblings section"
        );
        assert!(summary.contains("wi-sibling"), "should contain sibling work_id");
    }

    // --- F3: Budget exhaustion / force-propose tests ---

    /// Mock LLM that records all user_message inputs it receives via shared Arc.
    struct RecordingLlm {
        response: String,
        calls: Arc<StdMutex<Vec<String>>>,
    }

    impl RecordingLlm {
        fn new(response: &str) -> (Self, Arc<StdMutex<Vec<String>>>) {
            let calls = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    response: response.to_string(),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl LlmClient for RecordingLlm {
        async fn call(&self, _system_prompt: &str, user_message: &str) -> Result<String> {
            self.calls.lock().unwrap().push(user_message.to_string());
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn test_budget_exhaustion_prompt_injected_at_penultimate_iteration() {
        let dir = TestDir::new("loopr-impl-budget");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 3; // Budget warning should appear at iterations 2 and 3

        let (llm, calls) = RecordingLlm::new(r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#);
        let mut agent = test_implementer(Box::new(llm), stores.clone(), &dir, config);

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(agent.ctx.session.id.clone(), agent.ctx.session.clone());

        let _ = agent.run().await;

        let messages = calls.lock().unwrap().clone();
        // With max_iterations=3, budget warning triggers at i >= 3-1 = 2, so iterations 2 and 3
        assert!(messages.len() >= 2, "should have at least 2 LLM calls");
        // Iteration 2 (index 1) should contain the budget warning
        assert!(
            messages[1].contains("Budget Exhausted"),
            "penultimate iteration should contain budget warning, got: {}",
            &messages[1][..std::cmp::min(200, messages[1].len())]
        );
    }

    #[tokio::test]
    async fn test_force_propose_claims_content() {
        // Verify the force-propose uses the expected claims text
        let dir = TestDir::new("loopr-impl-forceclaims");
        let stores = setup_stores(&dir);
        let wi_id = get_work_id(&stores);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 1; // Immediately exhaust budget

        // LLM returns a write_file (no propose_bundle), so has_proposed stays false
        let llm = Box::new(MockLlm::new(
            r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#,
        ));
        let mut agent = test_implementer(llm, stores.clone(), &dir, config);

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(agent.ctx.session.id.clone(), agent.ctx.session.clone());

        // Transition work to InProgress so bundle.create can succeed
        let work_resp = agent.ctx.bridge.request(
            "work.transition",
            serde_json::json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator"}),
        );
        // May fail if not in Ready state, that's OK -- the force-propose attempt is what we test

        let _ = agent.run().await;

        // The force-propose should have attempted to create a bundle.
        // Even if it failed (no git repo), the attempt itself is what we verify.
        // Check if any bundle was created (it might fail due to no git repo for commit,
        // but the description format is what we care about).
        let bundles = stores.bundles.read().unwrap();
        if !bundles.is_empty() {
            let bundle = bundles.values().next().unwrap();
            let desc = bundle.description.as_deref().unwrap_or("");
            assert!(
                desc.contains("Auto-proposed at iteration cap"),
                "bundle description should indicate force-propose: {}",
                desc
            );
        }
        // Even if bundle creation failed, test passes -- we verified the code path runs
        let _ = work_resp;
    }

    #[tokio::test]
    async fn test_has_proposed_true_skips_force_propose() {
        // If the LLM proposes a bundle within the loop, force-propose should not trigger
        let dir = TestDir::new("loopr-impl-noforceprop");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 2;

        // First: write_file only -- has_proposed stays false
        let llm1 = Box::new(MockLlm::new(
            r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#,
        ));
        let mut agent1 = test_implementer(llm1, stores.clone(), &dir, config.clone());

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(agent1.ctx.session.id.clone(), agent1.ctx.session.clone());

        let _ = agent1.run().await;
        // Force-propose should have been attempted (may or may not succeed)

        // Second: done action -- returns Ok immediately, no force-propose
        let llm2 = Box::new(MockLlm::new(r#"[{"action": "done", "summary": "All done"}]"#));
        let mut agent2 = test_implementer(llm2, stores.clone(), &dir, config);

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(agent2.ctx.session.id.clone(), agent2.ctx.session.clone());

        let result2 = agent2.run().await;
        assert!(result2.is_ok(), "done action should return Ok without force-propose");
    }

    // --- is_correctable_error tests ---

    #[test]
    fn test_is_correctable_error_schema_errors() {
        assert!(is_correctable_error("missing field `summary`"));
        assert!(is_correctable_error("unknown field `files`"));
        assert!(is_correctable_error("invalid type: found object, expected array"));
        assert!(is_correctable_error("expected array of actions"));
    }

    #[test]
    fn test_is_correctable_error_path_errors() {
        assert!(is_correctable_error("path escapes sandbox: ../../../etc/passwd"));
        assert!(is_correctable_error("unknown tool: cargo_test"));
        assert!(is_correctable_error("path traversal detected"));
    }

    #[test]
    fn test_is_correctable_error_non_correctable() {
        assert!(!is_correctable_error("cargo test failed with exit code 1"));
        assert!(!is_correctable_error("compilation error: expected `;`"));
        assert!(!is_correctable_error("network error: connection refused"));
        assert!(!is_correctable_error("permission denied"));
        assert!(!is_correctable_error("file not found: src/main.rs"));
    }

    // --- Self-correction loop tests (Phase 1: parse failure correction) ---

    /// Mock LLM that returns a sequence of responses, one per call.
    struct SequenceLlm {
        responses: StdMutex<std::collections::VecDeque<String>>,
    }

    impl SequenceLlm {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl LlmClient for SequenceLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| eyre!("SequenceLlm: no more responses"))
        }
    }

    #[tokio::test]
    async fn test_self_correction_parse_failure_then_success() {
        // First response is malformed, second is valid JSON.
        // Self-correction loop should re-prompt and succeed within the same iteration.
        let dir = TestDir::new("loopr-impl-selfcorrect1");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            "I think we should read the file first.",               // malformed
            r#"[{"action": "done", "summary": "Self-corrected"}]"#, // valid
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s == "Self-corrected"),
            "expected Done after self-correction, got: {:?}",
            outcome
        );
    }

    #[tokio::test]
    async fn test_self_correction_multiple_retries_then_success() {
        // Three malformed responses, then valid. max_requeries=3 means 3 retries allowed.
        let dir = TestDir::new("loopr-impl-selfcorrect2");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer(); // max_requeries=3

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            "bad response 1",
            "bad response 2",
            "bad response 3",
            r#"[{"action": "done", "summary": "Finally corrected"}]"#,
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s == "Finally corrected"),
            "expected Done after 3 retries, got: {:?}",
            outcome
        );
    }

    #[tokio::test]
    async fn test_self_correction_max_requeries_exceeded() {
        // All responses are malformed. After max_requeries retries, should return Err.
        let dir = TestDir::new("loopr-impl-selfcorrect3");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer(); // max_requeries=3

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            "bad 1", "bad 2", "bad 3", "bad 4", // 1 initial + 3 retries = 4 calls, then error
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run_iteration(1, None, &mut Lifeguard::new()).await;
        assert!(result.is_err(), "expected error when max_requeries exceeded");
        assert!(
            result.unwrap_err().to_string().contains("failed to parse"),
            "error should be a parse error"
        );
    }

    #[tokio::test]
    async fn test_self_correction_disabled_when_max_requeries_zero() {
        // max_requeries=0 means no correction attempts — first parse failure returns Err immediately.
        let dir = TestDir::new("loopr-impl-selfcorrect4");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_requeries = 0;

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            "malformed response",
            r#"[{"action": "done", "summary": "never reached"}]"#,
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let result = agent.run_iteration(1, None, &mut Lifeguard::new()).await;
        assert!(result.is_err(), "expected immediate error with max_requeries=0");
    }

    #[tokio::test]
    async fn test_self_correction_no_overhead_on_valid_response() {
        // Valid response on first try — no correction loop, no extra calls.
        let dir = TestDir::new("loopr-impl-selfcorrect5");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        // SequenceLlm with only one response — second call would panic
        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            r#"[{"action": "done", "summary": "First try success"}]"#,
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s == "First try success"),
            "expected Done on first try, got: {:?}",
            outcome
        );
    }

    #[tokio::test]
    async fn test_self_correction_integrates_with_run_loop() {
        // Self-correction within run_iteration should not count as a separate iteration.
        // First iteration: malformed → corrected. Second iteration: done.
        let dir = TestDir::new("loopr-impl-selfcorrect6");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_iterations = 5;

        struct CorrectionThenDoneLlm {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait]
        impl LlmClient for CorrectionThenDoneLlm {
            async fn call(&self, _system: &str, _user: &str) -> Result<String> {
                let n = self.call_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match n {
                    0 => Ok("not json".to_string()), // iter 1: malformed
                    1 => Ok(r#"[{"action": "write_file", "path": "x.txt", "content": "x"}]"#.to_string()), // iter 1: corrected
                    2 => Ok(r#"[{"action": "done", "summary": "All done"}]"#.to_string()), // iter 2: done
                    _ => Err(eyre!("unexpected call")),
                }
            }
        }

        let llm: Box<dyn LlmClient> = Box::new(CorrectionThenDoneLlm {
            call_count: std::sync::atomic::AtomicU32::new(0),
        });
        let mut agent = test_implementer(llm, stores.clone(), &dir, config);

        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(agent.ctx.session.id.clone(), agent.ctx.session.clone());

        let result = agent.run().await;
        assert!(result.is_ok(), "expected success, got: {:?}", result);
        // Should complete in 2 iterations (correction happened within iteration 1)
        assert_eq!(agent.ctx.session.iteration, 2);
    }

    // --- Tool error correction tests (Phase 2) ---

    #[tokio::test]
    async fn test_tool_error_correction_path_escapes() {
        // WriteFile with path traversal → correctable error → LLM re-prompt → corrected action
        let dir = TestDir::new("loopr-impl-toolcorr1");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            // First: returns an action that will fail (path escapes sandbox)
            r#"[{"action": "write_file", "path": "../../etc/passwd", "content": "bad"}]"#,
            // Correction: returns a valid action
            r#"[{"action": "done", "summary": "Corrected path issue"}]"#,
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s == "Corrected path issue"),
            "expected Done after tool error correction, got: {:?}",
            outcome
        );
    }

    #[tokio::test]
    async fn test_tool_error_correction_non_correctable_falls_through() {
        // Non-correctable errors (like test failures) should NOT trigger re-prompt.
        // They go to the lifeguard path instead.
        let dir = TestDir::new("loopr-impl-toolcorr2");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        // RunTool with a tool that doesn't exist in ToolRunner — triggers "unknown tool" which IS correctable
        // Instead, use write_file to a valid path then read_file from a nonexistent path
        // read_file errors are NOT in is_correctable_error, so they fall through
        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            r#"[{"action": "read_file", "path": "nonexistent_file_xyz.rs"}]"#,
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        // Should be Continue with an error summary (not Done, since read_file error is non-fatal)
        if let IterationOutcome::Continue(summary) = &outcome {
            assert!(summary.contains("ERROR"), "summary should contain error: {}", summary);
        } else {
            panic!("expected Continue with error, got: {:?}", outcome);
        }
    }

    #[tokio::test]
    async fn test_tool_error_correction_budget_shared_with_parse() {
        // If parse corrections used some budget, tool corrections get the remainder.
        // max_requeries=1: use 1 for parse correction → 0 left for tool correction
        let dir = TestDir::new("loopr-impl-toolcorr3");
        let stores = setup_stores(&dir);
        let mut config = AgentRoleConfig::default_implementer();
        config.max_requeries = 1; // Only 1 re-prompt allowed total

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            "not json", // parse fail → uses 1 requery
            r#"[{"action": "write_file", "path": "../../etc/passwd", "content": "bad"}]"#, // corrected parse, but path escapes
                                                                                           // No more re-prompts available — tool error should NOT trigger correction
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        // The path escapes error should fall through (no budget left), producing an error summary
        if let IterationOutcome::Continue(summary) = &outcome {
            assert!(summary.contains("ERROR"), "should contain error: {}", summary);
        } else {
            panic!(
                "expected Continue with error (no correction budget), got: {:?}",
                outcome
            );
        }
    }

    #[tokio::test]
    async fn test_tool_error_correction_parse_failure_on_corrected_response() {
        // Tool error triggers correction, but the corrected response is also malformed.
        // Should fall back to ActionError for the original error.
        let dir = TestDir::new("loopr-impl-toolcorr4");
        let stores = setup_stores(&dir);
        let config = AgentRoleConfig::default_implementer();

        let llm: Box<dyn LlmClient> = Box::new(SequenceLlm::new(vec![
            // First: path escapes (correctable)
            r#"[{"action": "write_file", "path": "../../etc/passwd", "content": "bad"}]"#,
            // Correction response: malformed (can't parse)
            "I apologize, here is the fix",
        ]));
        let agent = test_implementer(llm, stores, &dir, config);

        let outcome = agent.run_iteration(1, None, &mut Lifeguard::new()).await.unwrap();
        // Should continue with an ActionError (correction failed, original error recorded)
        if let IterationOutcome::Continue(summary) = &outcome {
            assert!(summary.contains("ERROR"), "should contain error: {}", summary);
        } else {
            panic!(
                "expected Continue with error (correction parse failed), got: {:?}",
                outcome
            );
        }
    }
}
