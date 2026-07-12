//! Prompt assembly for every LLM-calling stage.
//!
//! `ContextBuilder` is the single entry point. Callers pass typed
//! records (`Work`, tool schemas, iteration history, state summary)
//! and receive `AssembledContext` — a ready-to-send system prompt +
//! user message plus a token estimate for budgeting.
//!
//! This crate does NOT depend on `llm`. `context` produces messages;
//! `llm` consumes them. Tool schemas come from `tools::ToolSchema`,
//! not `llm::ToolSchema`, so the assembly layer stays independent of
//! whichever LLM backend is in use.

mod history;
mod implementer;
mod loader;
mod reviewer;

pub use history::trim_history;

pub use implementer::InlineContextBuilder;
pub use loader::{BAKED_PROMPTS, PromptError, PromptLoader, baked_prompts};

use std::path::Path;

use domain::{Bundle, Work};
use llm::Message;
use tools::ToolSchema;

/// Wrap `content` in a code fence whose backtick run is one longer than the
/// longest backtick run inside `content` (floor of 3), so untrusted content
/// (a diff, file contents, or executed-check output) cannot close its own
/// fence and escape into instruction position (Phase-5 finding 9). Shared by
/// the reviewer prompt's evidence sections and `agents::reviewer`'s executed-
/// check evidence block (Phase 10 of `docs/design/2026-07-11-verified-swarm.md`).
pub fn dynamic_fence(content: &str) -> String {
    let longest = longest_backtick_run(content);
    let fence = "`".repeat((longest + 1).max(3));
    let mut out = String::with_capacity(content.len() + 2 * fence.len() + 2);
    out.push_str(&fence);
    out.push('\n');
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
    out
}

fn longest_backtick_run(s: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in s.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// The output of a successful context assembly. Ready to hand to
/// `LlmClient::complete_free` (system + messages).
///
/// `messages` carries the full turn sequence for the current call:
/// - Single-turn callers (Implementer, Reviewer): one `Message::user(...)`.
/// - Multi-turn callers (Director, Researcher): `[trimmed_history...,
///   fresh_state_summary]` — state summary is always last.
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub system_prompt: String,
    pub messages: Vec<Message>,
    pub token_estimate: usize,
}

impl AssembledContext {
    /// Returns the text content of the first message if it is a
    /// single-`Text`-block user message. Used by the Implementer's
    /// transcript writer. Returns `None` if `messages` is empty or
    /// the first content block is not `Text`.
    pub fn first_user_text(&self) -> Option<&str> {
        use llm::MessageContent;
        self.messages.first().and_then(|m| {
            if let Some(MessageContent::Text { text }) = m.content.first() {
                Some(text.as_str())
            } else {
                None
            }
        })
    }
}

/// Cross-iteration state the Implementer carries. Populated by the
/// reactor on a retry: if the prior Bundle was rejected, the
/// reason is threaded through so the LLM sees why.
#[derive(Debug, Clone, Default)]
pub struct StateSummary {
    pub rejected_bundle_reason: Option<String>,
}

/// Per-iteration summary. `actions_summary` is capped at
/// `ITERATION_SUMMARY_CAP` chars (4000) to prevent context blow-up
/// across long runs.
#[derive(Debug, Clone)]
pub struct IterationSummary {
    pub iteration: u32,
    pub actions_summary: String,
}

/// Maximum characters retained from any single `IterationSummary`.
/// Locked by design doc Phase 2 requirement; tested below.
pub const ITERATION_SUMMARY_CAP: usize = 4000;

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("context assembly failed: {0}")]
    Assembly(String),
    #[error(transparent)]
    Prompt(#[from] PromptError),
}

/// State snapshot passed to `build_for_director`. All fields are
/// plain strings so `context` does not need to import domain FSM
/// types. The caller (`agents::run_director`) converts domain records
/// to this display-oriented shape before calling in.
#[derive(Debug, Clone, Default)]
pub struct DirectorState {
    pub plan_id: String,
    pub works: Vec<WorkLine>,
    pub bundles: Vec<BundleLine>,
    pub blocked_reason: Option<String>,
    /// Escalation mode label (`Normal` / `Conservative` / `NeedsOperator`).
    /// Rendered as `**Director mode:**` near the top of the user prompt;
    /// the LLM matches this against the system-prompt's
    /// `## Mode-Aware Recovery` subsections. Phase 6 of
    /// `docs/design/2026-05-09-director-phase-2.md`. Default `"Normal"`
    /// so callers that pre-date Phase 6 still render a sensible label.
    pub mode: String,
    /// Operator-submitted note bodies that arrived since the previous
    /// Director iteration. Rendered as a `## Operator Notes` section
    /// in the user prompt. Phase 9 of
    /// `docs/design/2026-05-09-director-phase-2.md`. Empty by default
    /// so callers that pre-date Phase 9 still produce a valid prompt.
    /// The Director loop reads `NotesStore::list_unread_notes_for_plan`
    /// each iteration, populates this field with the raw bodies, and
    /// (after a successful LLM round-trip) marks the notes read so the
    /// next iteration's vector is empty unless a new note arrived.
    pub operator_notes: Vec<String>,
    /// Operator-tunable retry budget (`agents.director.max-work-attempts`,
    /// default 3) rendered into the user prompt's retry guidance. The
    /// Director loop sets this from `deps.config.max_work_attempts` each
    /// iteration; `Default` (0) is only the test-only `build_director_state`
    /// baseline, where the caller overrides it.
    pub max_work_attempts: u32,
}

/// One Work row in a `DirectorState`.
#[derive(Debug, Clone)]
pub struct WorkLine {
    pub id: String,
    pub title: String,
    /// Stringified status ("Pending", "InProgress", etc.). Stringified
    /// by the caller so `context` does not import `domain::WorkStatus`.
    pub status: String,
    pub attempt_count: u32,
}

/// One Bundle row in a `DirectorState`.
#[derive(Debug, Clone)]
pub struct BundleLine {
    pub id: String,
    pub work_id: String,
    pub status: String,
}

/// Query passed to `build_for_researcher`.
#[derive(Debug, Clone)]
pub struct ResearchQuery {
    pub question: String,
    pub context_hints: Vec<String>,
}

/// Single entry point for prompt assembly. One method per role/stage.
pub trait ContextBuilder: Send + Sync {
    /// Assemble the Implementer's system + user prompts for one
    /// iteration. `tool_schemas` comes from `tools::ToolSchema`
    /// (not `llm::ToolSchema`) — the assembly layer renders the
    /// schemas into the prompt; the LLM transport layer never
    /// sees raw templates.
    fn build_for_implementer(
        &self,
        work: &Work,
        worktree_path: &Path,
        tool_schemas: &[ToolSchema],
        history: &[IterationSummary],
        state: &StateSummary,
        iteration: u32,
    ) -> Result<AssembledContext, ContextError>;

    /// Assemble the Reviewer's system + user prompts for a single
    /// turn. `diff: &str` is pre-extracted (header stripped,
    /// truncated); `noop_files: Option<&[(String, String)]>` is
    /// pre-read for noop Bundles with aggregate + per-file caps
    /// already applied. `None` renders the `Diff` section; `Some`
    /// renders the `File Contents` section. No I/O happens here
    /// (per `context/CLAUDE.md`'s pure-prompt-assembly rule); all
    /// git-show and file-read calls live upstream in
    /// `agents::reviewer`.
    fn build_for_reviewer(
        &self,
        bundle: &Bundle,
        work: &Work,
        diff: &str,
        noop_files: Option<&[(String, String)]>,
    ) -> Result<AssembledContext, ContextError>;

    /// Assemble the Director's context for one turn. Message order:
    ///   `[trimmed prior history] + [fresh state summary (current user turn)]`
    ///
    /// The state summary is the LAST message — the one the model responds to.
    /// `history` must already alternate user→assistant; the caller maintains
    /// this invariant before calling in.
    fn build_for_director(
        &self,
        state: &DirectorState,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError>;

    /// Assemble the Researcher's context for one turn. Same ordering
    /// contract as `build_for_director`: current query last.
    fn build_for_researcher(
        &self,
        query: &ResearchQuery,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError>;
}

/// Forwarding impl for `Arc<C>` so daemon code can hand an
/// `Arc`-shared builder to `agents::implementer::Deps` without
/// cloning or unwrapping.
impl<C: ContextBuilder + ?Sized> ContextBuilder for std::sync::Arc<C> {
    fn build_for_implementer(
        &self,
        work: &Work,
        worktree_path: &Path,
        tool_schemas: &[ToolSchema],
        history: &[IterationSummary],
        state: &StateSummary,
        iteration: u32,
    ) -> Result<AssembledContext, ContextError> {
        (**self).build_for_implementer(work, worktree_path, tool_schemas, history, state, iteration)
    }

    fn build_for_reviewer(
        &self,
        bundle: &Bundle,
        work: &Work,
        diff: &str,
        noop_files: Option<&[(String, String)]>,
    ) -> Result<AssembledContext, ContextError> {
        (**self).build_for_reviewer(bundle, work, diff, noop_files)
    }

    fn build_for_director(
        &self,
        state: &DirectorState,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError> {
        (**self).build_for_director(state, history, token_budget)
    }

    fn build_for_researcher(
        &self,
        query: &ResearchQuery,
        history: &[Message],
        token_budget: usize,
    ) -> Result<AssembledContext, ContextError> {
        (**self).build_for_researcher(query, history, token_budget)
    }
}
