//! Seam test: when `Store::update` returns `Stale` from inside
//! `run_reviewer`, the reviewer surfaces it as
//! `ReviewerError::Update(BundleUpdateError::Stale { .. })` without
//! swallowing or remapping.
//!
//! Replaces the deleted `seam_reviewer_concurrency.rs`. That test
//! tried to manufacture the race with a `Barrier::new(2)` and ran
//! two `run_reviewer` tasks concurrently; if either task hit any of
//! `run_reviewer`'s pre-LLM early returns (`bundle.work_id` check,
//! `git_show` subprocess, `build_for_reviewer`), the other task
//! parked forever on the barrier. The wall-clock OCC contention
//! invariant ("exactly one of N concurrent writers wins") is already
//! pinned by `crates/store/src/bundles/tests.rs::concurrent_updates_produce_exactly_one_winner`;
//! the snapshot-mismatch invariant is pinned by `update_stale_version_rejected`
//! in the same file. The only piece left to cover at the agents seam
//! is propagation of `Stale` through `run_reviewer`'s error type,
//! which this test does with a deterministic pre-loaded stale
//! snapshot — no concurrency, no barrier, no hang.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use tempfile::TempDir;

use agents::{ReviewerConfig, ReviewerDeps, ReviewerError, run_reviewer};
use context::InlineContextBuilder;
use domain::{AcceptanceCriteria, Bundle, BundleStatus, Role, TargetKind, Work};
use llm::ScriptedLlm;
use store::{BundleUpdateError, Store};

// ---------------------------------------------------------------------------
// Git helpers (copied from seam_reviewer.rs)
// ---------------------------------------------------------------------------

fn init_repo_with_commit() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    std::fs::write(path.join("src.rs"), "fn hello() -> u32 { 42 }\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "add src", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reviewer_propagates_store_stale() {
    let (_dir, repo_path, sha) = init_repo_with_commit();
    let store = Store::open(&repo_path).await.unwrap();

    let plan = domain::Plan::new("stale propagation plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "add hello()".to_string());
    work.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["hello returns 42".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(
        work.id.clone(),
        "loopr/wk-stale".to_string(),
        vec!["added hello".to_string()],
    );
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    // Snapshot the bundle BEFORE manufacturing staleness. This is the
    // snapshot `run_reviewer` will pass to `store.update(..., expected_updated_at=stale.updated_at)`.
    let stale = store.bundles().get(&bundle.id).await.unwrap();

    // Manufacture staleness deterministically: transition the on-disk
    // bundle to Reviewed via a direct store update. This bumps
    // `updated_at` so the snapshot above no longer matches what's on
    // disk by the time `run_reviewer` tries to commit.
    let mut external = stale.clone();
    external.verification = "external reviewer beat us".to_string();
    external.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();
    store
        .bundles()
        .update(external, stale.updated_at, Role::Reviewer, TargetKind::Normal)
        .await
        .unwrap();

    // Now run the reviewer with the original (now-stale) snapshot. The
    // reviewer's internal `bundle.clone()` -> transition -> store.update
    // path uses `stale.updated_at` as `expected_updated_at`, which the
    // store will reject as Stale.
    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(
        r#"{"kind":"accept","summary":"AC met: hello returns 42"}"#.to_string()
    ));

    let deps = ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path,
        path_deny_patterns: Vec::new(),
    };

    let result = run_reviewer(&stale, &work, &deps).await;

    assert!(
        matches!(result, Err(ReviewerError::Update(BundleUpdateError::Stale { .. }))),
        "reviewer must propagate Stale, got {result:?}"
    );

    // The on-disk bundle still reflects the external winner, not the
    // late reviewer's attempt.
    let after = deps.store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(after.status, BundleStatus::Reviewed);
    assert_eq!(after.verification, "external reviewer beat us");

    deps.store.close().await.unwrap();
}
