//! Phase 6 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! Drives a real `propose_bundle` against a tempdir worktree under
//! the production telemetry subscriber, then asserts events.log
//! carries the canonical "implementer produced bundle" event with
//! the manifest fields (paths_added/paths_modified/paths_deleted +
//! patch_id + diff_bytes).

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use serde_json::Value;
use tempfile::TempDir;

use agents::{ActionResult, AgentAction, DispatchError, ToolExecutor, dispatch_action};
use domain::{PlanId, Work};
use worktree::Worktree;

struct NoopTools;
impl ToolExecutor for NoopTools {
    async fn execute(&self, _tool: &str, _input: &serde_json::Value, _wd: &Path) -> Result<String, DispatchError> {
        Ok(String::new())
    }
}

// ---------- Harness ----------

fn read_events(run_dir: &Path) -> Vec<Value> {
    let body = fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("parse JSONL line {line:?}: {e}")))
        .collect()
}

fn init_repo() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    fs::write(path.join("README.md"), "initial\n").unwrap();
    fs::write(path.join("oldfile.txt"), "old\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------- Scenario ----------

#[tokio::test]
async fn propose_bundle_emits_manifest_event() {
    let (_dir, repo_path, sha) = init_repo();
    let worktree_root = repo_path.parent().unwrap().join("manifest-wts");
    fs::create_dir_all(&worktree_root).unwrap();
    let mut work = Work::new(PlanId::new(), "manifest visibility".to_string());
    work.files = vec![
        "newfile.rs".to_string(),
        "README.md".to_string(),
        "oldfile.txt".to_string(),
    ];
    let wt = Worktree::create(&repo_path, &worktree_root, work.id.clone(), &sha).unwrap();
    let wt_path = wt.path();

    // Stage one of each: a new file (added), an edit (modified), and a
    // deletion. The bundle's manifest should classify them respectively.
    fs::write(wt_path.join("newfile.rs"), "fn new_fn() {}\n").unwrap();
    fs::write(wt_path.join("README.md"), "v2\n").unwrap();
    fs::remove_file(wt_path.join("oldfile.txt")).unwrap();
    git(wt_path, &["add", "-A"]);
    git(wt_path, &["commit", "-q", "-m", "implement", "--no-gpg-sign"]);

    let log_dir = TempDir::new().unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let action = AgentAction::ProposeBundle {
            claims: vec!["did the work".into()],
        };
        let result = dispatch_action(action, &work, &wt, &NoopTools).await.unwrap();
        assert!(matches!(result, ActionResult::BundleCreated { .. }));
    }

    let events = read_events(log_dir.path());

    let manifest_event = events
        .iter()
        .find(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str())
                == Some("implementer produced bundle")
        })
        .expect(
            "expected `implementer produced bundle` event from agents::dispatch::propose_bundle. \
             If this fails, the daemon-context site may have re-emitted; the canonical site is now agents::dispatch.",
        );
    let f = manifest_event.get("fields").expect("manifest event fields");

    // Manifest fields: each must appear and the classification must be sane.
    assert!(f.get("bundle_id").is_some(), "missing bundle_id");
    assert!(f.get("work_id").is_some(), "missing work_id");
    assert!(f.get("paths_added").is_some(), "missing paths_added");
    assert!(f.get("paths_modified").is_some(), "missing paths_modified");
    assert!(f.get("paths_deleted").is_some(), "missing paths_deleted");
    assert!(f.get("patch_id").is_some(), "missing patch_id");
    let diff_bytes = f.get("diff_bytes").and_then(|v| v.as_u64()).expect("diff_bytes is u64");
    assert!(
        diff_bytes > 0,
        "diff_bytes should be > 0 for a non-empty implementer commit"
    );
    let patch_id = f.get("patch_id").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        !patch_id.is_empty() && patch_id != "oversize",
        "patch_id should be non-empty and not oversize for a tiny commit; got {patch_id}"
    );

    // Sanity-check classification: the format!("{:?}", Vec<String>) form
    // ends up as a Debug-formatted string in the JSON value (rust's
    // ?-prefix), so we look for the file names by substring rather than
    // by parsing the array form.
    let added = f.get("paths_added").map(|v| v.to_string()).unwrap_or_default();
    let modified = f.get("paths_modified").map(|v| v.to_string()).unwrap_or_default();
    let deleted = f.get("paths_deleted").map(|v| v.to_string()).unwrap_or_default();
    assert!(
        added.contains("newfile.rs"),
        "newfile.rs should be in paths_added: {added}"
    );
    assert!(
        modified.contains("README.md"),
        "README.md should be in paths_modified: {modified}"
    );
    assert!(
        deleted.contains("oldfile.txt"),
        "oldfile.txt should be in paths_deleted: {deleted}"
    );

    // Phase 6 also asserts the loopr::daemon::context duplicate is gone.
    // We can't dispatch from inside the daemon here, but we can confirm
    // exactly ONE `implementer produced bundle` event lands per dispatch
    // call.
    let count = events
        .iter()
        .filter(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str())
                == Some("implementer produced bundle")
        })
        .count();
    assert_eq!(
        count, 1,
        "expected exactly one `implementer produced bundle` event per dispatch; got {count}"
    );
}
