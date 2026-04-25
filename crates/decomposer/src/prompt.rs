//! Prompt assembly for `decompose`. Renders the on-disk
//! `decompose/work/{system,user}.pmt` templates via
//! `context::PromptLoader`.
//!
//! The loader honors the three-layer override chain
//! (`<target>/.loopr/prompts/` -> `~/.config/loopr/prompts/` -> baked
//! into the binary) so a user can edit the decomposer's prompts
//! without rebuilding loopr. The retry-path branching (whether to
//! interpolate a previous attempt's error) lives here in Rust; the
//! template is responsible only for the final layout.

use serde::Serialize;

use context::PromptLoader;

use crate::error::DecomposerError;

/// Cap on the error text embedded in the retry prompt. Beyond this
/// the prompt interpolates a truncated slice plus the original byte
/// length so the model knows truncation happened.
///
/// Rationale: `LlmError::Fatal(Auth)`'s body can be several KiB of
/// HTML from an auth-gateway; combined with a typical system prompt
/// (~3 KiB) and workspace tree (up to ~15 KiB at MAX_ENTRIES), an
/// unbounded error body can overrun Anthropic's context window.
/// 2 KiB of error text is plenty for the model to understand *what*
/// went wrong; more is noise.
const RETRY_ERROR_MAX_BYTES: usize = 2048;

#[derive(Serialize)]
struct DecomposeSystemCtx<'a> {
    tree: &'a str,
}

#[derive(Serialize)]
struct DecomposeUserCtx<'a> {
    goal: &'a str,
    prev_error: Option<String>,
}

#[tracing::instrument(level = "debug", skip_all, fields(tree_chars = tree.len()))]
pub(crate) fn assemble_system(loader: &PromptLoader, tree: &str) -> Result<String, DecomposerError> {
    let ctx = DecomposeSystemCtx { tree };
    Ok(loader.render("decompose/work/system.pmt", &ctx)?)
}

/// Assemble the user message. On first attempt, `prev_error` is
/// `None`. On retry, the previous attempt's error is interpolated
/// under `## Previous Attempt Failed`, capped at `RETRY_ERROR_MAX_BYTES`
/// with a truncation suffix to prevent prompt-size blowup.
#[tracing::instrument(level = "debug", skip_all, fields(goal_len = goal.len(), retry = prev_error.is_some()))]
pub(crate) fn assemble_user(
    loader: &PromptLoader,
    goal: &str,
    prev_error: Option<&str>,
) -> Result<String, DecomposerError> {
    let prev_error = prev_error.map(truncate_retry_error);
    let ctx = DecomposeUserCtx { goal, prev_error };
    Ok(loader.render("decompose/work/user.pmt", &ctx)?)
}

/// Cap `err` at `RETRY_ERROR_MAX_BYTES`, preserving UTF-8 char
/// boundaries, appending `… [error truncated from N bytes]` when
/// truncation actually happens. The exact suffix wording is asserted
/// on by the Phase 6 truncation test — don't change it without
/// updating the test.
fn truncate_retry_error(err: &str) -> String {
    if err.len() <= RETRY_ERROR_MAX_BYTES {
        return err.to_string();
    }
    let original = err.len();
    let mut cut = RETRY_ERROR_MAX_BYTES;
    while !err.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}… [error truncated from {} bytes]", &err[..cut], original)
}

#[cfg(test)]
mod tests;
