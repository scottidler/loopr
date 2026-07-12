#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use serde_json::json;
use tempfile::TempDir;

use domain::{PlanId, Work, WorkId};
use worktree::Worktree;

use crate::action::AgentAction;
use crate::dispatch::{ActionResult, CommitContext, DispatchError, ToolExecutor, dispatch_action};

/// Build a `Work` with the given scope `files`. Tests that don't care
/// about scope pass `vec![]` and rely on the empty-scope fallback
/// (artifact-only filtering, which still commits every non-`.loopr/`
/// dirty path).
fn test_work(files: Vec<String>) -> Work {
    let mut w = Work::new(PlanId::new(), "test work".to_string());
    w.files = files;
    w
}

/// In-memory tool executor fake. Records each call and returns a
/// canned string. Never touches real subprocesses.
struct FakeTools {
    response: String,
}

impl ToolExecutor for FakeTools {
    async fn execute(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<String, DispatchError> {
        if tool_name == "fail" {
            Err(DispatchError::Tool("synthetic tool failure".into()))
        } else {
            Ok(self.response.clone())
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

fn fake_worktree(path: &Path, sha: String) -> Worktree {
    // We can't use from_parts directly (it's pub(crate) in worktree).
    // Instead: the dispatcher only reads path/work_id/branch/sha.
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
    Worktree::create(path, &worktree_root, WorkId::new(), &sha).unwrap()
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    assert!(
        matches!(result, ActionResult::NothingToCommit { .. }),
        "expected NothingToCommit, got {result:?}"
    );
}

#[tokio::test]
async fn commit_changes_with_empty_scope_stages_new_files() {
    // Empty `Work.files` triggers the artifact-only filter: every
    // non-`.loopr/` dirty path is in-scope, including untracked new
    // files. Verifies that `git add -- <in_scope> && git commit --only`
    // promotes a new file into the commit.
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Committed { sha, dropped } => {
            assert!(dropped.is_empty(), "no scope set, no drops expected: {dropped:?}");
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::BundleCreated { bundle, dropped } => {
            assert!(dropped.is_empty(), "no scope set, no drops expected: {dropped:?}");
            assert!(bundle.head_commit.is_some(), "head_commit must be captured");
            assert_eq!(bundle.head_commit.as_ref().unwrap().len(), 40, "full SHA expected");
            assert_eq!(bundle.claims, vec!["changed readme".to_string()]);
            // Base had README.md = "initial\n" (1 line). New content is 3 lines.
            // Diff: -1 / +3 = 4 lines changed.
            assert_eq!(bundle.loc_changed, Some(4), "loc_changed must reflect diff vs sha");
            assert!(!bundle.force_proposed, "force_proposed false on normal propose");
        }
        other => panic!("expected BundleCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_with_no_commits_errors() {
    // Finding 1: a propose with no new commits leaves HEAD at the
    // worktree BASE sha; capturing that would have the reviewer review
    // the base commit's foreign diff. The dispatcher must reject instead.
    // (Inverts the former `propose_bundle_with_no_changes_still_captures_head`,
    // which pinned the wrong behavior.)
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::ProposeBundle { claims: vec![] };
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Error(msg) => {
            assert!(msg.contains("no commits to propose"), "got: {msg}");
        }
        other => panic!("expected Error (no commits to propose), got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_rejects_out_of_scope_branch_path() {
    // Phase 14 scope gate. If a path OUTSIDE the Work's `files` scope
    // reached a commit outside the scoped dispatcher (here: a raw
    // `git add -A && git commit`, standing in for a bash-tool bypass),
    // `propose_bundle` must reject with a typed reason naming the path,
    // NOT construct a Bundle. Break-to-prove: deleting the scope gate in
    // dispatch.rs makes this return BundleCreated and fail the test.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    // One in-scope file and one out-of-scope file, both committed directly
    // (bypassing the dispatcher's scoped staging).
    std::fs::create_dir_all(wt_path.join("src")).unwrap();
    std::fs::write(wt_path.join("src/foo.rs"), "fn foo() {}\n").unwrap();
    std::fs::write(wt_path.join("secret.txt"), "leak\n").unwrap();
    run(wt_path, &["add", "-A"]);
    run(
        wt_path,
        &["commit", "-q", "-m", "sneak out-of-scope file", "--no-gpg-sign"],
    );

    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::ProposeBundle {
        claims: vec!["did the work".into()],
    };
    let work = test_work(vec!["src/foo.rs".to_string()]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Error(msg) => {
            assert!(msg.contains("outside this Work's `files` scope"), "got: {msg}");
            assert!(msg.contains("secret.txt"), "reason must name the offending path: {msg}");
        }
        other => panic!("expected Error (out-of-scope propose rejection), got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_accepts_in_scope_directory_prefix() {
    // Complement to the rejection test: a directory-prefix scope (`src/`)
    // admits every path under it, so a bundle touching only `src/**` is
    // accepted, not rejected.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    std::fs::create_dir_all(wt_path.join("src/nested")).unwrap();
    std::fs::write(wt_path.join("src/foo.rs"), "fn foo() {}\n").unwrap();
    std::fs::write(wt_path.join("src/nested/bar.rs"), "fn bar() {}\n").unwrap();
    run(wt_path, &["add", "-A"]);
    run(wt_path, &["commit", "-q", "-m", "in-scope work", "--no-gpg-sign"]);

    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::ProposeBundle {
        claims: vec!["did the work".into()],
    };
    let work = test_work(vec!["src/".to_string()]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::BundleCreated { bundle, .. } => {
            assert!(bundle.head_commit.is_some(), "head_commit must be captured");
        }
        other => panic!("expected BundleCreated for in-scope dir prefix, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_emits_loopr_trailers() {
    // Finding 6: every agent commit carries Loopr-* trailers. Default
    // posture is unsigned (--no-gpg-sign appended); the trailers ride
    // regardless.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    std::fs::write(wt.path().join("a.rs"), "fn a() {}\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let work = test_work(vec![]);
    let ctx = CommitContext {
        run_id: Some("pc-abc123".to_string()),
        plan_id: "p-0001".to_string(),
        work_id: "wk-0001".to_string(),
        role: "implementer".to_string(),
        model: Some("claude-sonnet-4-6".to_string()),
        gpg_sign: false,
    };
    let action = AgentAction::CommitChanges { message: "do a".into() };
    let result = dispatch_action(action, &work, &wt, &tools, &ctx).await.unwrap();
    assert!(matches!(result, ActionResult::Committed { .. }), "got {result:?}");
    // git normalizes `--trailer key=value` to `Key: value`.
    let body = run_capture(wt.path(), &["log", "-1", "--format=%B"]);
    assert!(body.contains("Loopr-Run: pc-abc123"), "got: {body}");
    assert!(body.contains("Loopr-Plan: p-0001"), "got: {body}");
    assert!(body.contains("Loopr-Work: wk-0001"), "got: {body}");
    assert!(body.contains("Loopr-Role: implementer"), "got: {body}");
    assert!(body.contains("Loopr-Model: claude-sonnet-4-6"), "got: {body}");
}

#[tokio::test]
async fn done_bundle_carries_scope_and_message_evidence() {
    // Finding 5: a noop (`Done`) bundle must give the reviewer evidence -
    // the Work's scope as `paths` (files to read) and the operator
    // message as both a claim and `noop_reason`.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let tools = FakeTools {
        response: String::new(),
    };
    let work = test_work(vec!["src/lib.rs".to_string()]);
    let action = AgentAction::Done {
        message: "no code needed; spec is doc-only".into(),
    };
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Done(bundle) => {
            assert_eq!(
                bundle.paths,
                vec!["src/lib.rs".to_string()],
                "noop bundle carries work scope"
            );
            assert_eq!(bundle.claims, vec!["no code needed; spec is doc-only".to_string()]);
            assert_eq!(bundle.noop_reason.as_deref(), Some("no code needed; spec is doc-only"));
        }
        other => panic!("expected Done, got {other:?}"),
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
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
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
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
    let work = test_work(vec![]);
    let _ = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    let after = run_capture(wt_path, &["rev-parse", "HEAD"]);
    assert_eq!(before, after, "clean tree must not produce a commit");
}

// ---------------------------------------------------------------
// Scoped-staging tests (docs/design/2026-04-26-scoped-staging.md)
// ---------------------------------------------------------------

#[tokio::test]
async fn commit_changes_with_nonempty_scope_drops_out_of_scope_files() {
    // (a) commit_changes with non-empty scope drops out-of-scope files;
    // the resulting Committed.dropped lists them.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    std::fs::write(wt_path.join("main.py"), "x = 1\n").unwrap();
    std::fs::write(wt_path.join("database.py"), "y = 2\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::CommitChanges {
        message: "scoped commit".into(),
    };
    let work = test_work(vec!["main.py".to_string()]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Committed { sha, dropped } => {
            assert_eq!(sha.len(), 40);
            assert_eq!(dropped, vec!["database.py".to_string()]);
            // HEAD's diff against the previous commit should list main.py and not database.py.
            let names = run_capture(wt_path, &["show", "--name-only", "--format=", "HEAD"]);
            let names_set: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(names_set, vec!["main.py"]);
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_changes_with_empty_scope_still_drops_loopr_artifacts() {
    // (b) Empty scope still drops `.loopr/` artifacts (defensive parity).
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    std::fs::write(wt_path.join("main.py"), "x = 1\n").unwrap();
    std::fs::create_dir_all(wt_path.join(".loopr/runs/r-1")).unwrap();
    std::fs::write(wt_path.join(".loopr/runs/r-1/log"), "noise\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::CommitChanges {
        message: "scoped commit".into(),
    };
    let work = test_work(vec![]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Committed { dropped, .. } => {
            assert_eq!(dropped, vec![".loopr/runs/r-1/log".to_string()]);
            let names = run_capture(wt_path, &["show", "--name-only", "--format=", "HEAD"]);
            let names_set: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(names_set, vec!["main.py"]);
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_changes_does_not_commit_previously_staged_out_of_scope_file() {
    // (c) Index-leak regression: pre-stage `database.py` via `git add`,
    // then call commit_changes with scope ["main.py"]. HEAD's
    // `git show --name-only` must list only main.py.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    std::fs::write(wt_path.join("database.py"), "old\n").unwrap();
    run(wt_path, &["add", "database.py"]);
    std::fs::write(wt_path.join("main.py"), "x = 1\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let action = AgentAction::CommitChanges {
        message: "scoped commit".into(),
    };
    let work = test_work(vec!["main.py".to_string()]);
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Committed { dropped, .. } => {
            // database.py was pre-staged so porcelain reports it; partition
            // drops it for being out-of-scope.
            assert!(
                dropped.iter().any(|p| p == "database.py"),
                "database.py must appear in dropped: {dropped:?}"
            );
            let names = run_capture(wt_path, &["show", "--name-only", "--format=", "HEAD"]);
            let names_set: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(
                names_set,
                vec!["main.py"],
                "only main.py should land in HEAD; database.py must NOT leak through the index"
            );
        }
        other => panic!("expected Committed, got {other:?}"),
    }
}

#[tokio::test]
async fn propose_bundle_paths_reflect_full_branch_diff() {
    // (d) propose_bundle populates `bundle.paths` from the full branch-vs-base
    // diff, including paths landed by earlier commit_changes actions on the
    // same iteration.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();

    // Iteration 1: commit_changes against scope ["main.py"].
    std::fs::write(wt_path.join("main.py"), "x = 1\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let work = test_work(vec!["main.py".to_string(), "test_api.py".to_string()]);
    let action1 = AgentAction::CommitChanges {
        message: "iter 1".into(),
    };
    let _ = dispatch_action(action1, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();

    // Iteration 2: write test_api.py, then propose_bundle.
    std::fs::write(wt_path.join("test_api.py"), "assert True\n").unwrap();
    let action2 = AgentAction::ProposeBundle { claims: vec![] };
    let result = dispatch_action(action2, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::BundleCreated { bundle, .. } => {
            let mut paths = bundle.paths.clone();
            paths.sort();
            assert_eq!(
                paths,
                vec!["main.py".to_string(), "test_api.py".to_string()],
                "bundle.paths must reflect full branch diff: got {:?}",
                bundle.paths
            );
        }
        other => panic!("expected BundleCreated, got {other:?}"),
    }
}

#[tokio::test]
async fn commit_changes_enumerates_files_in_untracked_dir() {
    // (e) `--untracked-files=all` regression test: writing a new file
    // inside a brand-new untracked directory must let scope ["new_dir/main.py"]
    // match it. Without `-uall` git would emit `?? new_dir/` and the
    // exact-path partition would treat the file as out-of-scope.
    let (_dir, path, base) = init_repo();
    let wt = fake_worktree(&path, base);
    let wt_path = wt.path();
    std::fs::create_dir_all(wt_path.join("new_dir")).unwrap();
    std::fs::write(wt_path.join("new_dir/main.py"), "x = 1\n").unwrap();
    let tools = FakeTools {
        response: String::new(),
    };
    let work = test_work(vec!["new_dir/main.py".to_string()]);
    let action = AgentAction::CommitChanges {
        message: "untracked dir".into(),
    };
    let result = dispatch_action(action, &work, &wt, &tools, &CommitContext::test())
        .await
        .unwrap();
    match result {
        ActionResult::Committed { dropped, .. } => {
            assert!(dropped.is_empty(), "no drops expected: {dropped:?}");
            let names = run_capture(wt_path, &["show", "--name-only", "--format=", "HEAD"]);
            let names_set: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
            assert_eq!(names_set, vec!["new_dir/main.py"]);
        }
        other => panic!("expected Committed, got {other:?}"),
    }
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
