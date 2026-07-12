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
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

use context::ContextBuilder;
use domain::{
    Bundle, BundleStatus, CheckRun, CheckRunId, CriterionResult, CriterionStatus, Review, ReviewIssue, Role, Severity,
    TargetKind, Verdict, Work, verdict_kind,
};
use llm::{LlmClient, Message};
use store::{BundleUpdateError, BundleUpdateSink, CheckRunSink, ReviewSink};
use telemetry::transcript::{TranscriptIteration, append_iteration, reviewer_path};

use crate::check::CheckRunner;
use crate::config::ReviewerConfig;
use crate::retry::{RetryPolicy, with_llm_retry};

/// Bytes of combined check output retained as the persisted `CheckRun`
/// excerpt (a tail; the full output's sha256 is the tamper-evident digest).
const CHECK_EXCERPT_CAP: usize = 4096;

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
    /// A configured check could not be SPAWNED (command not found / exec
    /// failure). Phase 10 failure taxonomy: this is an ENVIRONMENT problem,
    /// not a code signal — the daemon Blocks the Work (no ChangeRequested,
    /// no LLM turn burning `max_work_attempts`). The message names the
    /// offending command so `blocked_reason` is diagnosable.
    #[error("check environment failure: command {command:?} could not be spawned: {detail}")]
    CheckEnvironment { command: String, detail: String },
    /// Persisting a `CheckRun` record failed.
    #[error("check-run persist failed: {0}")]
    CheckPersist(String),
    /// Persisting the `Review` record failed (Phase 11).
    #[error("review persist failed: {0}")]
    ReviewPersist(String),
}

/// The one-line human summary carried on the persisted `Review` (mirrors the
/// verdict's own text). Accept/ChangeRequested carry `summary`; Reject carries
/// its `reason`.
fn verdict_summary(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Accept { summary } => summary.clone(),
        Verdict::ChangeRequested { summary, .. } => summary.clone(),
        Verdict::Reject { reason } => reason.clone(),
    }
}

/// The structured per-issue reasons persisted on the `Review`. Only a
/// `ChangeRequested` verdict carries a non-empty set; Accept and Reject
/// persist an empty Vec (a Reject's rationale lives in `summary`).
fn verdict_reasons(verdict: &Verdict) -> Vec<ReviewIssue> {
    match verdict {
        Verdict::ChangeRequested { reasons, .. } => reasons.clone(),
        Verdict::Accept { .. } | Verdict::Reject { .. } => Vec::new(),
    }
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
    S: BundleUpdateSink + CheckRunSink + ReviewSink,
    C: ContextBuilder,
{
    pub llm: L,
    pub store: S,
    pub context: C,
    pub config: ReviewerConfig,
    /// Target repo path. Used for `git show <head_commit> -- <paths>` (the
    /// commits live in the shared object DB, reachable from the main repo).
    /// Daemon-constant; cheap to clone on `Deps` construction.
    pub target: PathBuf,
    /// The Bundle's checkout: the Work's warm implementer worktree (lifetime
    /// extended to outlive review so build caches stay warm), or an ephemeral
    /// recreate from the bundle branch on the crash-recovery path. This is
    /// where executed checks RUN, and (Phase 10 fix) where noop-bundle file
    /// contents are READ from — previously the noop path incorrectly read
    /// from `target`. Daemon-supplied.
    pub checkout_path: PathBuf,
    /// True when `checkout_path` is an ephemeral recreate (the warm worktree
    /// was missing, e.g. after a crash). Flagged into each persisted
    /// `CheckRun` excerpt so an operator knows the caches were cold.
    pub ephemeral_checkout: bool,
    /// Executed-check runner (Phase 10). `Arc<dyn CheckRunner>` per the
    /// design doc's API Design; runs `config.check_commands` in
    /// `checkout_path` before the LLM turn.
    pub check_runner: Arc<dyn CheckRunner>,
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
    S: BundleUpdateSink + CheckRunSink + ReviewSink,
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

    // Phase 10: executed checks run BEFORE the LLM turn. A spawn-level failure
    // aborts here (env problem -> Blocked, no LLM). Otherwise every command's
    // outcome is persisted as a `CheckRun` and the red set is carried to the
    // code-gate below.
    let (check_evidence, red_checks, check_run_ids) = run_bundle_checks(deps, bundle, work).await?;

    let (diff, noop_files) = match bundle.head_commit.as_deref() {
        Some(head) => {
            // Finding 4: review the full base..head range so every commit
            // on the branch is seen, not just the final one. Fall back to
            // `git show <head>` for bundles with no recorded base (noop
            // bundles, or rows persisted before base_commit existed).
            let raw = match bundle.base_commit.as_deref() {
                Some(base) => git_diff_range(&deps.target, base, head, &bundle.paths).await?,
                None => git_show(&deps.target, head, &bundle.paths).await?,
            };
            let body = strip_commit_header(&raw);
            let truncated = truncate_diff(body, deps.config.diff_byte_cap);
            (truncated, None)
        }
        None => {
            // Phase 10 fix: read noop-bundle file contents from the Bundle's
            // CHECKOUT (the worktree where the implementer wrote them), not
            // from `deps.target` (the main repo, where an uncommitted noop
            // change does not exist).
            let files = read_file_contents(&deps.checkout_path, &bundle.paths, deps.config.noop_files_byte_cap).await?;
            (String::new(), Some(files))
        }
    };

    let noop_slice = noop_files.as_deref();
    let assembled = deps.context.build_for_reviewer(bundle, work, &diff, noop_slice)?;

    // Append the executed-check evidence to the user message (fenced via the
    // context crate's dynamic-fence helper inside `check_evidence`).
    let base_user = assembled.first_user_text().unwrap_or_default();
    let user_text = if check_evidence.is_empty() {
        base_user.to_string()
    } else {
        format!("{base_user}\n\n{check_evidence}")
    };

    let (verdict, model) = call_llm_with_retry(&deps.llm, &deps.config, &assembled.system_prompt, &user_text).await?;

    // Phase 10 code-gate: an LLM `Accept` while ANY check is red is overridden
    // to `ChangeRequested` BEFORE the FSM transition, with a synthesized
    // `ReviewIssue` naming each red command. The LLM never gets the final word
    // over an exit code.
    let verdict = apply_code_gate(verdict, &red_checks);

    // Phase 8: synthesize one `CriterionResult` per acceptance criterion
    // from the (post-code-gate) Verdict, keyed on the criterion's stable id,
    // emit the per-criterion + roll-up events, and hold the results.
    let criteria_results = evaluate_and_emit_criteria(&verdict, work, &bundle.id, &deps.path_deny_patterns);

    // Phase 11: persist the Review record BEFORE the Bundle transition. Round
    // is `prior review count for this bundle + 1` (append-only history, no
    // dedup). A crash between this persist and the Bundle transition below is
    // benign: the reconcile sweep re-reviews and appends a fresh round. The
    // record links the CheckRuns this round weighed, carries the structured
    // reasons (fed back to a retry Implementer), the per-criterion results, and
    // the concrete model the provider echoed.
    let round = match deps.store.list_reviews_by_bundle(&bundle.id).await {
        Ok(prior) => prior.len() as u32 + 1,
        Err(e) => {
            warn!(error = %e, bundle_id = %bundle.id, "reviewer: prior-review count failed; defaulting round to 1");
            1
        }
    };
    let mut review = Review::new(
        bundle.id.clone(),
        round,
        verdict.clone(),
        verdict_summary(&verdict),
        verdict_reasons(&verdict),
        check_run_ids,
        model.unwrap_or_default(),
    );
    review.criteria = criteria_results;
    let review_id = deps
        .store
        .create_review(review)
        .await
        .map_err(|e| ReviewerError::ReviewPersist(e.to_string()))?;
    info!(
        bundle_id = %bundle.id,
        work_id = %work.id,
        review_id = %review_id,
        round,
        verdict = verdict_kind(&verdict),
        "reviewer: Review record persisted"
    );

    // Best-effort transcript write. Reviewer is single-turn (modulo
    // parse-retry sub-loop, which we collapse to "the verdict that
    // came out"). Failures emit a warn and continue.
    let mut iter = TranscriptIteration::new_single_turn(String::new(), String::new());
    iter.system_prompt = assembled.system_prompt.clone();
    iter.user_prompt = user_text.clone();
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

    // Phase 9: the store chokepoint re-validates this Reviewer-role normal
    // edge (Triaged -> Reviewed / Rejected) that `transition` just accepted.
    deps.store
        .update(bundle, expected_updated_at, Role::Reviewer, TargetKind::Normal)
        .await?;

    Ok(verdict)
}

// ---------------------------------------------------------------------------
// Phase 10: executed checks
// ---------------------------------------------------------------------------

/// A configured check that spawned cleanly but exited nonzero — a code signal
/// the deterministic accept gate acts on.
struct RedCheck {
    command: String,
    exit_code: i32,
    excerpt: String,
}

/// Run the configured `check_commands` in the Bundle's checkout, persist one
/// `CheckRun` per command, and return `(prompt_evidence, red_checks)`.
///
/// Empty `check_commands` is a no-op (checks skipped, verdict proceeds
/// LLM-only). A SPAWN-level failure on ANY command returns
/// `ReviewerError::CheckEnvironment` immediately — the caller has not yet
/// called the LLM, so the "no LLM turn on an environment failure" invariant
/// holds structurally.
async fn run_bundle_checks<L, S, C>(
    deps: &ReviewerDeps<L, S, C>,
    bundle: &Bundle,
    work: &Work,
) -> Result<(String, Vec<RedCheck>, Vec<CheckRunId>), ReviewerError>
where
    L: LlmClient,
    S: BundleUpdateSink + CheckRunSink + ReviewSink,
    C: ContextBuilder,
{
    if deps.config.check_commands.is_empty() {
        return Ok((String::new(), Vec::new(), Vec::new()));
    }

    let outcomes = deps
        .check_runner
        .run(&deps.checkout_path, &deps.config.check_commands)
        .await;

    // Environment gate: any spawn-level failure aborts the review before the
    // LLM turn. Named command -> diagnosable `blocked_reason`.
    for outcome in &outcomes {
        if let Some(detail) = &outcome.spawn_error {
            warn!(
                bundle_id = %bundle.id,
                work_id = %work.id,
                command = %outcome.command,
                detail = %detail,
                "reviewer: check spawn-level failure (environment); Work will Block, no LLM turn"
            );
            return Err(ReviewerError::CheckEnvironment {
                command: outcome.command.clone(),
                detail: detail.clone(),
            });
        }
    }

    // Every command spawned cleanly: persist evidence and partition red/green.
    // Collect the persisted CheckRun ids so the Review record (Phase 11) can
    // reference exactly the runs this round weighed.
    let mut red_checks = Vec::new();
    let mut check_run_ids = Vec::new();
    for outcome in &outcomes {
        let tail = tail_capped(&outcome.combined_output, CHECK_EXCERPT_CAP);
        let excerpt = if deps.ephemeral_checkout {
            format!("[ephemeral checkout: warm worktree missing; recreated from bundle branch]\n{tail}")
        } else {
            tail
        };
        let digest = hex_encode(&Sha256::digest(outcome.combined_output.as_bytes()));
        let record = CheckRun::new(
            bundle.id.clone(),
            work.id.clone(),
            outcome.command.clone(),
            outcome.exit_code,
            digest,
            excerpt.clone(),
            Role::Reviewer,
            outcome.duration_ms,
        );
        let check_run_id = deps
            .store
            .create_check_run(record)
            .await
            .map_err(|e| ReviewerError::CheckPersist(e.to_string()))?;
        check_run_ids.push(check_run_id);
        if outcome.is_red() {
            red_checks.push(RedCheck {
                command: outcome.command.clone(),
                exit_code: outcome.exit_code,
                excerpt,
            });
        }
    }

    let evidence = render_check_evidence(&outcomes, deps.ephemeral_checkout);
    debug!(
        bundle_id = %bundle.id,
        work_id = %work.id,
        check_count = outcomes.len(),
        red_count = red_checks.len(),
        ephemeral = deps.ephemeral_checkout,
        "reviewer: executed checks complete"
    );
    Ok((evidence, red_checks, check_run_ids))
}

/// Deterministic accept gate: an `Accept` verdict is overridden to
/// `ChangeRequested` when any check is red. Non-`Accept` verdicts pass
/// through unchanged (a red check only tightens the verdict, never loosens).
fn apply_code_gate(verdict: Verdict, red_checks: &[RedCheck]) -> Verdict {
    if red_checks.is_empty() {
        return verdict;
    }
    match verdict {
        Verdict::Accept { summary } => {
            let commands = red_checks
                .iter()
                .map(|r| r.command.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            warn!(
                red_count = red_checks.len(),
                commands = %commands,
                "reviewer: code-gate overriding Accept -> ChangeRequested over red check(s)"
            );
            let reasons = red_checks.iter().map(synthesized_issue).collect();
            Verdict::ChangeRequested {
                summary: format!(
                    "Reviewer accepted, but {} configured check(s) failed ({commands}); the accept is \
                     overridden by the executed-check gate. LLM summary: {summary}",
                    red_checks.len()
                ),
                reasons,
            }
        }
        other => other,
    }
}

/// Synthesize a `ReviewIssue` naming a red command, carrying its exit code
/// and output tail so the retry Implementer (and the operator) sees what
/// failed.
fn synthesized_issue(red: &RedCheck) -> ReviewIssue {
    ReviewIssue {
        severity: Severity::Error,
        file: red.command.clone(),
        line: None,
        message: format!(
            "executed check `{}` failed with exit code {}: {}",
            red.command, red.exit_code, red.excerpt
        ),
        suggestion: None,
    }
}

/// Render the executed-check evidence block for the reviewer prompt. Each
/// command's output is fenced with the context crate's dynamic-fence helper
/// so untrusted output cannot escape into instruction position.
fn render_check_evidence(outcomes: &[crate::check::CheckOutcome], ephemeral: bool) -> String {
    use std::fmt::Write;
    let mut out = String::from(
        "## Executed checks (evidence)\n\nThese checks were executed against the bundle checkout BEFORE this \
         review. Their exit codes are authoritative: an accept over a red (nonzero) check is overridden to \
         change_requested by the harness — do not accept a bundle whose checks are red.\n",
    );
    if ephemeral {
        out.push_str(
            "\n> Note: the warm worktree was missing; these checks ran in an EPHEMERAL checkout recreated from \
             the bundle branch (build caches were cold).\n",
        );
    }
    for outcome in outcomes {
        let _ = write!(out, "\n### `{}` — exit {}\n", outcome.command, outcome.exit_code);
        let tail = tail_capped(&outcome.combined_output, CHECK_EXCERPT_CAP);
        out.push_str(&context::dynamic_fence(&tail));
    }
    out
}

/// Retain the last `cap` bytes of `s` (char-boundary safe), prefixed with an
/// omission marker when truncated.
fn tail_capped(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("[... {start} earlier bytes omitted]\n{}", &s[start..])
}

/// Lowercase-hex encode a byte slice (avoids a `hex` crate dependency for the
/// one sha256 digest site).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
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
) -> Result<(Verdict, Option<String>), ReviewerError>
where
    L: LlmClient,
{
    let mut messages = vec![Message::user(user_message.to_string())];
    let mut requeries: u32 = 0;
    loop {
        let (raw, usage) = with_llm_retry(&RetryPolicy::default(), || {
            llm.complete_free(system_prompt, &messages, None)
        })
        .await?;
        match parse_verdict(&raw) {
            Ok(v) => {
                debug!(requeries, "reviewer verdict parsed");
                // The concrete model the provider echoed on the successful
                // call is persisted on the Review record (pinning audit).
                return Ok((v, usage.model));
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
// Phase 8: per-criterion results keyed on criterion id + events
// ---------------------------------------------------------------------------

/// Synthesize one `CriterionResult` per acceptance criterion from the
/// parsed `Verdict`, emit one `reviewer.ac` debug event per criterion
/// (keyed on the criterion's stable `id`) plus a roll-up info event, and
/// return the results.
///
/// Phase 8 change: results are keyed on `Criterion::id`, NOT on a fuzzy
/// word-match of the criterion text against the freeform review reasons
/// (that heuristic — `issue_mentions_criterion` — was dropped). Until the
/// Reviewer prompt enumerates per-criterion outcomes structurally
/// (Phase 10), the verdict maps coarsely:
/// - `Accept` -> every criterion `Met`.
/// - `ChangeRequested` -> every criterion `Unmet`, carrying the verdict
///   `summary` as evidence.
/// - `Reject` -> every criterion `Unmet`, carrying the `reason` as evidence.
///
/// The returned `Vec<CriterionResult>` is what Phase 11 persists onto the
/// `Review` record; this phase produces them and surfaces them in events.
fn evaluate_and_emit_criteria(
    verdict: &Verdict,
    work: &Work,
    bundle_id: &domain::BundleId,
    path_deny_patterns: &[String],
) -> Vec<CriterionResult> {
    if work.acceptance_criteria.is_empty() {
        // No criteria declared; nothing to evaluate. The verdict-level
        // accept/reject decision still drives the FSM.
        return Vec::new();
    }

    let (status, evidence) = match verdict {
        Verdict::Accept { .. } => (CriterionStatus::Met, None),
        Verdict::ChangeRequested { summary, .. } => (CriterionStatus::Unmet, Some(summary.clone())),
        Verdict::Reject { reason } => (CriterionStatus::Unmet, Some(reason.clone())),
    };

    let results: Vec<CriterionResult> = work
        .acceptance_criteria
        .iter()
        .map(|c| CriterionResult {
            criterion_id: c.id,
            status,
            evidence: evidence.clone(),
        })
        .collect();

    let mut met = 0u64;
    let mut unmet = 0u64;
    for (criterion, result) in work.acceptance_criteria.iter().zip(results.iter()) {
        let status_str = match result.status {
            CriterionStatus::Met => "met",
            CriterionStatus::Unmet => "unmet",
        };
        let evidence_display = result.evidence.as_deref().unwrap_or("");
        let evidence_redacted = telemetry::transcript::redact_paths(evidence_display, path_deny_patterns);
        debug!(
            target: "reviewer.ac",
            bundle_id = %bundle_id,
            work_id = %work.id,
            criterion_id = criterion.id,
            criterion = %criterion.text,
            status = status_str,
            evidence = %evidence_redacted,
            "reviewer: ac evaluated"
        );
        match result.status {
            CriterionStatus::Met => met += 1,
            CriterionStatus::Unmet => unmet += 1,
        }
    }

    info!(
        bundle_id = %bundle_id,
        work_id = %work.id,
        ac_count = results.len(),
        ac_met = met,
        ac_unmet = unmet,
        "reviewer: ac roll-up"
    );

    results
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

/// Maximum number of structured reasons rendered into a retry Implementer's
/// feedback (`StateSummary.rejected_bundle_reason`). The full set stays on the
/// persisted `Review` record; the prompt shows the first N with an omission
/// marker so a pathological review can't blow up the context.
pub const REJECTION_REASONS_CAP: usize = 20;

/// Render a persisted `Review`'s STRUCTURED feedback for a retry Implementer's
/// prompt (Phase 11). Uses the Review's `summary` + structured `reasons` — NOT
/// the one-line `Bundle.verification` string — so the Implementer sees the
/// issues the Reviewer raised, one by one. The reason list is capped at
/// `REJECTION_REASONS_CAP` with an omission marker; the full record stays on
/// disk. Returns `None` when the Review carries no usable feedback (empty
/// summary and no reasons), letting the caller fall through to no feedback.
pub fn render_review_feedback(review: &Review) -> Option<String> {
    use std::fmt::Write;

    let summary = review.summary.trim();
    if summary.is_empty() && review.reasons.is_empty() {
        return None;
    }
    let mut out = if summary.is_empty() {
        format!("Reviewer requested changes (round {})", review.round)
    } else {
        format!("Reviewer requested changes (round {}): {summary}", review.round)
    };
    let shown = review.reasons.len().min(REJECTION_REASONS_CAP);
    for issue in review.reasons.iter().take(shown) {
        let severity = match issue.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
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
    if review.reasons.len() > shown {
        let _ = write!(
            out,
            "\n  [... {} more reason(s) omitted; full detail in reviews.jsonl]",
            review.reasons.len() - shown
        );
    }
    Some(out)
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

/// Run `git -C <target> diff --no-color <base>..<head> -- <paths...>`.
/// Used for multi-commit bundles so the reviewer sees the cumulative
/// diff across ALL the implementer's commits (finding 4) - what the
/// integrator actually merges - not just `git show <head>`, which is the
/// final commit alone and leaves earlier commits unreviewed.
#[instrument(
    level = "debug",
    skip_all,
    fields(target = %target.display(), base_commit, head_commit, path_count = paths.len()),
    err
)]
pub async fn git_diff_range(
    target: &Path,
    base_commit: &str,
    head_commit: &str,
    paths: &[String],
) -> Result<String, ReviewerError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(target);
    let range = format!("{base_commit}..{head_commit}");
    cmd.args(["diff", "--no-color", &range]);
    if !paths.is_empty() {
        cmd.arg("--");
        for p in paths {
            cmd.arg(p);
        }
    }
    let out = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output().await?;
    if !out.status.success() {
        return Err(ReviewerError::Git(format!(
            "git diff {range} failed: {}",
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
