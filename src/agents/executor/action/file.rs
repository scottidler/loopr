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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{test_agent_context, test_agent_context_with_config, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};
    use crate::config::Config;

    use crate::test_util::TestDir;

    #[tokio::test]
    async fn test_execute_action_write_file() {
        let dir = TestDir::new("loopr-exec-write");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "hello world".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));

        let content = std::fs::read_to_string(dir.join("test.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_write_file_lock_strict_blocks_other_agent() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockstrict");

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentKind::Implementer, config);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "should be blocked".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("agent-2")).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_lock_strict_allows_holder_rewrite() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-lockholderrewrite");

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentKind::Implementer, config);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "wi-abc", "granted_by": "wi-abc" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "holder can rewrite".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("wi-abc")).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileWritten(_)),
            "expected FileWritten (holder should not self-block), got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_write_file_lock_advisory_allows() {
        let dir = TestDir::new("loopr-exec-lockadvisory");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let lock_resp = ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );
        assert!(!lock_resp.is_error());

        let action = AgentAction::WriteFile {
            path: "test.txt".to_string(),
            content: "should work".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
    }

    #[tokio::test]
    async fn test_execute_action_read_file() {
        let dir = TestDir::new("loopr-exec-read");
        std::fs::write(dir.join("read-me.txt"), "file content").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::ReadFile {
            path: "read-me.txt".to_string(),
            offset: None,
            limit: None,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = result {
            assert!(content.contains("file content"));
        } else {
            panic!("expected FileRead result");
        }
    }

    #[tokio::test]
    async fn test_execute_commit_success() {
        let dir = TestDir::new("loopr-exec-commit");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join("test.txt"), "hello").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "test commit".to_string(),
            paths: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Committed(ref msg) if msg == "test commit"),
            "expected Committed, got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_execute_commit_specific_paths() {
        let dir = TestDir::new("loopr-exec-commitpaths");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(dir.join("b.txt"), "bbb").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::Commit {
            message: "add a.txt only".to_string(),
            paths: vec!["a.txt".to_string()],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::Committed(_)));
    }

    #[tokio::test]
    async fn test_write_file_path_escape() {
        let dir = TestDir::new("loopr-exec-escape");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "../../../etc/passwd".to_string(),
            content: "pwned".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[tokio::test]
    async fn test_read_file_not_found() {
        let dir = TestDir::new("loopr-exec-readnf");
        let stores = test_stores(&dir);

        let action = AgentAction::ReadFile {
            path: "nonexistent.txt".to_string(),
            offset: None,
            limit: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_file_creates_parent_dirs() {
        let dir = TestDir::new("loopr-exec-writedirs");
        let stores = test_stores(&dir);

        let action = AgentAction::WriteFile {
            path: "deep/nested/dir/file.txt".to_string(),
            content: "nested content".to_string(),
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(matches!(result, ActionResult::FileWritten(_)));
        let content = std::fs::read_to_string(dir.join("deep/nested/dir/file.txt")).unwrap();
        assert_eq!(content, "nested content");
    }

    #[tokio::test]
    async fn test_search_code_action() {
        let dir = TestDir::new("loopr-exec-searchcode");
        std::fs::write(dir.join("example.rs"), "fn main() { println!(\"hello\"); }").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::SearchCode {
            pattern: "fn main".to_string(),
            glob: None,
            path: None,
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("fn main"))
                || matches!(result, ActionResult::ActionError(_)),
            "got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_list_directory_action() {
        let dir = TestDir::new("loopr-exec-listdir");
        std::fs::write(dir.join("file1.txt"), "a").unwrap();
        std::fs::write(dir.join("file2.txt"), "b").unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ListDirectory { path: ".".to_string() };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileRead(ref content) if content.contains("file1.txt")),
            "got: {:?}",
            result
        );
    }

    // --- Task #6: Additional coverage tests ---

    #[tokio::test]
    async fn test_read_file_dedup_returns_unchanged_on_second_read() {
        let dir = TestDir::new("loopr-exec-dedup");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        let r1 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r1 {
            assert!(content.contains("line1"), "first read should return content");
        } else {
            panic!("expected FileRead, got: {:?}", r1);
        }

        let r2 = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                content.contains("File unchanged since last read"),
                "second read should return dedup message, got: {}",
                content
            );
            assert!(content.contains("3"), "should mention total lines");
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_write() {
        let dir = TestDir::new("loopr-exec-dedup-write");
        std::fs::write(dir.join("target.rs"), "original\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        let write_action = AgentAction::WriteFile {
            path: "target.rs".to_string(),
            content: "updated\n".to_string(),
        };
        execute_action(&write_action, &ctx, &dir, None).await.unwrap();

        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("updated"),
                "read after write should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after write");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_invalidated_by_edit() {
        let dir = TestDir::new("loopr-exec-dedup-edit");
        std::fs::write(dir.join("target.rs"), "hello world\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let read_action = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };

        execute_action(&read_action, &ctx, &dir, None).await.unwrap();

        let edit_action = AgentAction::EditFile {
            path: "target.rs".to_string(),
            old_string: "hello".to_string(),
            new_string: "goodbye".to_string(),
        };
        execute_action(&edit_action, &ctx, &dir, None).await.unwrap();

        let r3 = execute_action(&read_action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r3 {
            assert!(
                content.contains("goodbye"),
                "read after edit should return fresh content, got: {}",
                content
            );
            assert!(!content.contains("File unchanged"), "should not be dedup after edit");
        } else {
            panic!("expected FileRead, got: {:?}", r3);
        }
    }

    #[tokio::test]
    async fn test_read_file_dedup_different_offset_no_dedup() {
        let dir = TestDir::new("loopr-exec-dedup-offset");
        std::fs::write(dir.join("target.rs"), "line1\nline2\nline3\n").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action1 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: None,
            limit: None,
        };
        let action2 = AgentAction::ReadFile {
            path: "target.rs".to_string(),
            offset: Some(1),
            limit: None,
        };

        execute_action(&action1, &ctx, &dir, None).await.unwrap();

        let r2 = execute_action(&action2, &ctx, &dir, None).await.unwrap();
        if let ActionResult::FileRead(content) = &r2 {
            assert!(
                !content.contains("File unchanged"),
                "different offset should not dedup, got: {}",
                content
            );
            assert!(content.contains("line1"));
        } else {
            panic!("expected FileRead, got: {:?}", r2);
        }
    }

    // --- Phase 1: Auto-Lock tests ---

    #[tokio::test]
    async fn test_write_file_auto_acquires_lock() {
        let dir = TestDir::new("loopr-exec-autolock-write");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "hello".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-100")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-100", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 auto-acquired lock, got {}", locks.len());
        assert_eq!(locks[0]["holder_id"].as_str().unwrap(), "wi-100");
    }

    #[tokio::test]
    async fn test_edit_file_auto_acquires_lock() {
        let dir = TestDir::new("loopr-exec-autolock-edit");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "old content").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::EditFile {
            path: "src/lib.rs".to_string(),
            old_string: "old content".to_string(),
            new_string: "new content".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-200")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-200", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 auto-acquired lock");
    }

    #[tokio::test]
    async fn test_write_file_reuses_existing_lock() {
        let dir = TestDir::new("loopr-exec-autolock-reuse");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "first".to_string(),
        };
        execute_action(&action, &ctx, &dir, Some("wi-300")).await.unwrap();
        let action2 = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "second".to_string(),
        };
        execute_action(&action2, &ctx, &dir, Some("wi-300")).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "holder_id": "wi-300", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert_eq!(locks.len(), 1, "expected 1 lock (reused), got {}", locks.len());
    }

    #[tokio::test]
    async fn test_no_auto_lock_without_work_id() {
        let dir = TestDir::new("loopr-exec-autolock-none");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let action = AgentAction::WriteFile {
            path: "src/lib.rs".to_string(),
            content: "hello".to_string(),
        };
        execute_action(&action, &ctx, &dir, None).await.unwrap();

        let lock_resp = ctx.bridge.request(
            "lock.list",
            serde_json::json!({ "resource": "src/lib.rs", "active_only": true }),
        );
        let locks = lock_resp.result.as_ref().unwrap().as_array().unwrap();
        assert!(locks.is_empty(), "expected no locks when work_id is None");
    }

    #[tokio::test]
    async fn test_edit_file_lock_strict_allows_holder() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-editlock-holder");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "original").unwrap();

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentKind::Implementer, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "wi-edit", "granted_by": "wi-edit" }),
        );

        let action = AgentAction::EditFile {
            path: "src/main.rs".to_string(),
            old_string: "original".to_string(),
            new_string: "modified".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("wi-edit")).await.unwrap();
        assert!(
            matches!(result, ActionResult::FileEdited(_)),
            "expected FileEdited (holder should not self-block on edit), got: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn test_edit_file_lock_strict_blocks_other_agent() {
        use crate::config::{ConflictPolicy, StrategyConfig};

        let dir = TestDir::new("loopr-exec-editlock-other");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "original").unwrap();

        let stores = test_stores(&dir);
        let config = Config {
            strategy: StrategyConfig {
                conflict_policy: ConflictPolicy::LockStrict,
                ..StrategyConfig::default()
            },
            ..Config::default()
        };
        let ctx = test_agent_context_with_config(&dir, &stores, AgentKind::Implementer, config);

        ctx.bridge.request(
            "lock.create",
            serde_json::json!({ "resource": "src/main.rs", "holder_id": "agent-1", "granted_by": "agent-1" }),
        );

        let action = AgentAction::EditFile {
            path: "src/main.rs".to_string(),
            old_string: "original".to_string(),
            new_string: "modified".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, Some("agent-2")).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("locked")),
            "expected ActionError for locked file, got: {:?}",
            result
        );
    }
}
