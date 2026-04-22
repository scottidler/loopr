#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use serde_json::json;
use tempfile::TempDir;

use domain::WorkId;
use worktree::Worktree;

use crate::action::AgentAction;
use crate::dispatch::{ActionResult, DispatchError, ToolExecutor, dispatch_action};

/// In-memory tool executor fake. Records each call and returns a
/// canned string. Never touches real subprocesses.
struct FakeTools {
    response: String,
}

impl ToolExecutor for FakeTools {
    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        tool_name: &'a str,
        _input: &'a serde_json::Value,
        _working_dir: &'a Path,
    ) -> impl std::future::Future<Output = Result<String, DispatchError>> + Send + 'a {
        let response = self.response.clone();
        async move {
            if tool_name == "fail" {
                Err(DispatchError::Tool("synthetic tool failure".into()))
            } else {
                Ok(response)
            }
        }
    }
}

/// Init a git repo in a fresh TempDir with one initial commit.
/// Returns (TempDir, repo_path, initial_sha, worktree_path).
/// The "worktree" here is just the same repo — we use a fabricated
/// `Worktree` handle via `from_parts` pointing at the repo. For
/// dispatch purposes the distinction does not matter; git ops run
/// against the path.
fn init_repo() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    run(&path, &["init", "-q", "-b", "main"]);
    run(&path, &["config", "user.email", "test@example.com"]);
    run(&path, &["config", "user.name", "Test"]);
    run(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    run(&path, &["add", "-A"]);
    run(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);

    let sha = run_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn run(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn fake_worktree(path: &Path, base_sha: String) -> Worktree {
    // We can't use from_parts directly (it's pub(crate) in worktree).
    // Instead: the dispatcher only reads path/work_id/branch/base_sha.
    // We fake a worktree via a shell-level Worktree construction is not
    // exposed; the dispatcher has what it needs through a minimal shim.
    // Actually the handle's fields are private; the only way to get one
    // is Worktree::create (real git-worktree invocation) or from_parts
    // (test-only). Since we're integration-testing dispatch against a
    // real git repo but not a real worktree, we build a fake handle
    // manually via a helper constructor exposed later.
    //
    // Simpler: add a public test shim in this test module that calls
    // worktree::Worktree::create against the repo itself. Easiest
    // correct path: spin up a real worktree.
    let worktree_root = path.parent().unwrap().join("wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    Worktree::create(path, &worktree_root, WorkId::new(), &base_sha).unwrap()
}

#[tokio::test]
async fn run_tool_happy_path_returns_tool_output() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: "hi from fake".into(),
    };
    let action = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({"command": "echo hi"}),
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::ToolOutput(s) => assert_eq!(s, "hi from fake"),
        other => panic!("expected ToolOutput, got {other:?}"),
    }
}

#[tokio::test]
async fn run_tool_correctable_error_returns_error_variant() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: "unused".into(),
    };
    let action = AgentAction::RunTool {
        tool: "fail".into(),
        input: json!({}),
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::Error(msg) => assert!(msg.contains("synthetic tool failure"), "got: {msg}"),
        other => panic!("expected Error variant, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_changes_on_clean_tree_returns_nothing_to_commit() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::CommitChanges { message: "noop".into() };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    assert!(
        matches!(result, ActionResult::NothingToCommit),
        "expected NothingToCommit, got {result:?}"
    );
}

#[tokio::test]
async fn commit_changes_stages_new_file_via_add_minus_a_flag() {
    // Architect-R1 decision: CommitChanges uses `git add -A` so new
    // files the agent created are staged. Without that, writing a
    // new file and calling CommitChanges would fail with "nothing
    // added to commit".
    let (_dir, _path, base) = init_repo();
    let wt = fake_worktree(&_path, base);
    let wt_path = wt.path();
    std::fs::write(wt_path.join("new.txt"), "hello\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::CommitChanges {
        message: "add new.txt".into(),
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::Committed(sha) => {
            assert_eq!(sha.len(), 40, "expected full SHA, got: {sha}");
            // Verify the commit message and file are in the commit.
            let msg = run_capture(wt_path, &["log", "-1", "--format=%s"]);
            assert_eq!(msg, "add new.txt");
            let tree = run_capture(wt_path, &["ls-tree", "--name-only", "HEAD"]);
            assert!(tree.contains("new.txt"), "new.txt must be in commit tree: {tree}");
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_captures_head_and_loc_changed() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base.clone());
    let wt_path = wt.path();
    // Make 3 lines of change in a tracked file.
    std::fs::write(wt_path.join("README.md"), "a\nb\nc\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::ProposeBundle {
        claims: vec!["changed readme".into()],
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::BundleCreated(bundle) => {
            assert!(bundle.head_commit.is_some(), "head_commit must be captured");
            assert_eq!(bundle.head_commit.as_ref().unwrap().len(), 40, "full SHA expected");
            assert_eq!(bundle.claims, vec!["changed readme".to_string()]);
            // Base had README.md = "initial\n" (1 line). New content is 3 lines.
            // Diff: -1 / +3 = 4 lines changed.
            assert_eq!(bundle.loc_changed, Some(4), "loc_changed must reflect diff vs base_sha");
            assert!(!bundle.force_proposed, "force_proposed false on normal propose");
        }
        other => panic!("expected BundleCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_with_no_changes_still_captures_head() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::ProposeBundle { claims: vec![] };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::BundleCreated(bundle) => {
            assert!(bundle.head_commit.is_some());
            assert_eq!(bundle.loc_changed, Some(0), "no changes = 0 loc");
        }
        other => panic!("expected BundleCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn done_action_constructs_noop_bundle() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::Done {
        message: "no code needed".into(),
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::Done(bundle) => {
            assert_eq!(bundle.noop_reason, Some("no code needed".to_string()));
            assert!(bundle.head_commit.is_none(), "Done Bundle has no HEAD capture");
            assert_eq!(bundle.loc_changed, None);
            assert!(!bundle.force_proposed);
        }
        other => panic!("expected Done variant, got {other:?}"),
    }
}

#[tokio::test]
async fn need_help_returns_reason_after_partial_commit() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    // Modify a tracked file so the partial commit has something to save.
    std::fs::write(wt_path.join("README.md"), "partial progress\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::NeedHelp {
        reason: "don't understand".into(),
    };
    let result = dispatch_action(action, &wt, &tools).await.unwrap();
    match result {
        ActionResult::NeedHelp(reason) => assert_eq!(reason, "don't understand"),
        other => panic!("expected NeedHelp, got {other:?}"),
    }
    // Partial commit must be visible in git log.
    let log = run_capture(wt_path, &["log", "-1", "--format=%s"]);
    assert!(log.contains("partial"), "expected partial commit: {log}");
}

#[tokio::test]
async fn need_help_on_clean_tree_does_not_commit() {
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    let before = run_capture(wt_path, &["rev-parse", "HEAD"]);
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::NeedHelp { reason: "clean".into() };
    let _ = dispatch_action(action, &wt, &tools).await.unwrap();
    let after = run_capture(wt_path, &["rev-parse", "HEAD"]);
    assert_eq!(before, after, "clean tree must not produce a commit");
}

#[test]
fn parse_numstat_sums_text_rows() {
    use super::parse_numstat;
    let input = "3\t1\tsrc/a.rs\n10\t5\tsrc/b.rs\n";
    assert_eq!(parse_numstat(input), 19);
}

#[test]
fn parse_numstat_skips_binary_rows() {
    use super::parse_numstat;
    // `-\t-` is git's marker for a binary file in numstat.
    let input = "3\t1\tsrc/a.rs\n-\t-\tassets/logo.png\n5\t0\tsrc/c.rs\n";
    assert_eq!(
        parse_numstat(input),
        9,
        "binary row must be skipped, not counted or errored"
    );
}

#[test]
fn parse_numstat_empty_input() {
    use super::parse_numstat;
    assert_eq!(parse_numstat(""), 0);
}

// ---------------------------------------------------------------
// RealTools seam tests (Stage 7 wiring doc, Phase 1)
// ---------------------------------------------------------------

use std::sync::Arc;

use tools::{BashDenylist, LaneRouter, SandboxMode};

use crate::dispatch::RealTools;

fn make_real_tools() -> RealTools {
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).expect("router init"));
    let denylist = Arc::new(BashDenylist::with_base());
    RealTools::new(router, SandboxMode::Off, denylist, vec![], None)
}

#[tokio::test]
async fn real_tools_read_returns_file_contents_as_json() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("hello.txt");
    std::fs::write(&file_path, "hello world\n").unwrap();

    let rt = make_real_tools();
    let input = json!({ "path": "hello.txt" });
    let output = rt.execute("read", &input, dir.path()).await.unwrap();

    let parsed: serde_json::Value = serde_json::from_str(&output).expect("output is JSON");
    let content = parsed.get("content").and_then(|v| v.as_str()).unwrap();
    // Read tool prepends line numbers cat-n style; assert the source text is present.
    assert!(
        content.contains("hello world"),
        "content missing source text: {content:?}"
    );
    let lines_total = parsed.get("lines_total").and_then(|v| v.as_u64()).unwrap();
    assert_eq!(lines_total, 1);
}

#[tokio::test]
async fn real_tools_read_missing_file_returns_error() {
    let dir = TempDir::new().unwrap();
    let rt = make_real_tools();
    let input = json!({ "path": "does-not-exist.txt" });
    let result = rt.execute("read", &input, dir.path()).await;
    match result {
        Err(DispatchError::Tool(msg)) => {
            assert!(!msg.is_empty(), "error message should be populated");
        }
        other => panic!("expected Err(DispatchError::Tool), got {other:?}"),
    }
}

#[tokio::test]
async fn real_tools_unknown_tool_name_returns_error() {
    let dir = TempDir::new().unwrap();
    let rt = make_real_tools();
    let input = json!({});
    let result = rt.execute("not-a-real-tool", &input, dir.path()).await;
    assert!(matches!(result, Err(DispatchError::Tool(_))));
}
