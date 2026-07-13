use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use domain::WorkId;
use tracing_subscriber::layer::SubscriberExt;

use super::*;

// ---------- delete_branch / cleanup_at guards (Phase-5 finding 12) ----------

#[test]
fn delete_branch_rejects_non_loopr_branch() {
    // A buggy caller must never reach `git branch -D main`.
    let err = delete_branch(Path::new("/tmp/repo"), "main").unwrap_err();
    assert!(matches!(err, WorktreeError::InvalidBranchName(b) if b == "main"));
}

// ---------- delete_branch ERROR hygiene (caller owns severity) ----------
//
// Log assertions use a JSON `tracing` subscriber captured into a byte
// buffer, mirroring `crates/store/src/works/tests.rs`. `with_default` (not
// `set_default`) scopes the subscriber to the single synchronous call under
// test, and `LOG_LOCK` serializes the two subscriber-installing tests below
// against each other - both share this unit-test binary with every other
// `#[cfg(test)] mod tests` in this crate, and concurrent subscriber installs
// race the process-global `tracing` interest cache (the known flaky-test
// class documented on `store`'s equivalent tests).

static LOG_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct VecWriter(Arc<Mutex<Vec<u8>>>);

impl VecWriter {
    fn snapshot(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }
}

impl std::io::Write for VecWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecWriter {
    type Writer = VecWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn json_subscriber(writer: VecWriter) -> impl tracing::Subscriber + Send + Sync {
    let layer = tracing_subscriber::fmt::layer().json().with_writer(writer);
    tracing_subscriber::registry().with(layer)
}

/// Count JSON log lines at `level`, regardless of message content.
fn count_at_level(json: &str, level: &str) -> usize {
    json.lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("level").and_then(|l| l.as_str().map(|s| s == level)))
                .unwrap_or(false)
        })
        .count()
}

/// Initialize a fresh git repo at `path` with a single seed commit and
/// return the commit SHA. Disables GPG signing so tests are hermetic
/// regardless of the host's `~/.gitconfig`.
fn seed_repo(path: &Path) -> String {
    for args in [
        &["init", "-q", "--initial-branch=main"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Test"][..],
        &["config", "commit.gpgsign", "false"][..],
        &["config", "tag.gpgsign", "false"][..],
    ] {
        let out = ops::git_cmd(path).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = ops::git_cmd(path)
        .args(["commit", "-q", "--allow-empty", "-m", "init"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let out = ops::git_cmd(path).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A tolerated git-failure delete (branch still checked out by a live
/// worktree) must return the typed error WITHOUT logging ERROR at either
/// `worktree::delete_branch` or `ops::delete_branch` - severity here belongs
/// to the caller (integrator/reap WARN best-effort; startup propagates
/// fatally and is logged loud by the startup-failure path).
///
/// Break-to-proven: restoring the `err` clause on both `#[instrument]`s
/// makes this test fail (an ERROR fires for the tolerated case).
#[test]
fn delete_branch_tolerated_git_failure_does_not_log_error() {
    let _serialize = LOG_LOCK.lock().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let sha = seed_repo(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    // Create the worktree OUTSIDE the captured-log scope so `Worktree::create`'s
    // own spans/events don't pollute the ERROR-event assertion below.
    let wk = WorkId::from_str("wk-abc12").unwrap();
    let wt = Worktree::create(&repo, &wt_root, wk, &sha).unwrap();
    let branch = wt.branch().to_string();

    let writer = VecWriter::default();
    let result = tracing::subscriber::with_default(json_subscriber(writer.clone()), || {
        // git refuses: `branch` is checked out by `wt`'s still-live worktree.
        delete_branch(&repo, &branch)
    });

    assert!(
        matches!(result, Err(WorktreeError::GitCommand(_))),
        "expected a tolerated git failure, got {result:?}"
    );
    let log = writer.snapshot();
    assert_eq!(
        count_at_level(&log, "ERROR"),
        0,
        "tolerated git failure must not log ERROR; log: {log}"
    );

    wt.cleanup().unwrap();
}

/// The Finding-12 guard refusal (a caller bug, not a tolerated race) must
/// stay loud even with the blanket `err` gone from both `#[instrument]`s.
#[test]
fn delete_branch_guard_refusal_logs_error() {
    let _serialize = LOG_LOCK.lock().unwrap();

    let writer = VecWriter::default();
    let result = tracing::subscriber::with_default(json_subscriber(writer.clone()), || {
        delete_branch(Path::new("/tmp/repo"), "main")
    });

    assert!(matches!(result, Err(WorktreeError::InvalidBranchName(b)) if b == "main"));
    let log = writer.snapshot();
    assert_eq!(
        count_at_level(&log, "ERROR"),
        1,
        "guard refusal must log exactly one ERROR; log: {log}"
    );
    assert!(
        log.contains("main"),
        "ERROR log should carry the offending branch name; log: {log}"
    );
}

#[test]
fn delete_branch_rejects_plain_feature_branch() {
    let err = delete_branch(Path::new("/tmp/repo"), "feature/x").unwrap_err();
    assert!(matches!(err, WorktreeError::InvalidBranchName(_)));
}

#[test]
fn under_worktrees_root_accepts_loopr_worktree_path() {
    assert!(under_worktrees_root(Path::new(
        "/home/me/proj/.loopr/worktrees/wk-abc12-1"
    )));
}

#[test]
fn under_worktrees_root_rejects_arbitrary_path() {
    assert!(!under_worktrees_root(Path::new("/home/me/proj/src")));
    assert!(!under_worktrees_root(Path::new("/home/me")));
    // `.loopr` without the `worktrees` child does not qualify.
    assert!(!under_worktrees_root(Path::new("/home/me/proj/.loopr/records/x")));
}

#[test]
fn cleanup_at_rejects_path_outside_worktrees_root() {
    let err = cleanup_at(Path::new("/tmp/repo"), &PathBuf::from("/home/me/proj/src")).unwrap_err();
    assert!(matches!(err, WorktreeError::NotFound(_)));
}
