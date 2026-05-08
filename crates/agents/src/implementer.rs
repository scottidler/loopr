//! `run_implementer`: the Stage-7 Ralph loop for Implementer role.
//!
//! Takes a ready Work + a Worktree + a Deps bundle, runs up to
//! `max_iterations` LLM turns with self-correction on parse/tool
//! failures, and returns a persisted Bundle or an EscalationNeeded.
//!
//! Invariants (enforced by tests):
//! - `Lifeguard::reset_parse_failures` is called ONLY on a
//!   successful parse (`Ok(actions)` branch), NEVER unconditionally.
//! - Force-propose guard escalates (returns Err(EscalationNeeded))
//!   rather than persisting a zombie Bundle with empty head_commit.
//! - `loc_changed` is computed against the worktree's sha at
//!   ProposeBundle time; the dispatcher does that already.
//! - Per-iteration message history is local; cross-iteration context
//!   travels only via `history: Vec<IterationSummary>`.

use std::future::Future;
use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

use context::{ContextBuilder, ITERATION_SUMMARY_CAP, IterationSummary, StateSummary};
use domain::{Bundle, BundleId, Work};
use llm::{Message, LlmClient};
use telemetry::transcript::{TranscriptIteration, append_iteration, implementer_path};
use worktree::Worktree;

use crate::action::AgentAction;
use crate::config::ImplementerConfig;
use crate::dispatch::{ActionResult, DispatchError, ToolExecutor, dispatch_action};
use crate::lifeguard::{Decision, Lifeguard};
use crate::parse::{parse_actions, parse_one};

/// Render a multi-line LLM-visible note when the partition filter
/// dropped paths from a commit / propose. The note restates the Work's
/// scope so the agent can decide whether to re-edit in scope or emit
/// `need_help`. Empty `dropped` is the no-op caller's responsibility:
/// this returns a non-empty string only when there's something to say.
fn format_dropped_note(action_label: &str, dropped: &[String], scope: &[String]) -> String {
    if dropped.is_empty() {
        return String::new();
    }
    format!(
        "note: {n} out-of-scope path(s) were dropped from {action_label} \
because they are not in the Work's `files` scope: {dropped:?}\n\
The Work's scope is: {scope:?}\n\
If you need to edit those files, emit `need_help` with the reason.",
        n = dropped.len(),
    )
}

/// Append one iteration's transcript block. Best-effort: failures emit
/// a warn and continue. Capturing this in a helper lets every return
/// path inside `run_implementer` write the transcript cheaply.
fn write_implementer_transcript(
    target: &Path,
    work_id: &str,
    iteration: u32,
    system_prompt: &str,
    user_prompt: &str,
    raw_response: &str,
    parsed_actions: &[AgentAction],
    dispatcher_outcomes: &[String],
    lifeguard_decision: Option<String>,
) {
    let mut iter = TranscriptIteration::new_single_turn(String::new(), String::new());
    iter.iteration = iteration;
    iter.system_prompt = system_prompt.to_string();
    iter.user_prompt = user_prompt.to_string();
    iter.response = raw_response.to_string();
    iter.parsed_actions = parsed_actions.iter().map(|a| a.kind().to_string()).collect();
    iter.dispatcher_outcomes = dispatcher_outcomes.to_vec();
    iter.lifeguard_decision = lifeguard_decision;
    let path = implementer_path(target, work_id);
    if let Err(e) = append_iteration(&path, &iter) {
        warn!(error = %e, path = %path.display(), "implementer transcript append failed");
    }
}

/// Minimal store-write interface. Abstracting just the
/// `Bundle`-create surface (not the full `Store`) keeps the
/// Implementer's test fakes tiny.
#[allow(clippy::manual_async_fn)]
pub trait BundleSink: Send + Sync {
    fn persist<'a>(&'a self, bundle: Bundle) -> impl Future<Output = Result<BundleId, BundleSinkError>> + Send + 'a;
}

#[derive(Debug, thiserror::Error)]
pub enum BundleSinkError {
    #[error("bundle persistence failed: {0}")]
    Persist(String),
}

/// Real `BundleSink` backed by `store::Store`. Lives in `agents`
/// (not `store`) because the trait is defined here; the impl does
/// not require any change to the `store` crate.
impl BundleSink for store::Store {
    #[allow(clippy::manual_async_fn)]
    fn persist<'a>(&'a self, bundle: Bundle) -> impl Future<Output = Result<BundleId, BundleSinkError>> + Send + 'a {
        async move {
            self.bundles()
                .create(bundle)
                .await
                .map_err(|e| BundleSinkError::Persist(e.to_string()))
        }
    }
}

/// Forwarding `BundleSink` for any reference to a `BundleSink`. Lets the
/// daemon build `Deps { bundles: &self.store, .. }` without cloning the
/// Store (which isn't `Clone`) or wrapping it in `Arc` (which would
/// conflict with the Store shutdown contract requiring unique ownership
/// at `Arc::try_unwrap` time).
impl<B: BundleSink + ?Sized> BundleSink for &B {
    #[allow(clippy::manual_async_fn)]
    fn persist<'a>(&'a self, bundle: Bundle) -> impl Future<Output = Result<BundleId, BundleSinkError>> + Send + 'a {
        async move { (*self).persist(bundle).await }
    }
}

/// Forwarding `BundleSink` for `Arc<B>`. Phase 6 of the Tier-1
/// cleanup wraps `Store` in `Arc<Store>` so the daemon's
/// `SummaryFanout` decorator can clone it; this impl lets the
/// implementer take `bundles: &*self.store` (where `self.store:
/// Arc<Store>`) without unwrapping.
impl<B: BundleSink + ?Sized> BundleSink for std::sync::Arc<B> {
    #[allow(clippy::manual_async_fn)]
    fn persist<'a>(&'a self, bundle: Bundle) -> impl Future<Output = Result<BundleId, BundleSinkError>> + Send + 'a {
        async move { (**self).persist(bundle).await }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImplementerError {
    #[error("escalation needed: {0}")]
    EscalationNeeded(String),
    #[error("llm error: {0}")]
    Llm(#[from] llm::LlmError),
    #[error("bundle sink error: {0}")]
    Sink(#[from] BundleSinkError),
    #[error("context error: {0}")]
    Context(#[from] context::ContextError),
    #[error("dispatch error: {0}")]
    Dispatch(#[from] DispatchError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Dependencies injected into `run_implementer`. Generic over the
/// four trait parameters per the crate's DI rule: one generic flows
/// through the function signature; concrete trait bounds live here.
pub struct Deps<L, T, S, C>
where
    L: LlmClient,
    T: ToolExecutor,
    S: BundleSink,
    C: ContextBuilder,
{
    pub llm: L,
    pub tools: T,
    pub bundles: S,
    pub context: C,
    pub config: ImplementerConfig,
    /// Tool schemas rendered into the system prompt. Snapshot at
    /// loop start; tool registry changes mid-run don't affect an
    /// in-flight loop.
    pub tool_schemas: Vec<tools::ToolSchema>,
    /// Optional state summary (e.g. rejected-bundle reason on a
    /// retry). `None` on first attempts.
    pub state: StateSummary,
}

#[instrument(
    level = "info",
    skip_all,
    fields(
        work_id = %work.id,
        worktree_path = %worktree.path().display(),
        branch = worktree.branch(),
        max_iterations = deps.config.max_iterations,
    ),
    err,
)]
pub async fn run_implementer<L, T, S, C>(
    work: &Work,
    worktree: &Worktree,
    deps: &Deps<L, T, S, C>,
) -> Result<Bundle, ImplementerError>
where
    L: LlmClient,
    T: ToolExecutor,
    S: BundleSink,
    C: ContextBuilder,
{
    let mut history: Vec<IterationSummary> = Vec::new();
    let mut lifeguard = Lifeguard::new(deps.config.max_repeat_action, deps.config.max_parse_failures);

    for iteration in 1..=deps.config.max_iterations {
        info!(iteration, work_id = %work.id, "implementer iteration start");

        let assembled = deps.context.build_for_implementer(
            work,
            worktree.path(),
            &deps.tool_schemas,
            &history,
            &deps.state,
            iteration,
        )?;

        // Track the last raw LLM response of this iteration. The parse-
        // retry sub-loop overwrites it on each call; whichever was
        // last is what the iteration's transcript records. The empty
        // initial assignment is overwritten on the first LLM call
        // below.
        #[allow(unused_assignments)]
        let mut last_raw = String::new();

        // Self-correction sub-loop: parse failures append to this
        // vec and re-prompt; reset_parse_failures ONLY on Ok.
        let mut messages = assembled.messages.clone();
        let actions = loop {
            let (raw, _usage) = deps.llm.complete_free(&assembled.system_prompt, &messages).await?;
            last_raw = raw.clone();
            match parse_actions(&raw) {
                Ok(actions) => {
                    lifeguard.reset_parse_failures();
                    break actions;
                }
                Err(e) => {
                    warn!(iteration, error = %e, "parse failure in sub-loop");
                    messages.push(Message::assistant(raw));
                    messages.push(Message::user(format!(
                        "parse failed: {e}. Return one JSON array of actions."
                    )));
                    let requeries_used = (messages.len() - 1) / 2;
                    if requeries_used as u32 >= deps.config.max_requeries {
                        if let Decision::Escalate(reason) = lifeguard.record_parse_failure() {
                            write_implementer_transcript(
                                worktree.repo_path(),
                                work.id.as_ref(),
                                iteration,
                                &assembled.system_prompt,
                                &assembled.first_user_text().unwrap_or_default(),
                                &last_raw,
                                &[],
                                &[],
                                Some(format!("Escalate (parse-failures): {reason}")),
                            );
                            return Err(ImplementerError::EscalationNeeded(reason));
                        }
                        break Vec::new();
                    }
                }
            }
        };

        if actions.is_empty() {
            write_implementer_transcript(
                worktree.repo_path(),
                work.id.as_ref(),
                iteration,
                &assembled.system_prompt,
                &assembled.first_user_text().unwrap_or_default(),
                &last_raw,
                &[],
                &["(all parse attempts failed this iteration)".to_string()],
                None,
            );
            history.push(IterationSummary {
                iteration,
                actions_summary: "(all parse attempts failed this iteration)".to_string(),
            });
            continue;
        }

        // Action loop. Correctable tool errors re-prompt once via
        // parse_one against the same local messages vec.
        let mut summaries: Vec<String> = Vec::new();
        let mut broke_loop = false;
        let parsed_actions_snapshot = actions.clone();
        for action in actions {
            if let Decision::Escalate(reason) = lifeguard.check_action(&action) {
                write_implementer_transcript(
                    worktree.repo_path(),
                    work.id.as_ref(),
                    iteration,
                    &assembled.system_prompt,
                    &assembled.first_user_text().unwrap_or_default(),
                    &last_raw,
                    &parsed_actions_snapshot,
                    &summaries,
                    Some(format!("Escalate (lifeguard): {reason}")),
                );
                return Err(ImplementerError::EscalationNeeded(reason));
            }
            let result = dispatch_action(action.clone(), work, worktree, &deps.tools).await?;
            match result {
                ActionResult::BundleCreated { mut bundle, dropped } => {
                    summaries.push("propose_bundle (id pending persistence)".to_string());
                    let note = format_dropped_note("propose_bundle", &dropped, &work.files);
                    if !note.is_empty() {
                        summaries.push(note);
                    }
                    write_implementer_transcript(
                        worktree.repo_path(),
                        work.id.as_ref(),
                        iteration,
                        &assembled.system_prompt,
                        &assembled.first_user_text().unwrap_or_default(),
                        &last_raw,
                        &parsed_actions_snapshot,
                        &summaries,
                        None,
                    );
                    let id = deps.bundles.persist(bundle.clone()).await?;
                    bundle.id = id;
                    return Ok(bundle);
                }
                ActionResult::Done(mut bundle) => {
                    summaries.push("done".to_string());
                    write_implementer_transcript(
                        worktree.repo_path(),
                        work.id.as_ref(),
                        iteration,
                        &assembled.system_prompt,
                        &assembled.first_user_text().unwrap_or_default(),
                        &last_raw,
                        &parsed_actions_snapshot,
                        &summaries,
                        None,
                    );
                    let id = deps.bundles.persist(bundle.clone()).await?;
                    bundle.id = id;
                    return Ok(bundle);
                }
                ActionResult::NeedHelp(reason) => {
                    summaries.push(format!("need_help: {reason}"));
                    write_implementer_transcript(
                        worktree.repo_path(),
                        work.id.as_ref(),
                        iteration,
                        &assembled.system_prompt,
                        &assembled.first_user_text().unwrap_or_default(),
                        &last_raw,
                        &parsed_actions_snapshot,
                        &summaries,
                        Some(format!("Escalate (need_help): {reason}")),
                    );
                    return Err(ImplementerError::EscalationNeeded(reason));
                }
                ActionResult::Committed { sha, dropped } => {
                    summaries.push(format!("committed {sha}"));
                    let note = format_dropped_note("this commit", &dropped, &work.files);
                    if !note.is_empty() {
                        summaries.push(note);
                    }
                }
                ActionResult::NothingToCommit { dropped } => {
                    if dropped.is_empty() {
                        summaries.push("commit_changes: nothing to commit".to_string());
                    } else {
                        summaries
                            .push("commit_changes: nothing to commit (all dirty paths were out of scope)".to_string());
                        let note = format_dropped_note("this commit", &dropped, &work.files);
                        summaries.push(note);
                    }
                }
                ActionResult::ToolOutput(out) => {
                    summaries.push(out);
                }
                ActionResult::Error(err_msg) => {
                    warn!(iteration, error = %err_msg, "correctable tool error; re-prompting");
                    messages.push(Message::assistant(
                        serde_json::to_string(&action).unwrap_or_else(|_| "{}".into()),
                    ));
                    messages.push(Message::user(format!(
                        "action failed: {err_msg}. Return one corrected JSON action (single object, not array)."
                    )));
                    let (corrected_raw, _usage) = deps.llm.complete_free(&assembled.system_prompt, &messages).await?;
                    match parse_one(&corrected_raw) {
                        Ok(corrected) => {
                            let corrected_result = dispatch_action(corrected, work, worktree, &deps.tools).await?;
                            match corrected_result {
                                ActionResult::BundleCreated { mut bundle, dropped } => {
                                    summaries.push("corrected -> propose_bundle".to_string());
                                    let note = format_dropped_note("propose_bundle", &dropped, &work.files);
                                    if !note.is_empty() {
                                        summaries.push(note);
                                    }
                                    write_implementer_transcript(
                                        worktree.repo_path(),
                                        work.id.as_ref(),
                                        iteration,
                                        &assembled.system_prompt,
                                        &assembled.first_user_text().unwrap_or_default(),
                                        &last_raw,
                                        &parsed_actions_snapshot,
                                        &summaries,
                                        None,
                                    );
                                    let id = deps.bundles.persist(bundle.clone()).await?;
                                    bundle.id = id;
                                    return Ok(bundle);
                                }
                                ActionResult::Done(mut bundle) => {
                                    summaries.push("corrected -> done".to_string());
                                    write_implementer_transcript(
                                        worktree.repo_path(),
                                        work.id.as_ref(),
                                        iteration,
                                        &assembled.system_prompt,
                                        &assembled.first_user_text().unwrap_or_default(),
                                        &last_raw,
                                        &parsed_actions_snapshot,
                                        &summaries,
                                        None,
                                    );
                                    let id = deps.bundles.persist(bundle.clone()).await?;
                                    bundle.id = id;
                                    return Ok(bundle);
                                }
                                ActionResult::NeedHelp(reason) => {
                                    summaries.push(format!("corrected -> need_help: {reason}"));
                                    write_implementer_transcript(
                                        worktree.repo_path(),
                                        work.id.as_ref(),
                                        iteration,
                                        &assembled.system_prompt,
                                        &assembled.first_user_text().unwrap_or_default(),
                                        &last_raw,
                                        &parsed_actions_snapshot,
                                        &summaries,
                                        Some(format!("Escalate (corrected -> need_help): {reason}")),
                                    );
                                    return Err(ImplementerError::EscalationNeeded(reason));
                                }
                                ActionResult::ToolOutput(out) => {
                                    summaries.push(out);
                                }
                                ActionResult::Committed { sha, dropped } => {
                                    summaries.push(format!("corrected commit {sha}"));
                                    let note = format_dropped_note("this commit", &dropped, &work.files);
                                    if !note.is_empty() {
                                        summaries.push(note);
                                    }
                                }
                                ActionResult::NothingToCommit { dropped } => {
                                    if dropped.is_empty() {
                                        summaries.push("corrected commit_changes: nothing".to_string());
                                    } else {
                                        summaries.push(
                                            "corrected commit_changes: nothing (all dirty paths were out of scope)"
                                                .to_string(),
                                        );
                                        summaries.push(format_dropped_note("this commit", &dropped, &work.files));
                                    }
                                }
                                ActionResult::Error(e2) => {
                                    summaries.push(format!("corrected action also errored: {e2}"));
                                    broke_loop = true;
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            summaries.push(format!("corrected action unparseable: {e}"));
                            broke_loop = true;
                            break;
                        }
                    }
                }
            }
        }

        // Iteration completes naturally (continuing to next iteration).
        // Write a transcript covering this iteration's full story.
        write_implementer_transcript(
            worktree.repo_path(),
            work.id.as_ref(),
            iteration,
            &assembled.system_prompt,
            &assembled.first_user_text().unwrap_or_default(),
            &last_raw,
            &parsed_actions_snapshot,
            &summaries,
            None,
        );

        let summary = cap_str(&summaries.join("\n"), ITERATION_SUMMARY_CAP);
        history.push(IterationSummary {
            iteration,
            actions_summary: summary,
        });

        if broke_loop {
            debug!(iteration, "action loop broke out; continuing outer loop");
        }
    }

    // Iteration cap: force-propose path.
    force_propose(work, worktree, deps).await
}

/// Force-propose path: the LLM produced parseable actions but never
/// emitted `propose_bundle`. Commit any tracked modifications and
/// return a Bundle with `force_proposed: true`. Guard: if the
/// modified-file count or any staged-file size exceeds the configured
/// limits, escalate instead of committing.
#[instrument(
    level = "info",
    skip_all,
    fields(
        work_id = %work.id,
        worktree_path = %worktree.path().display(),
        max_force_propose_files = deps.config.max_force_propose_files,
        max_force_propose_file_size_bytes = deps.config.max_force_propose_file_size_bytes,
    ),
    err,
)]
async fn force_propose<L, T, S, C>(
    work: &Work,
    worktree: &Worktree,
    deps: &Deps<L, T, S, C>,
) -> Result<Bundle, ImplementerError>
where
    L: LlmClient,
    T: ToolExecutor,
    S: BundleSink,
    C: ContextBuilder,
{
    let modified = list_modified_tracked(worktree.path()).await?;
    if modified.len() as u32 > deps.config.max_force_propose_files {
        return Err(ImplementerError::EscalationNeeded(format!(
            "force-propose guard tripped: {} modified files exceeds max {}",
            modified.len(),
            deps.config.max_force_propose_files
        )));
    }
    for file in &modified {
        let full = worktree.path().join(file);
        if let Ok(meta) = tokio::fs::metadata(&full).await
            && meta.len() > deps.config.max_force_propose_file_size_bytes
        {
            return Err(ImplementerError::EscalationNeeded(format!(
                "force-propose guard tripped: {} is {} bytes (max {})",
                file,
                meta.len(),
                deps.config.max_force_propose_file_size_bytes
            )));
        }
    }

    let head_before = rev_parse_head(worktree.path()).await.ok();

    if !modified.is_empty() {
        run_git(worktree.path(), &["add", "-u"]).await?;
        run_git(
            worktree.path(),
            &[
                "commit",
                "--message",
                "force-propose: iteration cap reached",
                "--no-gpg-sign",
            ],
        )
        .await?;
    }

    let head_after = rev_parse_head(worktree.path()).await.ok();
    let loc_changed = if !worktree.sha().is_empty() {
        compute_loc_changed(worktree.path(), worktree.sha()).await.ok()
    } else {
        None
    };

    let mut bundle = Bundle::new(
        work.id.clone(),
        worktree.branch().to_string(),
        vec!["force_proposed: iteration cap reached without propose_bundle".to_string()],
    );
    bundle.force_proposed = true;
    bundle.head_commit = head_after.or(head_before);
    bundle.loc_changed = loc_changed;
    let id = deps.bundles.persist(bundle.clone()).await?;
    bundle.id = id;
    Ok(bundle)
}

#[instrument(level = "trace", skip_all, fields(path = %path.display()), err)]
async fn list_modified_tracked(path: &Path) -> Result<Vec<String>, DispatchError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["ls-files", "--modified"])
        .stdout(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git ls-files --modified failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

#[instrument(level = "trace", skip_all, fields(path = %path.display()), err)]
async fn rev_parse_head(path: &Path) -> Result<String, DispatchError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[instrument(level = "trace", skip_all, fields(path = %path.display(), base_sha = sha), err)]
async fn compute_loc_changed(path: &Path, sha: &str) -> Result<u32, DispatchError> {
    let spec = format!("{sha}..HEAD");
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--numstat", &spec])
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git diff --numstat failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(parse_numstat(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_numstat(s: &str) -> u32 {
    let mut total: u32 = 0;
    for line in s.lines() {
        let mut parts = line.split('\t');
        let insertions = parts.next().unwrap_or("0");
        let deletions = parts.next().unwrap_or("0");
        if insertions == "-" || deletions == "-" {
            continue;
        }
        let ins = insertions.parse::<u32>().unwrap_or(0);
        let del = deletions.parse::<u32>().unwrap_or(0);
        total = total.saturating_add(ins).saturating_add(del);
    }
    total
}

#[instrument(level = "trace", skip_all, fields(path = %path.display(), git_args = ?args), err)]
async fn run_git(path: &Path, args: &[&str]) -> Result<(), DispatchError> {
    let output = Command::new("git").arg("-C").arg(path).args(args).output().await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn cap_str(input: &str, cap: usize) -> String {
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
