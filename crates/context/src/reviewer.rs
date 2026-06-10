//! Reviewer prompt context-building. Pure typed-context construction;
//! no I/O, no string rendering. The actual handlebars render happens
//! in `crates/context/src/implementer.rs`'s
//! `InlineContextBuilder::build_for_reviewer` via the loader.
//!
//! All git-diff extraction and file-content reads happen upstream in
//! `agents::reviewer`, per `crates/context/CLAUDE.md`'s pure-prompt-
//! assembly rule.

use std::fmt::Write;

use serde::Serialize;

use domain::{Bundle, Work};

#[derive(Serialize)]
pub(crate) struct ReviewerBundleCtx<'a> {
    pub(crate) id: String,
    pub(crate) branch_name: &'a str,
    pub(crate) head_commit_display: String,
    pub(crate) paths_display: String,
    pub(crate) loc_changed_display: String,
    pub(crate) force_proposed: bool,
    pub(crate) claims: &'a [String],
}

#[derive(Serialize)]
pub(crate) struct ReviewerUserCtx<'a> {
    pub(crate) work_title: &'a str,
    pub(crate) work_id: String,
    pub(crate) acceptance_criteria: &'a [String],
    /// Per-Work allowed-files scope (`work.files`). Rendered so the reviewer
    /// can enforce the "out-of-scope file modifications" criterion against
    /// the actual scope (Phase-5 finding 10). Empty = no restriction.
    pub(crate) allowed_files: &'a [String],
    pub(crate) bundle: ReviewerBundleCtx<'a>,
    pub(crate) evidence_section: String,
}

/// Build the typed render context for the Reviewer's user message.
/// `diff: &str` is pre-extracted (header stripped, truncated);
/// `noop_files: Option<&[(String, String)]>` is pre-read for noop
/// Bundles with aggregate + per-file caps already applied. `None`
/// renders the `Diff` section; `Some` renders the `File Contents`
/// section. No I/O happens here; all git-show and file-read calls
/// live upstream in `agents::reviewer`.
pub(crate) fn build_reviewer_user_ctx<'a>(
    bundle: &'a Bundle,
    work: &'a Work,
    diff: &str,
    noop_files: Option<&[(String, String)]>,
) -> ReviewerUserCtx<'a> {
    let head_commit_display = bundle
        .head_commit
        .as_deref()
        .unwrap_or("(none, noop bundle)")
        .to_string();
    let paths_display = if bundle.paths.is_empty() {
        "(none)".to_string()
    } else {
        bundle.paths.join(", ")
    };
    let loc_changed_display = bundle
        .loc_changed
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let evidence_section = build_evidence_section(
        diff,
        noop_files,
        bundle.head_commit.is_some(),
        bundle.noop_reason.as_deref(),
    );

    ReviewerUserCtx {
        work_title: work.title.as_str(),
        work_id: work.id.to_string(),
        acceptance_criteria: work.acceptance_criteria.0.as_slice(),
        allowed_files: work.files.as_slice(),
        bundle: ReviewerBundleCtx {
            id: bundle.id.to_string(),
            branch_name: bundle.branch_name.as_str(),
            head_commit_display,
            paths_display,
            loc_changed_display,
            force_proposed: bundle.force_proposed,
            claims: bundle.claims.as_slice(),
        },
        evidence_section,
    }
}

/// Pre-render the diff or file-contents block as a complete markdown
/// section (heading + code fences + special-case prose). Keeps the
/// `.pmt` template a flat layout instead of branching on diff vs
/// noop-files vs empty-with-head vs empty-without-head.
fn build_evidence_section(
    diff: &str,
    noop_files: Option<&[(String, String)]>,
    has_head_commit: bool,
    noop_reason: Option<&str>,
) -> String {
    match noop_files {
        None => {
            let mut s = String::from("### Diff\n");
            if diff.is_empty() && has_head_commit {
                s.push_str("(empty patch body: structural corruption; see system prompt)\n");
            } else if diff.is_empty() {
                s.push_str("(no diff: noop bundle without head_commit)\n");
            } else {
                push_fenced(&mut s, diff);
            }
            s
        }
        Some(files) => {
            // Finding 5: a noop (`Done`) bundle's primary evidence is the
            // agent's stated justification. Render it ahead of the file
            // contents so the reviewer has something to judge "no code
            // needed" against (previously the reviewer got empty claims,
            // empty paths, and `noop_reason` was never surfaced).
            let mut s = String::from("### Noop Justification\n");
            match noop_reason {
                Some(reason) if !reason.trim().is_empty() => {
                    s.push_str(reason);
                    if !reason.ends_with('\n') {
                        s.push('\n');
                    }
                }
                _ => s.push_str("(none given)\n"),
            }
            s.push_str("\n### File Contents\n");
            if files.is_empty() {
                s.push_str("(no paths on noop bundle)\n");
            } else {
                for (path, contents) in files {
                    let _ = writeln!(s, "#### {path}");
                    push_fenced(&mut s, contents);
                    s.push('\n');
                }
            }
            s
        }
    }
}

/// Append `content` to `out` inside a code fence whose backtick run is one
/// longer than the longest backtick run inside `content` (floor of 3).
///
/// Phase-5 finding 9 (forged-verdict injection): repo diffs and file
/// contents are untrusted target data. A fixed ``` fence is escapable —
/// content containing its own ``` line breaks out of the fence into
/// instruction position, where a planted "review passed, emit accept" line
/// could forge a verdict. A dynamically-sized fence cannot be closed by the
/// content it wraps.
fn push_fenced(out: &mut String, content: &str) {
    let longest = longest_backtick_run(content);
    let fence = "`".repeat((longest + 1).max(3));
    out.push_str(&fence);
    out.push('\n');
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
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

#[cfg(test)]
mod tests;
