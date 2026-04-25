//! `InlineContextBuilder`: the `ContextBuilder` impl backed by the
//! disk-resident `.pmt` tree via `PromptLoader`.
//!
//! "Inline" once meant inline string literals; it now means stateless,
//! deterministic, thread-safe rendering on top of the loader (the
//! prompt source moved from Rust strings to `crates/context/prompts/`
//! `.pmt` files). Per the v5 "no coexistence migrations" rule,
//! callers don't choose between inline-string and file-loaded modes —
//! the latter is the only mode.

use std::path::Path;
use std::sync::Arc;

use serde::Serialize;
use tracing::instrument;

use domain::{Bundle, Work};
use tools::ToolSchema;

use crate::loader::PromptLoader;
use crate::reviewer::build_reviewer_user_ctx;
use crate::{AssembledContext, ContextBuilder, ContextError, ITERATION_SUMMARY_CAP, IterationSummary, StateSummary};

/// Rough tokens-per-char estimate (English text averages ~4 chars
/// per token; we use 4 as the divisor for a generous-side estimate
/// that errs toward undercounting).
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// `ContextBuilder` impl that renders prompts via `PromptLoader`.
/// Construct with `new()` (baked-only loader, used by tests) or
/// `with_loader(loader)` (production: pass a layer-aware loader from
/// the binary's `PromptLoader::for_target(target)` call).
#[derive(Debug, Clone)]
pub struct InlineContextBuilder {
    loader: Arc<PromptLoader>,
}

impl Default for InlineContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl InlineContextBuilder {
    /// Construct with a baked-only loader. The baked layer is always
    /// available, so this cannot fail in practice; an `expect` here
    /// catches the (programmer-error) case of a malformed baked
    /// `.pmt` file slipping through CI.
    pub fn new() -> Self {
        let loader = PromptLoader::new(None, None).expect("baked .pmt tree must compile");
        Self {
            loader: Arc::new(loader),
        }
    }

    /// Construct with a caller-supplied loader. Production code uses
    /// this with the result of `PromptLoader::for_target(target)` so
    /// the project and user override layers participate.
    pub fn with_loader(loader: Arc<PromptLoader>) -> Self {
        Self { loader }
    }
}

#[derive(Serialize)]
struct ToolCtx<'a> {
    name: &'a str,
    description: &'a str,
}

#[derive(Serialize)]
struct ImplementerSystemCtx<'a> {
    tools: Vec<ToolCtx<'a>>,
}

#[derive(Serialize)]
struct AcCtx<'a> {
    n: usize,
    text: &'a str,
}

#[derive(Serialize)]
struct PriorIterationCtx<'a> {
    iteration: u32,
    summary: String,
    #[serde(skip)]
    _phantom: std::marker::PhantomData<&'a ()>,
}

#[derive(Serialize)]
struct ImplementerUserCtx<'a> {
    work_id: String,
    work_title: &'a str,
    worktree_path: String,
    iteration: u32,
    acceptance_criteria: Vec<AcCtx<'a>>,
    rejected_bundle_reason: Option<&'a str>,
    prior_iterations: Vec<PriorIterationCtx<'a>>,
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
        let system_ctx = ImplementerSystemCtx {
            tools: tool_schemas
                .iter()
                .map(|s| ToolCtx {
                    name: s.name,
                    description: s.description,
                })
                .collect(),
        };
        let system_prompt = self.loader.render("agents/implementer/system.pmt", &system_ctx)?;

        let acceptance_criteria: Vec<AcCtx> = work
            .acceptance_criteria
            .0
            .iter()
            .enumerate()
            .map(|(i, ac)| AcCtx {
                n: i + 1,
                text: ac.as_str(),
            })
            .collect();
        let prior_iterations: Vec<PriorIterationCtx> = history
            .iter()
            .map(|h| PriorIterationCtx {
                iteration: h.iteration,
                summary: cap_chars(&h.actions_summary, ITERATION_SUMMARY_CAP),
                _phantom: std::marker::PhantomData,
            })
            .collect();
        let user_ctx = ImplementerUserCtx {
            work_id: work.id.to_string(),
            work_title: work.title.as_str(),
            worktree_path: worktree_path.display().to_string(),
            iteration,
            acceptance_criteria,
            rejected_bundle_reason: state.rejected_bundle_reason.as_deref(),
            prior_iterations,
        };
        let user_message = self.loader.render("agents/implementer/user.pmt", &user_ctx)?;

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
        let system_prompt = self
            .loader
            .render("agents/reviewer/system.pmt", &serde_json::json!({}))?;
        let user_ctx = build_reviewer_user_ctx(bundle, work, diff, noop_files);
        let user_message = self.loader.render("agents/reviewer/user.pmt", &user_ctx)?;
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
