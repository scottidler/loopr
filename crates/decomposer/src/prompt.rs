//! Prompt assembly for `decompose`. One const `SYSTEM_TEMPLATE` with a
//! single `{{ TREE }}` substitution, plus the user-message builder
//! that appends the goal and, on retry, the previous attempt's error.
//!
//! Template text is adapted from v3's `prompts/decompose/work.pmt`
//! with Brief-mode framing (Stage 6 has only one mode) and tool-use
//! output wording substituted in place of v3's raw-JSON-array
//! instructions. Stage 7's `context-builder` earns the handlebars-
//! backed prompt engine that replaces this inline template; until
//! then, `const &str` with `.replace("{{ TREE }}", ...)`.

#![allow(dead_code)]

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

const SYSTEM_TEMPLATE: &str = r#"You are a software architect decomposing a Plan into Work items.

A Work item is the implementer's complete brief - everything needed to write code without asking a question. Every decision has been made upstream.

## Output

Use the submit_decomposition tool. For each Work, provide:
- title: Short descriptive title
- content: Full markdown content (template-free; prose is fine)
- dependencies: Titles of sibling Works this one depends on
- acceptance_criteria: Concrete, testable assertions (assert statements)

## Rules

1. Each Work's acceptance_criteria must be concrete, testable assertions (assert statements), NOT prose.
2. Dependencies are sibling titles only. The ONLY valid reason for a dependency is when a Work literally cannot compile or test without another Work's output being present in the repo.
3. Produce 1-5 Work items. Prefer fewer, larger items over many small serial items. Two independent items beat five dependent ones.
4. Each Work must have at least one acceptance criterion.

## Parallelism

Work items are discrete chunks that can be built independently and in parallel. This is their primary design purpose. When decomposing:

- Most work items should have NO dependencies.
- Prefer fan-out: many independent Work items, NOT linear chains.
- STRONGLY AVOID splitting work on the same file across parallel Work items. Same-file parallel writes cause merge conflicts.

## Workspace file tree

The target repository currently contains these files:

{{ TREE }}

Use this to ground your decomposition in the actual codebase: name real files when referring to them, do not propose creating files that already exist, and do not propose removing files you cannot see.
"#;

pub(crate) fn assemble_system(tree: &str) -> String {
    SYSTEM_TEMPLATE.replace("{{ TREE }}", tree)
}

/// Assemble the user message. On first attempt, `prev_error` is
/// `None`. On retry, the previous attempt's error is interpolated
/// under `## Previous Attempt Failed`, capped at `RETRY_ERROR_MAX_BYTES`
/// with a truncation suffix to prevent prompt-size blowup.
pub(crate) fn assemble_user(goal: &str, prev_error: Option<&str>) -> String {
    match prev_error {
        None => format!("## Plan\n\n{goal}"),
        Some(err) => {
            let truncated = truncate_retry_error(err);
            format!(
                "## Plan\n\n{goal}\n\n## Previous Attempt Failed\n\n{truncated}\n\nPlease fix the issues and try again."
            )
        }
    }
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
