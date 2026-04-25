//! `InlineContextBuilder`: the Stage-7 `ContextBuilder` impl.
//!
//! Inline string rendering — no handlebars, no `.pmt` file loading.
//!
//! TODO(pmt-migration): The three-layer template-override chain
//! documented in `crates/context/CLAUDE.md` (`.loopr/prompts/` ->
//! `~/.config/loopr/prompts/` -> `include_dir!()`-baked defaults) plus
//! handlebars-rust is the committed end state. It lands in a separate
//! design doc (see the Open Questions section of
//! `docs/design/2026-04-22-reviewer.md`). Both the Implementer's
//! `render_system_prompt` constant and the Reviewer's
//! `REVIEWER_SYSTEM_PROMPT` migrate together. Not forgotten.

use std::fmt::Write;
use std::path::Path;

use tracing::instrument;

use domain::{Bundle, Work};
use tools::ToolSchema;

use crate::reviewer::render_reviewer_user_message;
use crate::{
    AssembledContext, ContextBuilder, ContextError, ITERATION_SUMMARY_CAP, IterationSummary, REVIEWER_SYSTEM_PROMPT,
    StateSummary,
};

/// Rough tokens-per-char estimate (English text averages ~4 chars
/// per token; we use 4 as the divisor for a generous-side estimate
/// that errs toward undercounting).
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// Inline string-rendering `ContextBuilder`. Produces deterministic
/// output; takes nothing from disk; thread-safe by being stateless.
#[derive(Debug, Default, Clone, Copy)]
pub struct InlineContextBuilder;

impl InlineContextBuilder {
    pub const fn new() -> Self {
        Self
    }
}

impl ContextBuilder for InlineContextBuilder {
    #[instrument(
        name = "context.build_for_implementer",
        level = "debug",
        skip_all,
        fields(
            role = "implementer",
            work_id = %work.id,
            iteration = iteration,
            history_len = history.len(),
            tool_count = tool_schemas.len(),
            system_chars = tracing::field::Empty,
            user_chars = tracing::field::Empty,
            token_estimate = tracing::field::Empty,
        ),
        err,
    )]
    fn build_for_implementer(
        &self,
        work: &Work,
        worktree_path: &Path,
        tool_schemas: &[ToolSchema],
        history: &[IterationSummary],
        state: &StateSummary,
        iteration: u32,
    ) -> Result<AssembledContext, ContextError> {
        let system_prompt = render_system_prompt(tool_schemas);
        let user_message = render_user_message(work, worktree_path, history, state, iteration);
        let token_estimate = (system_prompt.len() + user_message.len()) / CHARS_PER_TOKEN;
        let span = tracing::Span::current();
        span.record("system_chars", system_prompt.len());
        span.record("user_chars", user_message.len());
        span.record("token_estimate", token_estimate);
        Ok(AssembledContext {
            system_prompt,
            user_message,
            token_estimate,
        })
    }

    #[instrument(
        name = "context.build_for_reviewer",
        level = "debug",
        skip_all,
        fields(
            role = "reviewer",
            bundle_id = %bundle.id,
            work_id = %work.id,
            diff_chars = diff.len(),
            noop_files_count = noop_files.map(|f| f.len()).unwrap_or(0),
            system_chars = tracing::field::Empty,
            user_chars = tracing::field::Empty,
            token_estimate = tracing::field::Empty,
        ),
        err,
    )]
    fn build_for_reviewer(
        &self,
        bundle: &Bundle,
        work: &Work,
        diff: &str,
        noop_files: Option<&[(String, String)]>,
    ) -> Result<AssembledContext, ContextError> {
        let system_prompt = REVIEWER_SYSTEM_PROMPT.to_string();
        let user_message = render_reviewer_user_message(bundle, work, diff, noop_files);
        let token_estimate = (system_prompt.len() + user_message.len()) / CHARS_PER_TOKEN;
        let span = tracing::Span::current();
        span.record("system_chars", system_prompt.len());
        span.record("user_chars", user_message.len());
        span.record("token_estimate", token_estimate);
        Ok(AssembledContext {
            system_prompt,
            user_message,
            token_estimate,
        })
    }
}

fn render_system_prompt(tool_schemas: &[ToolSchema]) -> String {
    let mut s = String::new();
    s.push_str("You are the Implementer agent in a loopr pipeline. Your job is to complete one Work item inside a git worktree.\n\n");
    s.push_str("You operate by emitting a JSON array of actions. Each iteration, you receive the current Work (title, acceptance criteria), the worktree path, and summaries of prior iterations. You respond with one JSON array.\n\n");
    s.push_str("Valid action types:\n");
    s.push_str("  - run_tool: invoke a tool (see schemas below)\n");
    s.push_str("  - commit_changes: git add + commit staged work\n");
    s.push_str("  - propose_bundle: finalize the Bundle for review (use only when all acceptance criteria pass)\n");
    s.push_str("  - done: no-op completion (use when no code changes are required)\n");
    s.push_str("  - need_help: escalate to a human (use only when truly blocked)\n\n");
    s.push_str("Tools available:\n");
    if tool_schemas.is_empty() {
        s.push_str("  (none)\n");
    } else {
        for schema in tool_schemas {
            let _ = writeln!(s, "  - {}: {}", schema.name, schema.description);
        }
    }
    s.push_str("\nRespond with a JSON array of actions, nothing else. Example:\n");
    s.push_str(r#"[{"type":"run_tool","tool":"bash","input":{"command":"ls"}}]"#);
    s.push('\n');
    s
}

fn render_user_message(
    work: &Work,
    worktree_path: &Path,
    history: &[IterationSummary],
    state: &StateSummary,
    iteration: u32,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Work: {}", work.title);
    let _ = writeln!(s, "Work ID: {}", work.id);
    let _ = writeln!(s, "Worktree: {}", worktree_path.display());
    let _ = writeln!(s, "Iteration: {iteration}");
    s.push('\n');

    s.push_str("## Acceptance Criteria\n");
    if work.acceptance_criteria.is_empty() {
        s.push_str("(none specified)\n");
    } else {
        for (i, ac) in work.acceptance_criteria.0.iter().enumerate() {
            let _ = writeln!(s, "{}. {}", i + 1, ac);
        }
    }
    s.push('\n');

    if let Some(reason) = &state.rejected_bundle_reason {
        s.push_str("## Prior Bundle Was Rejected\n");
        s.push_str(reason);
        s.push('\n');
        s.push('\n');
    }

    if !history.is_empty() {
        s.push_str("## Prior Iterations\n");
        for entry in history {
            let _ = writeln!(s, "### Iteration {}", entry.iteration);
            let capped = cap_chars(&entry.actions_summary, ITERATION_SUMMARY_CAP);
            s.push_str(&capped);
            s.push('\n');
            s.push('\n');
        }
    }

    s.push_str("## Respond\n");
    s.push_str("Emit a JSON array of actions for this iteration.\n");
    s
}

/// Truncate to at most `cap` bytes, respecting UTF-8 boundaries. If
/// truncated, append a `[truncated; original N chars]` marker.
fn cap_chars(input: &str, cap: usize) -> String {
    if input.len() <= cap {
        return input.to_string();
    }
    let mut cut = cap;
    while !input.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}… [truncated; original {} chars]", &input[..cut], input.len())
}

#[cfg(test)]
mod tests;
