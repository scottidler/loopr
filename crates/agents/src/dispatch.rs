//! `dispatch_action`: execute one `AgentAction` in its worktree.
//!
//! Scope: one action in, one `ActionResult` out. No loop, no retry,
//! no LLM re-prompt. The `run_implementer` loop in Phase 4 calls
//! this per action and handles iteration / correction / escalation.
//!
//! Persistence: `ProposeBundle` and `Done` return an unpersisted
//! `Bundle` inside `ActionResult`; the caller persists via
//! `BundlesStore::create`. Keeping dispatch store-free makes it
//! testable without a full taskstore.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::process::Command;
use tracing::{Span, debug, info, instrument, warn};

use domain::{Bundle, Work};
use tools::{BashDenylist, LaneRouter, SandboxMode, ToolContext};
use worktree::Worktree;

use crate::action::AgentAction;
use crate::scope;

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("git command failed: {0}")]
    Git(String),
    #[error("tool execution failed: {0}")]
    Tool(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Outcome of dispatching one action. The `Bundle`-carrying variants
/// hold an unpersisted Bundle; the caller writes to the store.
///
/// `Committed`, `NothingToCommit`, and `BundleCreated` carry a
/// `dropped` field listing out-of-scope paths the partition filter
/// excluded from staging. A non-empty `dropped` is a soft warning the
/// implementer renders to the LLM via the iteration-history summary
/// so the agent can react (re-edit in scope, or emit `need_help`).
#[derive(Debug)]
pub enum ActionResult {
    /// `RunTool` stdout/stderr as a string.
    ToolOutput(String),
    /// `CommitChanges` succeeded. `dropped` lists out-of-scope paths
    /// the partition filter excluded from this commit.
    Committed { sha: String, dropped: Vec<String> },
    /// `CommitChanges` produced no commit. `dropped` lists paths the
    /// partition filter excluded; non-empty `dropped` means there was
    /// dirty content but none of it matched the Work's scope.
    NothingToCommit { dropped: Vec<String> },
    /// `ProposeBundle` constructed a Bundle (not yet persisted).
    /// `dropped` lists out-of-scope paths left uncommitted in the
    /// worktree at propose time.
    BundleCreated { bundle: Bundle, dropped: Vec<String> },
    /// `Done` constructed a no-op Bundle (not yet persisted).
    Done(Bundle),
    /// `NeedHelp` — caller escalates. Partial-work commit (if any)
    /// was attempted by the dispatcher.
    NeedHelp(String),
    /// Correctable failure. The `run_implementer` loop may re-prompt
    /// the LLM with the error string before advancing.
    Error(String),
}

/// Minimal tool-execution abstraction. The real registry lives in
/// `tools`; this trait is what the Implementer sees. Keeps
/// `agents::dispatch_action` testable with fakes.
#[trait_variant::make(Send)]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        working_dir: &Path,
    ) -> Result<String, DispatchError>;
}

#[instrument(
    level = "debug",
    skip_all,
    fields(
        action_kind = action.kind(),
        worktree_path = %worktree.path().display(),
        work_id = %worktree.work_id(),
    ),
    err,
)]
pub async fn dispatch_action<T: ToolExecutor>(
    action: AgentAction,
    work: &Work,
    worktree: &Worktree,
    tools: &T,
) -> Result<ActionResult, DispatchError> {
    debug!(action_kind = action.kind(), "dispatch: action begin");
    match action {
        AgentAction::RunTool { tool, input } => {
            debug!(tool = %tool, "dispatch: run_tool");
            match tools.execute(&tool, &input, worktree.path()).await {
                Ok(output) => Ok(ActionResult::ToolOutput(output)),
                Err(e) => Ok(ActionResult::Error(format!("tool {tool} failed: {e}"))),
            }
        }
        AgentAction::CommitChanges { message } => commit_changes(worktree.path(), &work.files, &message).await,
        AgentAction::ProposeBundle { claims } => propose_bundle(worktree, &work.files, claims).await,
        AgentAction::Done { message } => {
            let mut bundle = Bundle::new(worktree.work_id().clone(), worktree.branch().to_string(), vec![]);
            bundle.noop_reason = Some(message);
            Ok(ActionResult::Done(bundle))
        }
        AgentAction::NeedHelp { reason } => {
            if let Err(e) = commit_partial_for_inspection(worktree.path()).await {
                warn!(error = %e, "partial commit for need_help failed");
            }
            Ok(ActionResult::NeedHelp(reason))
        }
    }
}

/// Stage and commit only the dirty paths matching the Work's scope.
///
/// Pipeline: `git status --porcelain --untracked-files=all` -> partition
/// against `scope_files` (with `.loopr/` always filtered to out-of-scope) ->
/// `git commit --only -- <in_scope...>`. `--only` snapshots the working-tree
/// contents of the listed paths into the commit, ignoring any other staged
/// entries. This eliminates the index-leak class of bugs where prior
/// `git add` invocations from `bash` actions would otherwise be folded in.
///
/// `scope_files` empty falls back to artifact-only filtering: every
/// non-`.loopr/` dirty path is in-scope.
#[instrument(
    level = "debug",
    skip_all,
    fields(
        path = %path.display(),
        message_chars = message.len(),
        scope_count = scope_files.len(),
        in_scope_count = tracing::field::Empty,
        out_of_scope_count = tracing::field::Empty,
    ),
    err,
)]
async fn commit_changes(path: &Path, scope_files: &[String], message: &str) -> Result<ActionResult, DispatchError> {
    let dirty = git_status_porcelain(path).await?;
    if dirty.is_empty() {
        Span::current().record("in_scope_count", 0u64);
        Span::current().record("out_of_scope_count", 0u64);
        return Ok(ActionResult::NothingToCommit { dropped: vec![] });
    }
    let (in_scope, out_of_scope) = scope::partition_by_scope(&dirty, scope_files);
    Span::current().record("in_scope_count", in_scope.len() as u64);
    Span::current().record("out_of_scope_count", out_of_scope.len() as u64);
    if !out_of_scope.is_empty() {
        warn!(
            out_of_scope = ?out_of_scope,
            "commit_changes: dropping out-of-scope dirty paths"
        );
    }
    if in_scope.is_empty() {
        return Ok(ActionResult::NothingToCommit { dropped: out_of_scope });
    }
    // Stage only the in-scope paths so untracked new files become known
    // to git before the `--only` commit. `--only` then snapshots exactly
    // those paths into the commit, ignoring any other stale index
    // entries (the index-leak fix).
    let mut add_args: Vec<&str> = vec!["add", "--"];
    add_args.extend(in_scope.iter().map(String::as_str));
    run_git(path, &add_args).await?;
    let mut commit_args: Vec<&str> = vec!["commit", "--only", "--message", message, "--no-gpg-sign", "--"];
    commit_args.extend(in_scope.iter().map(String::as_str));
    run_git(path, &commit_args).await?;
    let sha = rev_parse_head(path).await?;
    debug!(
        sha = %sha,
        in_scope = in_scope.len(),
        dropped = out_of_scope.len(),
        "dispatch: commit_changes ok"
    );
    Ok(ActionResult::Committed {
        sha,
        dropped: out_of_scope,
    })
}

/// Commit any in-progress work for human inspection and keep going.
/// `git add -u` is intentional: only tracked modifications, so a
/// runaway agent dropping garbage files doesn't pollute the branch.
#[instrument(level = "debug", skip_all, fields(path = %path.display()), err)]
async fn commit_partial_for_inspection(path: &Path) -> Result<(), DispatchError> {
    run_git(path, &["add", "-u"]).await?;
    if is_staging_empty(path).await? {
        debug!("dispatch: commit_partial_for_inspection nothing-staged");
        return Ok(());
    }
    run_git(
        path,
        &["commit", "--message", "partial: agent needed help", "--no-gpg-sign"],
    )
    .await?;
    debug!("dispatch: commit_partial_for_inspection ok");
    Ok(())
}

/// Construct the Bundle, computing `loc_changed` from base SHA and
/// capturing HEAD SHA as `head_commit`. If the worktree is dirty at
/// propose time, stage and commit any in-scope paths via `git commit
/// --only`. `bundle.paths` is populated from the branch-vs-base diff
/// so the reviewer's `git_show` filter and the integrator's collision
/// detector see every path the bundle touches (including those landed
/// by earlier `commit_changes` actions on the same iteration), not
/// just the last staging step.
#[instrument(
    level = "debug",
    skip_all,
    fields(
        work_id = %worktree.work_id(),
        branch = worktree.branch(),
        worktree_path = %worktree.path().display(),
        claim_count = claims.len(),
        scope_count = scope_files.len(),
        in_scope_count = tracing::field::Empty,
        out_of_scope_count = tracing::field::Empty,
    ),
    err,
)]
async fn propose_bundle(
    worktree: &Worktree,
    scope_files: &[String],
    claims: Vec<String>,
) -> Result<ActionResult, DispatchError> {
    let mut total_dropped: Vec<String> = vec![];
    let dirty = git_status_porcelain(worktree.path()).await?;
    if !dirty.is_empty() {
        let (in_scope, out_of_scope) = scope::partition_by_scope(&dirty, scope_files);
        Span::current().record("in_scope_count", in_scope.len() as u64);
        Span::current().record("out_of_scope_count", out_of_scope.len() as u64);
        if !out_of_scope.is_empty() {
            warn!(
                out_of_scope = ?out_of_scope,
                "propose_bundle: dropping out-of-scope dirty paths"
            );
            total_dropped = out_of_scope;
        }
        if !in_scope.is_empty() {
            // Stage only the in-scope paths so untracked new files become
            // known to git before the `--only` commit. See `commit_changes`
            // for the index-leak rationale.
            let mut add_args: Vec<&str> = vec!["add", "--"];
            add_args.extend(in_scope.iter().map(String::as_str));
            run_git(worktree.path(), &add_args).await?;
            let mut commit_args: Vec<&str> = vec![
                "commit",
                "--only",
                "--message",
                "propose_bundle: stage remaining changes",
                "--no-gpg-sign",
                "--",
            ];
            commit_args.extend(in_scope.iter().map(String::as_str));
            run_git(worktree.path(), &commit_args).await?;
        }
    } else {
        Span::current().record("in_scope_count", 0u64);
        Span::current().record("out_of_scope_count", 0u64);
    }

    let head_commit = rev_parse_head(worktree.path()).await.ok();
    let loc_changed = compute_loc_changed(worktree.path(), worktree.sha()).await.ok();
    // Branch-vs-base diff: every path the bundle touches across all the
    // implementer's commits, not just the last staging step. With an
    // empty worktree base sha (test-only construction), skip the diff.
    let branch_paths = if worktree.sha().is_empty() {
        Vec::new()
    } else {
        git_diff_name_only(worktree.path(), worktree.sha())
            .await
            .unwrap_or_default()
    };

    // Phase 6 manifest: classify the branch diff into added / modified
    // / deleted, plus a stable patch_id capped at PATCH_ID_OVERSIZE_CAP.
    let manifest = if worktree.sha().is_empty() {
        NameStatusManifest {
            added: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        }
    } else {
        git_diff_name_status(worktree.path(), worktree.sha())
            .await
            .unwrap_or(NameStatusManifest {
                added: Vec::new(),
                modified: Vec::new(),
                deleted: Vec::new(),
            })
    };
    let paths_added = &manifest.added;
    let paths_modified = &manifest.modified;
    let paths_deleted = &manifest.deleted;
    let (patch_id, diff_bytes) = match (head_commit.as_deref(), worktree.sha().is_empty()) {
        (Some(commit), false) => git_patch_id(worktree.path(), commit, PATCH_ID_OVERSIZE_CAP)
            .await
            .unwrap_or((None, 0)),
        _ => (None, 0),
    };
    let patch_id_str = match patch_id {
        Some(id) => id,
        None if diff_bytes > PATCH_ID_OVERSIZE_CAP => "oversize".to_string(),
        None => String::new(),
    };

    let mut bundle = Bundle::new(worktree.work_id().clone(), worktree.branch().to_string(), claims);
    bundle.head_commit = head_commit.clone();
    bundle.loc_changed = loc_changed;
    bundle.paths = branch_paths.clone();

    // Phase 6 canonical "implementer produced bundle" event. The
    // earlier daemon-context site is now silent; this is the
    // one-and-only emission. Span ancestry is `propose_bundle` so
    // the agents.dispatch target shows up on the line.
    info!(
        bundle_id = %bundle.id,
        work_id = %worktree.work_id(),
        head_commit = ?head_commit,
        loc_changed = ?loc_changed,
        paths_added = ?paths_added,
        paths_modified = ?paths_modified,
        paths_deleted = ?paths_deleted,
        path_count = branch_paths.len(),
        patch_id = %patch_id_str,
        diff_bytes,
        dropped_count = total_dropped.len(),
        "implementer produced bundle"
    );
    Ok(ActionResult::BundleCreated {
        bundle,
        dropped: total_dropped,
    })
}

/// Maximum bytes of `git show <commit>` output for which we compute a
/// stable `patch_id`. Beyond this, `propose_bundle` emits the literal
/// `"oversize"` and only `diff_bytes` so a runaway implementer is
/// visible without paying the patch-id compute cost.
const PATCH_ID_OVERSIZE_CAP: usize = 1024 * 1024;

/// Run `git status --porcelain --untracked-files=all` and parse the
/// result via `scope::parse_porcelain_status`. The `-uall` flag is
/// load-bearing: without it, untracked directories collapse into a
/// single `?? new_dir/` entry that exact-path scope matching cannot
/// resolve to any file under it.
#[instrument(level = "trace", skip_all, fields(path = %path.display()), err)]
async fn git_status_porcelain(path: &Path) -> Result<Vec<String>, DispatchError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git status --porcelain failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(scope::parse_porcelain_status(&text))
}

/// Run `git diff --name-only <base_sha>..HEAD`. Used by `propose_bundle`
/// to populate `bundle.paths` with the canonical set of paths the
/// branch touched across all of the implementer's commits, not just
/// the final staging step.
#[instrument(level = "trace", skip_all, fields(path = %path.display(), base_sha = base_sha), err)]
async fn git_diff_name_only(path: &Path, base_sha: &str) -> Result<Vec<String>, DispatchError> {
    let spec = format!("{base_sha}..HEAD");
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--name-only", &spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git diff --name-only failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// (added, modified, deleted) lists from `git diff --name-status`.
/// Folded into a typed return so clippy's `complex_type` lint doesn't
/// fire on the tuple form.
struct NameStatusManifest {
    added: Vec<String>,
    modified: Vec<String>,
    deleted: Vec<String>,
}

/// Phase 6: classify the branch's changed paths into added /
/// modified / deleted via `git diff --name-status <base>..HEAD`. Used
/// by `propose_bundle`'s manifest event so a reader can grep
/// `paths_added` directly without re-running git. Renames (`R<score>`)
/// are folded into `paths_modified`; copies (`C<score>`) into
/// `paths_added`. Unknown statuses (`U`, `T`) fall through to
/// `paths_modified` rather than being dropped.
#[instrument(level = "trace", skip_all, fields(path = %path.display(), base_sha = base_sha), err)]
async fn git_diff_name_status(path: &Path, base_sha: &str) -> Result<NameStatusManifest, DispatchError> {
    let spec = format!("{base_sha}..HEAD");
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--name-status", &spec])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git diff --name-status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let mut manifest = NameStatusManifest {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.is_empty() {
            continue;
        }
        let (status, rest) = line.split_at(1);
        let trimmed = rest.trim_start();
        // Renames carry two paths: `R<score>\told\tnew`. The destination
        // is the relevant one for an "after" view of the branch; take
        // the last tab-separated field.
        let path_str = trimmed.split('\t').next_back().unwrap_or(trimmed).to_string();
        match status {
            "A" | "C" => manifest.added.push(path_str),
            "D" => manifest.deleted.push(path_str),
            _ => manifest.modified.push(path_str),
        }
    }
    Ok(manifest)
}

/// Phase 6: compute a stable patch id via `git show <commit> | git
/// patch-id --stable`. Whitespace- and context-line-stable across
/// `diff.algorithm` and `diff.context` settings, unlike a raw
/// `sha256(unified diff)`. Returns the first whitespace-delimited
/// token from `git patch-id`'s output (`<patch-id> <commit>`).
///
/// `oversize_cap_bytes` short-circuits to `Ok((None, byte_count))`
/// when `git show` produces more bytes than the cap; the caller emits
/// `patch_id = "oversize"` plus `diff_bytes` so a runaway implementer
/// is visible without paying the patch-id compute cost.
#[instrument(level = "trace", skip_all, fields(path = %path.display(), commit = commit), err)]
async fn git_patch_id(
    path: &Path,
    commit: &str,
    oversize_cap_bytes: usize,
) -> Result<(Option<String>, usize), DispatchError> {
    let show = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["show", commit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !show.status.success() {
        return Err(DispatchError::Git(format!(
            "git show {commit} failed: {}",
            String::from_utf8_lossy(&show.stderr)
        )));
    }
    let diff_bytes = show.stdout.len();
    if diff_bytes > oversize_cap_bytes {
        return Ok((None, diff_bytes));
    }

    let mut patch_id_proc = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["patch-id", "--stable"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = patch_id_proc.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(&show.stdout).await?;
        stdin.shutdown().await?;
    }
    let patch_out = patch_id_proc.wait_with_output().await?;
    if !patch_out.status.success() {
        return Err(DispatchError::Git(format!(
            "git patch-id failed: {}",
            String::from_utf8_lossy(&patch_out.stderr)
        )));
    }
    let patch_id = String::from_utf8_lossy(&patch_out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string);
    Ok((patch_id, diff_bytes))
}

#[instrument(level = "trace", skip_all, fields(path = %path.display()), err)]
async fn is_staging_empty(path: &Path) -> Result<bool, DispatchError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["diff", "--cached", "--name-only"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        return Err(DispatchError::Git(format!(
            "git diff --cached failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout.is_empty())
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

/// Compute lines changed between `sha` and `HEAD` via
/// `git diff --numstat`. Binary files show `-\t-\t<file>` in numstat
/// output and contribute 0 to the total. If `sha` is empty
/// (test-only constructor), returns 0.
#[instrument(level = "trace", skip_all, fields(path = %path.display(), base_sha = sha), err)]
async fn compute_loc_changed(path: &Path, sha: &str) -> Result<u32, DispatchError> {
    if sha.is_empty() {
        return Ok(0);
    }
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

/// Sum insertions + deletions across all text files, skipping binary
/// rows (which show `-` in the first two columns).
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

/// Production implementation of `ToolExecutor`. Thin adapter: builds a
/// `tools::ToolContext` per invocation and forwards to `tools::dispatch`.
/// All shared state (router, sandbox posture, denylist) is Arc'd in from
/// the owning `DaemonContext`.
///
/// Per-Work instance: each Work's implementer gets its own `RealTools`
/// with that Work's `persist_base` so overflow-output files land under
/// `.loopr/runs/<session-id>/work/<work-id>/`.
pub struct RealTools {
    router: Arc<LaneRouter>,
    sandbox: SandboxMode,
    bash_denylist: Arc<BashDenylist>,
    path_deny_patterns: Vec<String>,
    persist_base: Option<PathBuf>,
}

impl RealTools {
    pub fn new(
        router: Arc<LaneRouter>,
        sandbox: SandboxMode,
        bash_denylist: Arc<BashDenylist>,
        path_deny_patterns: Vec<String>,
        persist_base: Option<PathBuf>,
    ) -> Self {
        Self {
            router,
            sandbox,
            bash_denylist,
            path_deny_patterns,
            persist_base,
        }
    }
}

impl ToolExecutor for RealTools {
    async fn execute(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        working_dir: &Path,
    ) -> Result<String, DispatchError> {
        let span = tracing::debug_span!(
            "real_tools.execute",
            tool_name = tool_name,
            working_dir = %working_dir.display(),
        );
        let _enter = span.enter();
        let ctx = ToolContext {
            working_dir: working_dir.to_path_buf(),
            router: self.router.clone(),
            sandbox: self.sandbox,
            path_deny_patterns: self.path_deny_patterns.clone(),
            bash_denylist: self.bash_denylist.clone(),
            persist_base: self.persist_base.clone(),
            invocation_id: Some(uuid::Uuid::now_v7()),
        };
        let value = tools::dispatch(tool_name, input.clone(), &ctx)
            .await
            .map_err(|e| DispatchError::Tool(e.to_string()))?;
        Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
    }
}

#[cfg(test)]
mod tests;
