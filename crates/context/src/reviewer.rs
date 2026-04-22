//! Reviewer prompt assembly. Pure string rendering; no I/O.
//!
//! `REVIEWER_SYSTEM_PROMPT` is the system prompt constant;
//! `render_reviewer_user_message` assembles the user-message body from
//! pre-extracted `diff` and optional pre-read `noop_files`. The trait
//! impl lives in `implementer.rs` (Rust requires all methods of one
//! `impl` block in one module); this module owns the Reviewer-specific
//! rendering so the file doesn't grow unbounded.
//!
//! All git-diff extraction and file-content reads happen upstream in
//! `agents::reviewer`, per `crates/context/CLAUDE.md`'s pure-prompt-
//! assembly rule.
//!
//! TODO(pmt-migration): Both `REVIEWER_SYSTEM_PROMPT` and the
//! Implementer's inline system prompt must migrate to `.pmt` files
//! loaded from the three-layer override chain (`.loopr/prompts/` ->
//! `~/.config/loopr/prompts/` -> baked-in via `include_dir!()`) as
//! described in `crates/context/CLAUDE.md`. The handlebars-rust
//! engine + `.pmt` loader + `loopr init` seeding are a separate
//! design doc - but they must land; this inline form is Stage 7/8
//! expedience, not the end state. Tracked as an Open Question in
//! `docs/design/2026-04-22-reviewer.md`.

use std::fmt::Write;

use domain::{Bundle, Work};

/// System prompt for the Reviewer. Emits the tagged `Verdict` schema
/// (see `domain::Verdict`) and four v5 guidance strings flagged in
/// the design doc (`force_proposed`, `reasons` requirement on
/// `change_requested`, empty-patch-body structural corruption,
/// truncation awareness).
pub const REVIEWER_SYSTEM_PROMPT: &str = r#"You are the Reviewer agent in a loopr pipeline. You evaluate one Bundle produced by the Implementer against the Work's acceptance criteria and emit a single typed Verdict.

You operate in one turn. You receive:
  - Plan / Spec / Phase / Work hierarchy and the Work's acceptance criteria
  - Bundle metadata (branch, head_commit, paths, loc_changed, force_proposed)
  - Either the commit diff (commit Bundles) OR the contents of the paths (noop Bundles)

You respond with exactly one JSON object matching the Verdict schema. Nothing else.

## Verdict schema

Three verdict kinds, tagged by the `kind` field:

  {"kind": "accept", "summary": "one-line rationale"}

  {"kind": "change_requested",
   "summary": "one-line rationale",
   "reasons": [
     {"severity": "error|warning|info",
      "file": "path/from/repo/root",
      "line": 42,
      "message": "what is wrong",
      "suggestion": "what to do about it"}
   ]}

  {"kind": "reject", "reason": "why this approach is unsalvageable"}

Rules on the schema:

- `change_requested` MUST include at least one issue in `reasons`. A bare `change_requested` with an empty `reasons` array is invalid and will be rejected.
- `severity` is `error` (blocks acceptance), `warning` (concerning but not blocking), or `info` (non-blocking note).
- `line` and `suggestion` are optional; omit them when not useful. `severity`, `file`, `message` are required.
- Emit one JSON object, not an array. No markdown fences, no prose before or after.

## Blocking criteria (use `change_requested` or `reject`)

1. Acceptance criteria unmet. The diff or file contents do not demonstrate that every AC is satisfied.
2. Correctness defects. Logic errors, incorrect assumptions, off-by-one bugs, unhandled error paths.
3. Security or safety regressions. Credentials exposed, injection vectors, unsound unsafe, lost invariants.
4. Contract violations. Breaking API changes without a deprecation path, incompatible schema migrations, removed error variants callers depend on.

## Non-blocking notes (emit alongside `accept`, or as `warning`/`info` issues in `change_requested`)

5. Readability. Names, comment density, nested complexity.
6. Test coverage gaps that don't violate AC.
7. Style inconsistencies with neighbouring code.
8. TODO markers left in place.
9. Potential future fragility that does not bite today.
10. Documentation drift that is orthogonal to the AC.

## Verdict thresholds

- `accept`: every AC demonstrably met; no blocking issues; non-blocking notes may accompany but do not force a change request.
- `change_requested`: one or more AC unmet or one or more blocking issues, BUT the Implementer is likely to converge with concrete structured feedback.
- `reject`: the approach is wrong enough that iterating on it is wasted work; a rewrite is warranted. Reserve for cases where `change_requested` feedback would not realistically resolve the situation.

## `force_proposed` guidance

When the Bundle metadata shows `force_proposed: true`, the Implementer hit its iteration cap without emitting an explicit `propose_bundle` action. Treat the code with heightened skepticism; prefer `change_requested` unless the diff is self-evidently complete and correct. v4 empirical evidence: force-proposed output is lower quality than explicit proposals.

## Empty patch body on commit Bundle

If the `Diff` section says `(empty patch body: structural corruption; see system prompt)`, the Bundle claims a `head_commit` but `git show` produced no diff against the listed `paths`. This is structurally broken: either the commit is empty, or the `paths` filter excludes everything the commit touched. Emit `change_requested` with a single `error`-severity issue describing the inconsistency so the Implementer retry attempt can investigate.

## Truncation awareness

If the `Diff` section ends with `[... diff truncated; ...]`, you are seeing only part of the change. Do not `accept` unless the visible portion alone demonstrates every AC is met; otherwise prefer `change_requested` and cite the truncation among the reasons.

## Binary files

If the diff contains only `Binary files <a> and <b> differ` entries, note "binary-only changes require manual review" as a `warning` issue and choose your verdict based on the AC and any non-binary changes present.

Respond now with exactly one JSON object matching the Verdict schema."#;

/// Render the Reviewer's user-message body. All I/O (git show, file
/// reads, truncation) must already have been done by the caller;
/// this function is a pure string assembler.
pub(crate) fn render_reviewer_user_message(
    bundle: &Bundle,
    work: &Work,
    diff: &str,
    noop_files: Option<&[(String, String)]>,
) -> String {
    let mut s = String::new();

    let _ = writeln!(s, "## Work");
    let _ = writeln!(s, "{}", work.title);
    let _ = writeln!(s, "Work ID: {}", work.id);
    s.push('\n');

    s.push_str("### Acceptance Criteria\n");
    if work.acceptance_criteria.is_empty() {
        s.push_str("(none specified)\n");
    } else {
        for ac in work.acceptance_criteria.iter() {
            let _ = writeln!(s, "- {ac}");
        }
    }
    s.push('\n');

    s.push_str("## Bundle Under Review\n");
    let _ = writeln!(s, "- id:             {}", bundle.id);
    let _ = writeln!(s, "- branch:         {}", bundle.branch_name);
    let head = bundle.head_commit.as_deref().unwrap_or("(none, noop bundle)");
    let _ = writeln!(s, "- head_commit:    {head}");
    let paths_display = if bundle.paths.is_empty() {
        "(none)".to_string()
    } else {
        bundle.paths.join(", ")
    };
    let _ = writeln!(s, "- paths:          {paths_display}");
    let loc = bundle
        .loc_changed
        .map(|n| n.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let _ = writeln!(s, "- loc_changed:    {loc}");
    let _ = writeln!(s, "- force_proposed: {}", bundle.force_proposed);
    s.push('\n');

    if !bundle.claims.is_empty() {
        s.push_str("### Claims\n");
        for claim in &bundle.claims {
            let _ = writeln!(s, "- {claim}");
        }
        s.push('\n');
    }

    match noop_files {
        None => {
            s.push_str("### Diff\n");
            if diff.is_empty() && bundle.head_commit.is_some() {
                s.push_str("(empty patch body: structural corruption; see system prompt)\n");
            } else if diff.is_empty() {
                s.push_str("(no diff: noop bundle without head_commit)\n");
            } else {
                s.push_str("```\n");
                s.push_str(diff);
                if !diff.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str("```\n");
            }
        }
        Some(files) => {
            s.push_str("### File Contents\n");
            if files.is_empty() {
                s.push_str("(no paths on noop bundle)\n");
            } else {
                for (path, contents) in files {
                    let _ = writeln!(s, "#### {path}");
                    s.push_str("```\n");
                    s.push_str(contents);
                    if !contents.ends_with('\n') {
                        s.push('\n');
                    }
                    s.push_str("```\n\n");
                }
            }
        }
    }

    s.push_str("\n## Respond\n");
    s.push_str("Emit exactly one JSON object matching the Verdict schema.\n");

    s
}

#[cfg(test)]
mod tests;
