use std::path::Path;
use std::time::SystemTime;

use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;
use crate::agents::executor::util::auto_acquire_write_lock;

/// Handle WriteFile action.
pub(super) async fn handle_write_file(
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
    path: &str,
    content: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    // Validate path stays within worktree (sandbox)
    let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

    // Auto-acquire advisory lock for write operations
    if let Some(wi_id) = work_id {
        auto_acquire_write_lock(bridge, path, wi_id);
    }

    // Advisory lock check: under LockStrict, reject writes if another agent holds the lock
    if bridge.config().strategy.conflict_policy == crate::config::ConflictPolicy::LockStrict {
        let lock_resp = bridge.request(
            "lock.list",
            serde_json::json!({ "resource": path, "active_only": true }),
        );
        if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array()) {
            let held_by_other = locks
                .iter()
                .any(|l| l.get("holder_id").and_then(|v| v.as_str()) != work_id);
            if held_by_other {
                let holder = locks
                    .iter()
                    .find(|l| l.get("holder_id").and_then(|v| v.as_str()) != work_id)
                    .and_then(|l| l.get("holder_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Ok(ActionResult::ActionError(format!(
                    "file '{}' locked by {} (policy: LockStrict)",
                    path, holder
                )));
            }
        }
    }

    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| eyre!("write_file mkdir '{}': {}", path, e))?;
    }
    tokio::fs::write(&full_path, content)
        .await
        .map_err(|e| eyre!("write_file '{}': {}", path, e))?;
    ctx.cache().invalidate(&full_path);
    Ok(ActionResult::FileWritten(path.to_string()))
}

/// Handle EditFile action.
pub(super) async fn handle_edit_file(
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

    // Auto-acquire advisory lock for edit operations
    if let Some(wi_id) = work_id {
        auto_acquire_write_lock(bridge, path, wi_id);
    }

    // Advisory lock check: under LockStrict, reject edits if another agent holds the lock
    if bridge.config().strategy.conflict_policy == crate::config::ConflictPolicy::LockStrict {
        let lock_resp = bridge.request(
            "lock.list",
            serde_json::json!({ "resource": path, "active_only": true }),
        );
        if let Some(locks) = lock_resp.result.as_ref().and_then(|v| v.as_array()) {
            let held_by_other = locks
                .iter()
                .any(|l| l.get("holder_id").and_then(|v| v.as_str()) != work_id);
            if held_by_other {
                let holder = locks
                    .iter()
                    .find(|l| l.get("holder_id").and_then(|v| v.as_str()) != work_id)
                    .and_then(|l| l.get("holder_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                return Ok(ActionResult::ActionError(format!(
                    "file '{}' locked by {} (policy: LockStrict)",
                    path, holder
                )));
            }
        }
    }

    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| eyre!("edit_file read '{}': {}", path, e))?;

    let count = content.matches(old_string).count();
    if count == 0 {
        return Ok(ActionResult::ActionError(format!(
            "edit_file '{}': old_string not found in file",
            path
        )));
    }
    if count > 1 {
        return Ok(ActionResult::ActionError(format!(
            "edit_file '{}': old_string found {} times (must be unique - provide more context)",
            path, count
        )));
    }

    let updated = content.replacen(old_string, new_string, 1);
    tokio::fs::write(&full_path, &updated)
        .await
        .map_err(|e| eyre!("edit_file write '{}': {}", path, e))?;
    ctx.cache().invalidate(&full_path);
    Ok(ActionResult::FileEdited(path.to_string()))
}

/// Handle ReadFile action.
pub(super) async fn handle_read_file(
    ctx: &AgentContext,
    worktree_path: &Path,
    path: &str,
    offset: &Option<u64>,
    limit: &Option<u64>,
) -> Result<ActionResult> {
    let full_path = crate::agents::sandbox::validate_sandboxed_path(worktree_path, path, false)?;

    // Stat for mtime (single syscall, kernel-cached)
    let mtime = tokio::fs::metadata(&full_path)
        .await
        .map_err(|e| eyre!("read_file '{}': {}", path, e))?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);

    // Dedup check BEFORE reading the file
    if let Some(cached_lines) = ctx.cache().check_hit(&full_path, *offset, *limit, mtime) {
        let start = offset.unwrap_or(1).max(1);
        let effective_limit = limit.unwrap_or(500);
        let end = (start + effective_limit - 1).min(cached_lines as u64);
        return Ok(ActionResult::FileRead(format!(
            "File unchanged since last read \
             (lines {}-{} of {}, use offset/limit for other sections, \
             or proceed with editing).",
            start, end, cached_lines
        )));
    }

    // Cache miss - read the file
    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| eyre!("read_file '{}': {}", path, e))?;
    let lines: Vec<&str> = content.lines().collect();

    // Record in cache for future dedup
    ctx.cache().record(&full_path, *offset, *limit, mtime, lines.len());

    let start = offset.unwrap_or(1).max(1) as usize - 1;
    let effective_limit = limit.unwrap_or(500) as usize;
    let end = (start + effective_limit).min(lines.len());
    let mut numbered: Vec<String> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start + i + 1, line))
        .collect();
    if end < lines.len() && limit.is_none() {
        numbered.push(format!(
            "\n... [{} more lines, use offset/limit to read specific sections]",
            lines.len() - end
        ));
    }
    Ok(ActionResult::FileRead(numbered.join("\n")))
}

/// Handle Commit action.
pub(super) async fn handle_commit(worktree_path: &Path, message: &str, paths: &[String]) -> Result<ActionResult> {
    // Stage specified paths (or all changes if empty)
    let add_args = if paths.is_empty() { vec!["-A".to_string()] } else { paths.to_vec() };
    let mut add_cmd = tokio::process::Command::new("git");
    add_cmd.arg("add").args(&add_args).current_dir(worktree_path);
    let add_out = add_cmd.output().await?;
    if !add_out.status.success() {
        let stderr = String::from_utf8_lossy(&add_out.stderr);
        return Err(eyre!("git add failed: {}", stderr));
    }

    // Commit
    let mut commit_cmd = tokio::process::Command::new("git");
    commit_cmd.args(["commit", "-m", message]).current_dir(worktree_path);
    let commit_out = commit_cmd.output().await?;
    if !commit_out.status.success() {
        let stderr = String::from_utf8_lossy(&commit_out.stderr);
        let stdout = String::from_utf8_lossy(&commit_out.stdout);
        let detail = if stderr.trim().is_empty() { &stdout } else { &stderr };
        return Err(eyre!("git commit failed: {}", detail.trim()));
    }
    Ok(ActionResult::Committed(message.to_string()))
}

/// Handle SearchCode action.
pub(super) async fn handle_search_code(
    worktree_path: &Path,
    agent_log: &crate::agents::agent_logger::AgentLogger,
    pattern: &str,
    glob: Option<&str>,
    path: Option<&str>,
) -> Result<ActionResult> {
    let repo_root = worktree_path; // For Researcher, worktree_path is the repo root
    match crate::agents::researcher::execute_search_code(repo_root, pattern, glob, path, agent_log).await {
        Ok(output) => Ok(ActionResult::FileRead(output)),
        Err(e) => Ok(ActionResult::ActionError(e.to_string())),
    }
}

/// Handle SearchFiles action.
pub(super) async fn handle_search_files(
    worktree_path: &Path,
    agent_log: &crate::agents::agent_logger::AgentLogger,
    pattern: &str,
    path: Option<&str>,
) -> Result<ActionResult> {
    let repo_root = worktree_path;
    match crate::agents::researcher::execute_search_files(repo_root, pattern, path, agent_log).await {
        Ok(output) => Ok(ActionResult::FileRead(output)),
        Err(e) => Ok(ActionResult::ActionError(e.to_string())),
    }
}

/// Handle ListDirectory action.
pub(super) async fn handle_list_directory(
    worktree_path: &Path,
    agent_log: &crate::agents::agent_logger::AgentLogger,
    path: &str,
) -> Result<ActionResult> {
    let repo_root = worktree_path;
    match crate::agents::researcher::execute_list_directory(repo_root, path, agent_log).await {
        Ok(output) => Ok(ActionResult::FileRead(output)),
        Err(e) => Ok(ActionResult::ActionError(e.to_string())),
    }
}
