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

mod implementer;
mod loader;
mod reviewer;

pub use implementer::InlineContextBuilder;
pub use loader::{BAKED_PROMPTS, PromptError, PromptLoader, baked_prompts};

use std::path::Path;

use domain::{Bundle, Work};
use tools::ToolSchema;

/// The output of a successful context assembly. Ready to hand to
/// `LlmClient::complete_free` (system + user-as-first-message) or to
/// `LlmClient::complete_with_tool` (system + user).
#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub system_prompt: String,
    pub user_message: String,
    pub token_estimate: usize,
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

/// Single entry point for prompt assembly. One method per role/stage.
/// Stage 7 added `build_for_implementer`; Stage 8 (this trait
/// extension) adds `build_for_reviewer`.
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
}
