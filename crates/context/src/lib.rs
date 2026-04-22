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

pub use implementer::InlineContextBuilder;

use std::path::Path;

use domain::Work;
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
/// coordinator on a retry: if the prior Bundle was rejected, the
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
}

/// Single entry point for prompt assembly. One method per role/stage
/// (Stage 7 ships the Implementer method; Reviewer/Director land
/// in Stage 8+ without re-shaping the trait).
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
}
