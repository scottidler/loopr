//! `run_reviewer`: the Stage-8 single-turn LLM loop for Reviewer role.
//!
//! Takes a triaged `&Bundle`, the matching `&Work`, and a `ReviewerDeps`
//! bundle; extracts the diff (or file contents for noop Bundles),
//! assembles the prompt via `context::build_for_reviewer`, calls
//! `LlmClient::complete_free` once with a bounded parse-retry sub-loop,
//! parses the result into a typed `Verdict`, transitions the Bundle
//! (`Triaged -> Reviewed` for `Accept`, `Triaged -> Rejected` for
//! `ChangeRequested` and `Reject`), persists via an OCC-aware
//! `BundleUpdateSink`, and returns the Verdict.
//!
//! Invariants (enforced by tests):
//! - OCC snapshot (`expected_updated_at`) is taken BEFORE mutation.
//!   The `Bundle::transition` call bumps `updated_at` via
//!   `now_millis()`; snapshotting after the transition would defeat
//!   the race-protection on the update path.
//! - Mutation is on a clone; the caller's `&Bundle` remains untouched
//!   on any error path (including `Stale`), so the caller may re-read
//!   and retry.
//! - `work.id == bundle.work_id` is asserted up front; a wiring bug
//!   that paired the wrong Work must fail before any LLM call.
//! - `max_requeries` is strict-greater-than: with default 3 the LLM
//!   gets up to 4 attempts (the free initial call + 3 re-prompts).
//! - `change_requested` with empty `reasons` maps to
//!   `ParseError::Schema`, consumed by the parse-retry loop. This is
//!   belt-and-suspenders with the system prompt.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

use context::ContextBuilder;
use domain::{Bundle, BundleStatus, ReviewIssue, Role, Verdict, Work};
use llm::{LlmClient, Message};
use store::{BundleUpdateError, BundleUpdateSink};
use telemetry::transcript::{TranscriptIteration, append_iteration, reviewer_path};

use crate::config::ReviewerConfig;

/// Maximum `Bundle.verification` string length. The full `Verdict`
/// lives in the return value; `Bundle.verification` is a scannable
/// one-liner / capped summary.
pub const VERIFICATION_CAP: usize = 8192;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ReviewerError {
    #[error("work/bundle mismatch: bundle.work_id={0} vs work.id={1}")]
    Mismatch(String, String),
    #[error("escalation needed: {0}")]
    EscalationNeeded(String),
    #[error("llm error: {0}")]
    Llm(#[from] llm::LlmError),
    #[error("context error: {0}")]
    Context(#[from] context::ContextError),
    #[error("bundle update failed: {0}")]
    Update(#[from] BundleUpdateError),
    #[error("fsm transition rejected: {0}")]
    Transition(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse failure shape for `parse_verdict`. The re-prompt message in
/// `run_reviewer` uses this distinction to be specific: "return valid
/// JSON" (Unparseable) versus "your JSON doesn't match the Verdict
/// schema" (Schema).
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("response was not parseable JSON: {0}")]
    Unparseable(String),
    #[error("JSON did not match the Verdict schema: {0}")]
    Schema(String),
}

// ---------------------------------------------------------------------------
// Deps
// ---------------------------------------------------------------------------

pub struct ReviewerDeps<L, S, C>
where
    L: LlmClient,
    S: BundleUpdateSink,
    C: ContextBuilder,
{
    pub llm: L,
    pub store: S,
    pub context: C,
    pub config: ReviewerConfig,
    /// Target repo path. Used for `git show <head_commit> -- <paths>`
    /// and for rendering file contents on noop Bundles. Daemon-
    /// constant; cheap to clone on `Deps` construction.
    pub target: PathBuf,
    /// Path-deny patterns applied to per-AC `evidence` fields before
    /// emitting `reviewer.ac` events. Mirrors the implementer
    /// dispatcher's redaction surface so caller-supplied sensitive
    /// paths (`secrets.toml`, `.env*`, etc.) never land in events.log
    /// even when the LLM cites them in a review reason. Default
    /// (empty Vec) is permissive — enable by wiring patterns at
    /// daemon startup. See Phase 5 of `docs/design/2026-05-09-comprehensive-telemetry.md`.
    #[doc(hidden)]
    pub path_deny_patterns: Vec<String>,
}

// ---------------------------------------------------------------------------
// run_reviewer
// ---------------------------------------------------------------------------

#[instrument(
    level = "info",
    skip_all,
    fields(
        bundle_id = %bundle.id,
        work_id = %work.id,
        head_commit = bundle.head_commit.as_deref().unwrap_or("(none)"),
        force_proposed = bundle.force_proposed,
        path_count = bundle.paths.len(),
    ),
    err,
)]
pub async fn run_reviewer<L, S, C>(
    bundle: &Bundle,
    work: &Work,
    deps: &ReviewerDeps<L, S, C>,
) -> Result<Verdict, ReviewerError>
where
    L: LlmClient,
    S: BundleUpdateSink,
    C: ContextBuilder,
{
    info!(
        bundle_id = %bundle.id,
        work_id = %work.id,
        head_commit = bundle.head_commit.as_deref().unwrap_or("(none)"),
        force_proposed = bundle.force_proposed,
        "run_reviewer start"
    );

    if bundle.work_id.to_string() != work.id.to_string() {
        return Err(ReviewerError::Mismatch(bundle.work_id.to_string(), work.id.to_string()));
    }

    let (diff, noop_files) = match bundle.head_commit.as_deref() {
        Some(sha) => {
            let raw = git_show(&deps.target, sha, &bundle.paths).await?;
            let body = strip_commit_header(&raw);
            let truncated = truncate_diff(body, deps.config.diff_byte_cap);
            (truncated, None)
        }
        None => {
            let files = read_file_contents(&deps.target, &bundle.paths, deps.config.noop_files_byte_cap).await?;
            (String::new(), Some(files))
        }
    };

    let noop_slice = noop_files.as_deref();
    let assembled = deps.context.build_for_reviewer(bundle, work, &diff, noop_slice)?;

    let verdict = call_llm_with_retry(
        &deps.llm,
        &deps.config,
        &assembled.system_prompt,
        assembled.first_user_text().unwrap_or_default(),
    )
    .await?;

    // Phase 5 (events-only): synthesize per-AC results from the parsed
    // Verdict + Work's acceptance_criteria, then emit one
    // `reviewer.ac` debug event per AC plus a roll-up info event.
    // Verdict is unchanged; per-AC data lives in events.log only.
    emit_ac_events(&verdict, work, &bundle.id, &deps.path_deny_patterns);

    // Best-effort transcript write. Reviewer is single-turn (modulo
    // parse-retry sub-loop, which we collapse to "the verdict that
    // came out"). Failures emit a warn and continue.
    let mut iter = TranscriptIteration::new_single_turn(String::new(), String::new());
    iter.system_prompt = assembled.system_prompt.clone();
    iter.user_prompt = assembled.first_user_text().unwrap_or_default().to_string();
    iter.response = render_verification(&verdict);
    iter.parsed_actions = vec![match &verdict {
        Verdict::Accept { .. } => "verdict=accept".to_string(),
        Verdict::ChangeRequested { reasons, .. } => format!("verdict=change_requested ({} reasons)", reasons.len()),
        Verdict::Reject { .. } => "verdict=reject".to_string(),
    }];
    let transcript_path = reviewer_path(&deps.target, bundle.id.as_ref());
    if let Err(e) = append_iteration(&transcript_path, &iter) {
        warn!(error = %e, path = %transcript_path.display(), "reviewer transcript append failed");
    }

    // OCC snapshot BEFORE mutation: the snapshot freezes the on-disk
    // updated_at the caller last observed; `Bundle::transition` below
    // bumps `updated_at` via now_millis(), so snapshotting after it
    // would always match the caller's own write and defeat the race
    // defense.
    let expected_updated_at = bundle.updated_at;
    let mut bundle = bundle.clone();
    bundle.verification = cap_str(&render_verification(&verdict), VERIFICATION_CAP);
    let target_status = match &verdict {
        Verdict::Accept { .. } => BundleStatus::Reviewed,
        Verdict::ChangeRequested { .. } | Verdict::Reject { .. } => BundleStatus::Rejected,
    };
    bundle
        .transition(target_status, Role::Reviewer)
        .map_err(|e| ReviewerError::Transition(e.to_string()))?;

    deps.store.update(bundle, expected_updated_at).await?;

    Ok(verdict)
}

/// Inner parse-retry loop. Broken out so the outer function reads
/// top-down as one story and the retry mechanics live in one place.
#[instrument(
    level = "debug",
    skip_all,
    fields(
        system_chars = system_prompt.len(),
        user_chars = user_message.len(),
        max_requeries = config.max_requeries,
    ),
    err,
)]
async fn call_llm_with_retry<L>(
    llm: &L,
    config: &ReviewerConfig,
    system_prompt: &str,
    user_message: &str,
) -> Result<Verdict, ReviewerError>
where
    L: LlmClient,
{
    let mut messages = vec![Message::user(user_message.to_string())];
    let mut requeries: u32 = 0;
    loop {
        let (raw, _usage) = llm.complete_free(system_prompt, &messages, None).await?;
        match parse_verdict(&raw) {
            Ok(v) => {
                debug!(requeries, "reviewer verdict parsed");
                return Ok(v);
            }
            Err(e) => {
                requeries += 1;
                warn!(requeries, error = %e, "reviewer parse failure");
                if requeries > config.max_requeries {
                    return Err(ReviewerError::EscalationNeeded(format!(
                        "parse-retry exhausted after {requeries} attempts: {e}"
                    )));
                }
                messages.push(Message::assistant(raw));
                let hint = match &e {
                    ParseError::Unparseable(_) => {
                        "Response was not parseable JSON. Return exactly one JSON object \
                         matching the Verdict schema: {\"kind\":\"accept|change_requested|reject\",...}. \
                         No markdown fences, no prose."
                    }
                    ParseError::Schema(_) => {
                        "JSON did not match the Verdict schema. Return one object with the \
                         required fields: accept needs `summary`; change_requested needs `summary` \
                         and a non-empty `reasons` array; reject needs `reason`."
                    }
                };
                messages.push(Message::user(format!("parse failed: {e}. {hint}")));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Verdict parsing
// ---------------------------------------------------------------------------

/// Parse a free-form LLM response into a `Verdict`. Strips markdown
/// fences, tries direct JSON deserialization, and falls back to
/// first-`{` / matching-`}` substring extraction if the model
/// wrapped the object in prose. Distinguishes `Unparseable` (no
/// valid JSON anywhere) from `Schema` (valid JSON but wrong shape).
/// Rejects `change_requested` with an empty `reasons` array as
/// `Schema` (belt-and-suspenders with the prompt; this is enforced
/// here so the daemon never sees a ChangeRequested with nothing to
/// feed back to a retry Implementer).
#[instrument(level = "debug", skip_all, fields(raw_chars = raw.len()), err)]
pub fn parse_verdict(raw: &str) -> Result<Verdict, ParseError> {
    let stripped = strip_markdown_fences(raw);
    if let Some(v) = try_parse_full(stripped) {
        return validate_verdict(v);
    }
    if let Some(substr) = extract_object_substring(stripped) {
        if let Ok(v) = serde_json::from_str::<Verdict>(substr) {
            return validate_verdict(v);
        }
        // Valid JSON-looking substring but wrong shape: schema error.
        if serde_json::from_str::<serde_json::Value>(substr).is_ok() {
            return Err(ParseError::Schema(format!(
                "extracted substring is JSON but not a Verdict: {}",
                truncate_for_error(substr)
            )));
        }
    }
    // Final distinction: if the whole stripped body is valid JSON of
    // some shape, it's a Schema failure; otherwise Unparseable.
    match serde_json::from_str::<serde_json::Value>(stripped) {
        Ok(_) => Err(ParseError::Schema(format!(
            "body is JSON but not a Verdict: {}",
            truncate_for_error(stripped)
        ))),
        Err(e) => Err(ParseError::Unparseable(e.to_string())),
    }
}

fn try_parse_full(raw: &str) -> Option<Verdict> {
    serde_json::from_str::<Verdict>(raw).ok()
}

fn validate_verdict(v: Verdict) -> Result<Verdict, ParseError> {
    if let Verdict::ChangeRequested { ref reasons, .. } = v
        && reasons.is_empty()
    {
        return Err(ParseError::Schema(
            "change_requested verdict must have at least one issue in `reasons`".to_string(),
        ));
    }
    Ok(v)
}

fn strip_markdown_fences(raw: &str) -> &str {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("```json")
        && let Some(inner) = rest.trim_start().strip_suffix("```")
    {
        return inner.trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```")
        && let Some(inner) = rest.trim_start().strip_suffix("```")
    {
        return inner.trim();
    }
    trimmed
}

/// Extract the first well-balanced `{ ... }` substring. Mirrors the
/// actions-parser's balanced-bracket scan but for objects, not
/// arrays. Respects string-literal quoting so braces inside quoted
/// values don't throw off depth.
fn extract_object_substring(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&input[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn truncate_for_error(s: &str) -> String {
    const CAP: usize = 200;
    if s.len() <= CAP {
        return s.to_string();
    }
    let mut cut = CAP;
    while !s.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}

// ---------------------------------------------------------------------------
// Phase 5: per-AC observability (events-only)
// ---------------------------------------------------------------------------

/// Synthesized per-AC result for events.log emission. Not persisted;
/// not part of the `Verdict` shape.
struct AcResult<'a> {
    criterion: &'a str,
    status: &'static str,
    evidence: String,
}

/// Emit one `reviewer.ac` debug event per AC plus a roll-up info
/// event with the four counts. Synthesized from the `Verdict` +
/// `Work.acceptance_criteria` because today's reviewer prompt does
/// not enumerate per-AC outcomes in a structured form. Per-AC info
/// is observability, not a domain record.
///
/// Status mapping:
/// - `Accept` -> every AC: `verified`.
/// - `ChangeRequested` -> ACs whose text appears in any reason's
///   `message` are `failed` (with the reason's location/message as
///   evidence); the rest are `not_applicable` (we can't tell from
///   the freeform output whether they were verified or skipped).
/// - `Reject` -> every AC: `not_applicable` (the rejection reason
///   is at the verdict level, not per-AC).
fn emit_ac_events(verdict: &Verdict, work: &Work, bundle_id: &domain::BundleId, path_deny_patterns: &[String]) {
    let acs: &[String] = &work.acceptance_criteria.0;
    if acs.is_empty() {
        // No ACs declared; skip the per-AC events. The Verdict-level
        // accept/reject decision still drives the FSM.
        return;
    }

    let results: Vec<AcResult<'_>> = match verdict {
        Verdict::Accept { .. } => acs
            .iter()
            .map(|c| AcResult {
                criterion: c.as_str(),
                status: "verified",
                evidence: String::new(),
            })
            .collect(),
        Verdict::ChangeRequested { reasons, .. } => acs
            .iter()
            .map(|c| {
                let lower = c.to_lowercase();
                let matching: Vec<&ReviewIssue> =
                    reasons.iter().filter(|r| issue_mentions_criterion(r, &lower)).collect();
                if matching.is_empty() {
                    AcResult {
                        criterion: c.as_str(),
                        status: "not_applicable",
                        evidence: String::new(),
                    }
                } else {
                    let evidence = matching
                        .iter()
                        .map(|r| {
                            let location = match r.line {
                                Some(l) => format!("{}:{}", r.file, l),
                                None => r.file.clone(),
                            };
                            format!("{location}: {}", r.message)
                        })
                        .collect::<Vec<_>>()
                        .join("; ");
                    AcResult {
                        criterion: c.as_str(),
                        status: "failed",
                        evidence,
                    }
                }
            })
            .collect(),
        Verdict::Reject { .. } => acs
            .iter()
            .map(|c| AcResult {
                criterion: c.as_str(),
                status: "not_applicable",
                evidence: String::new(),
            })
            .collect(),
    };

    let mut verified = 0u64;
    let mut failed = 0u64;
    let mut skipped = 0u64;
    for r in &results {
        let evidence_redacted = telemetry::transcript::redact_paths(&r.evidence, path_deny_patterns);
        debug!(
            target: "reviewer.ac",
            bundle_id = %bundle_id,
            work_id = %work.id,
            criterion = %r.criterion,
            status = r.status,
            evidence = %evidence_redacted,
            "reviewer: ac evaluated"
        );
        match r.status {
            "verified" => verified += 1,
            "failed" => failed += 1,
            "not_applicable" | "skipped" => skipped += 1,
            _ => {}
        }
    }

    info!(
        bundle_id = %bundle_id,
        work_id = %work.id,
        ac_count = results.len(),
        ac_verified = verified,
        ac_skipped = skipped,
        ac_failed = failed,
        "reviewer: ac roll-up"
    );
}

/// Heuristic match: a `ReviewIssue` "mentions" an AC if any
/// whitespace-delimited word of length >= 4 from the AC's lowercase
/// form appears in the issue's lowercase `message`. Length floor
/// rules out filler (`is`, `the`, `and`); whole-word match avoids
/// "test" matching "fastest". This is best-effort — the whole point
/// of synthesizing AC results is that the LLM's response doesn't
/// structure them, so the mapping is necessarily fuzzy.
fn issue_mentions_criterion(issue: &ReviewIssue, criterion_lower: &str) -> bool {
    let message_lower = issue.message.to_lowercase();
    criterion_lower
        .split_whitespace()
        .filter(|w| w.len() >= 4)
        .any(|w| message_lower.contains(w))
}

// ---------------------------------------------------------------------------
// Verification rendering
// ---------------------------------------------------------------------------

fn render_verification(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Accept { summary } => format!("Reviewer approved: {summary}"),
        Verdict::ChangeRequested { summary, reasons } => render_issue_summary(summary, reasons),
        Verdict::Reject { reason } => format!("Rejected: {reason}"),
    }
}

/// Render a `ChangeRequested` as a compact multi-line string for
/// `Bundle.verification`. Caller enforces `VERIFICATION_CAP`.
pub fn render_issue_summary(summary: &str, reasons: &[ReviewIssue]) -> String {
    use std::fmt::Write;

    let mut out = format!("Changes requested: {summary}");
    for issue in reasons {
        let severity = match issue.severity {
            domain::Severity::Error => "error",
            domain::Severity::Warning => "warning",
            domain::Severity::Info => "info",
        };
        let location = match issue.line {
            Some(l) => format!("{}:{l}", issue.file),
            None => issue.file.clone(),
        };
        let _ = write!(out, "\n  [{severity}] {location}: {}", issue.message);
        if let Some(sug) = &issue.suggestion {
            let _ = write!(out, " (suggestion: {sug})");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// git helpers
// ---------------------------------------------------------------------------

/// Run `git -C <target> show --format=medium --no-color --patch <sha> -- <paths...>`.
/// Returns stdout as a String. `paths` may be empty (full commit
/// diff). Non-zero exit returns `ReviewerError::Git(stderr)`.
#[instrument(level = "debug", skip_all, fields(target = %target.display(), head_commit, path_count = paths.len()), err)]
pub async fn git_show(target: &Path, head_commit: &str, paths: &[String]) -> Result<String, ReviewerError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(target);
    cmd.args(["show", "--format=medium", "--no-color", "--patch", head_commit]);
    if !paths.is_empty() {
        cmd.arg("--");
        for p in paths {
            cmd.arg(p);
        }
    }
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await?;
    if !out.status.success() {
        return Err(ReviewerError::Git(format!(
            "git show {head_commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Locate the first `diff --git ` line and return the slice from
/// there to end. Everything before is the commit header (hash,
/// author, date, message). If the marker is absent, returns "" —
/// the prompt's empty-body structural-corruption branch handles
/// that case.
pub fn strip_commit_header(raw: &str) -> &str {
    const MARKER: &str = "diff --git ";
    match raw.find(MARKER) {
        Some(idx) => &raw[idx..],
        None => "",
    }
}

/// Truncate a diff body at `cap` bytes (UTF-8 boundary safe) and
/// append an explicit marker. Caller chooses `cap` from
/// `ReviewerConfig::diff_byte_cap`. When the body is shorter than
/// the cap, returns the body verbatim.
pub fn truncate_diff(body: &str, cap: usize) -> String {
    if body.len() <= cap {
        return body.to_string();
    }
    let mut cut = cap;
    while !body.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!(
        "{}\n[... diff truncated; original {} bytes, shown first {cut} bytes]\n",
        &body[..cut],
        body.len()
    )
}

// ---------------------------------------------------------------------------
// File-read helpers (noop bundles)
// ---------------------------------------------------------------------------

/// Read each path under `target`, applying per-file and aggregate
/// caps. Aggregate cap is `cap` bytes; per-file cap is
/// `cap / paths.len().max(1)` with a 2048-byte floor. Missing files
/// render as `(file not found)`; on an I/O error other than missing,
/// the entry renders `(read error: ...)` so a single unreadable path
/// does not fail the whole invocation.
#[instrument(level = "debug", skip_all, fields(target = %target.display(), path_count = paths.len(), cap), err)]
pub async fn read_file_contents(
    target: &Path,
    paths: &[String],
    cap: usize,
) -> Result<Vec<(String, String)>, ReviewerError> {
    let per_file_cap = (cap / paths.len().max(1)).max(2048);
    let mut out: Vec<(String, String)> = Vec::with_capacity(paths.len());
    let mut aggregate: usize = 0;
    for path in paths {
        if aggregate >= cap {
            out.push((
                path.clone(),
                "(aggregate cap exhausted before reaching this file)".to_string(),
            ));
            continue;
        }
        let full = target.join(path);
        let contents = match tokio::fs::read(&full).await {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                out.push((path.clone(), "(file not found)".to_string()));
                continue;
            }
            Err(e) => {
                out.push((path.clone(), format!("(read error: {e})")));
                continue;
            }
        };
        let remaining = cap.saturating_sub(aggregate);
        let effective_cap = per_file_cap.min(remaining);
        let rendered = cap_str(&contents, effective_cap);
        aggregate = aggregate.saturating_add(rendered.len());
        out.push((path.clone(), rendered));
    }
    Ok(out)
}

/// UTF-8 boundary-safe truncation with an explicit marker. Identical
/// shape to `context::implementer`'s `cap_chars`, duplicated here to
/// keep `context` free of the reviewer's rendering helpers.
fn cap_str(input: &str, cap: usize) -> String {
    if input.len() <= cap {
        return input.to_string();
    }
    let mut cut = cap;
    while !input.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!(
        "{}\n[... truncated; original {} bytes, shown first {cut} bytes]\n",
        &input[..cut],
        input.len()
    )
}

#[cfg(test)]
mod tests;
