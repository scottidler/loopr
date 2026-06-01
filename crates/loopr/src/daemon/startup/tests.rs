//! Tests for `daemon::startup::reconcile`.
//!
//! Exercises the three dispositional branches (terminal → cleanup, orphan →
//! log, non-terminal → carry forward) plus the foreign-branch skip. Uses a
//! real `Store` opened on a tempdir and a real `git init`'d repo so the
//! `worktree::list` + `Work` round-trip runs end-to-end.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use domain::{Bundle, BundleStatus, PlanId, Work, WorkId, WorkStatus};
use llm::ScriptedLlm;
use store::Store;
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tools::SandboxMode;

use super::*;

/// Initialize a fresh git repo at `path` with a single seed commit. GPG
/// signing is disabled so the test is hermetic.
fn seed_repo(path: &Path) {
    for args in [
        &["init", "-q", "--initial-branch=main"][..],
        &["config", "user.email", "test@example.com"][..],
        &["config", "user.name", "Test"][..],
        &["config", "commit.gpgsign", "false"][..],
        &["config", "tag.gpgsign", "false"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let out = std::process::Command::new("git")
            .current_dir(path)
            .env("LC_ALL", "C")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn resolve_head(path: &Path) -> String {
    let out = std::process::Command::new("git")
        .current_dir(path)
        .env("LC_ALL", "C")
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Set up: tempdir/repo with a seed commit and an opened Store. Returns
/// the tempdir (caller keeps alive), the repo path, and the store.
async fn setup() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);
    let store = Store::open(&repo).await.unwrap();
    (tmp, repo, store)
}

#[tokio::test]
async fn reconcile_on_empty_target_is_noop() {
    let (_tmp, repo, store) = setup().await;
    let report = sweep_worktrees(&repo, &store).await.unwrap();
    assert_eq!(report, ReconcileReport::default());
}

#[tokio::test]
async fn reconcile_cleans_terminal_done_worktree_and_deletes_branch() {
    let (_tmp, repo, store) = setup().await;
    let sha = resolve_head(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    // Create a worktree, persist a Work in `Done` state with the same id.
    let wt = worktree::Worktree::create(&repo, &wt_root, domain::WorkId::new(), &sha).unwrap();
    let work_id = wt.work_id().clone();
    let branch = wt.branch().to_string();
    let path = wt.path().to_path_buf();
    // Drop the handle WITHOUT cleanup so the worktree survives for
    // reconcile to find — but we need branch alive too. Use cleanup() (keeps
    // branch) and then... no, cleanup removes the worktree. We need the
    // worktree to survive. Forget the handle to suppress Drop-cleanup.
    std::mem::forget(wt);

    let plan_id = PlanId::new();
    let mut work = Work::new(plan_id, "title".into());
    work.id = work_id.clone();
    // Drive Work directly to terminal Done (test-only shortcut; production
    // code uses `Work::transition` which the FSM enforces).
    work.status = WorkStatus::Done;
    store.works().create(work).await.unwrap();

    let report = sweep_worktrees(&repo, &store).await.unwrap();
    assert_eq!(report.cleaned, 1, "terminal Done worktree should be cleaned");
    assert_eq!(report.orphans_logged, 0);
    assert_eq!(report.carried_forward, 0);

    assert!(!path.exists(), "worktree dir should be gone");

    // Branch should be deleted because status was Done.
    let out = std::process::Command::new("git")
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .args(["branch", "--list", &branch])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "Done → branch should be deleted"
    );
}

#[tokio::test]
async fn reconcile_carries_forward_non_terminal_worktree() {
    let (_tmp, repo, store) = setup().await;
    let sha = resolve_head(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let wt = worktree::Worktree::create(&repo, &wt_root, domain::WorkId::new(), &sha).unwrap();
    let work_id = wt.work_id().clone();
    let path = wt.path().to_path_buf();
    std::mem::forget(wt);

    let plan_id = PlanId::new();
    let mut work = Work::new(plan_id, "title".into());
    work.id = work_id.clone();
    // InProgress is non-terminal.
    work.status = WorkStatus::InProgress;
    store.works().create(work).await.unwrap();

    let report = sweep_worktrees(&repo, &store).await.unwrap();
    assert_eq!(report.cleaned, 0);
    assert_eq!(report.carried_forward, 1);
    assert!(path.exists(), "non-terminal worktree must survive reconcile");
}

#[tokio::test]
async fn reconcile_logs_orphan_when_store_has_no_work_record() {
    let (_tmp, repo, store) = setup().await;
    let sha = resolve_head(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");

    let wt = worktree::Worktree::create(&repo, &wt_root, domain::WorkId::new(), &sha).unwrap();
    let path = wt.path().to_path_buf();
    std::mem::forget(wt);

    // NOTE: no Work record created in store → orphan.
    let report = sweep_worktrees(&repo, &store).await.unwrap();
    assert_eq!(report.orphans_logged, 1);
    assert_eq!(report.cleaned, 0);
    assert_eq!(report.carried_forward, 0);
    assert!(path.exists(), "orphan worktree must be left alone for human review");
}

#[tokio::test]
async fn reconcile_skips_foreign_branch_under_worktree_root() {
    let (_tmp, repo, store) = setup().await;
    let sha = resolve_head(&repo);
    let wt_root = repo.join(".loopr").join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();

    // A human-created worktree living under our root with a non-loopr branch.
    let foreign_path = wt_root.join("human-experiment");
    let status = std::process::Command::new("git")
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .args([
            "worktree",
            "add",
            foreign_path.to_str().unwrap(),
            "-b",
            "feature/user-thing",
            &sha,
        ])
        .status()
        .unwrap();
    assert!(status.success());

    let report = sweep_worktrees(&repo, &store).await.unwrap();
    assert_eq!(report.foreign_skipped, 1);
    assert_eq!(report.cleaned, 0);
    assert!(foreign_path.exists(), "foreign worktree must NOT be cleaned");
}

// ---------- check_legacy_runs_dir ----------

#[test]
fn check_legacy_runs_dir_fresh_target_returns_false() {
    let td = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".loopr")).unwrap();
    assert!(!check_legacy_runs_dir(td.path()));
}

#[test]
fn check_legacy_runs_dir_absent_loopr_returns_false() {
    let td = tempfile::TempDir::new().unwrap();
    // No `.loopr/` at all.
    assert!(!check_legacy_runs_dir(td.path()));
}

#[test]
fn check_legacy_runs_dir_present_returns_true() {
    let td = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".loopr").join("runs")).unwrap();
    assert!(check_legacy_runs_dir(td.path()));
}

#[test]
fn check_legacy_runs_dir_file_at_runs_path_returns_false() {
    // If a plain file happens to sit at `.loopr/runs` (unusual but not
    // impossible) the detector should not treat it as legacy state.
    let td = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".loopr")).unwrap();
    std::fs::write(td.path().join(".loopr").join("runs"), b"").unwrap();
    assert!(!check_legacy_runs_dir(td.path()));
}

// ---------- sweep_bundles cold-boot re-drive (2026-05-31 seam doc, Phase 2) ----------

/// Build a `DaemonContext` on a seeded git repo with a `ScriptedLlm`. Mirrors
/// `tests/director_stuck_states.rs::build_test_context`, but in-crate so the
/// private `sweep_bundles` is reachable via `super::*`.
async fn build_ctx(target: &Path) -> Arc<DaemonContext<ScriptedLlm>> {
    let llm = ScriptedLlm::new();
    let session_id = SessionId::parse("20260531-000000").unwrap();
    let process_id = ProcessId::parse("pc-swp001").unwrap();
    let mut config = crate::config::Config::default();
    config.tools.sandbox = SandboxMode::Off;
    let snapshot = Arc::new(std::sync::Mutex::new(ProcessSnapshot::new("test-sweep-model")));
    crate::daemon::build_context(
        target.to_path_buf(),
        session_id,
        "-test-sweep".to_string(),
        process_id,
        0,
        llm,
        config,
        false,
        snapshot,
    )
    .await
    .unwrap()
}

fn seed_bundle(status: BundleStatus) -> Bundle {
    let mut b = Bundle::new(WorkId::new(), "loopr/wk-x-1".to_string(), vec!["claim".to_string()]);
    // Direct status set is a test-only shortcut: terminal statuses like
    // Merged are not reachable from Proposed via a single FSM edge.
    b.status = status;
    b
}

/// Cold-boot crash recovery: `sweep_bundles` must re-drive every stranded
/// non-terminal Bundle to its next stage (Proposed/Triaged -> reviewer,
/// Accepted/Integrating -> integrator) and count terminal Bundles without
/// re-driving them. A regression here silently strands the pipeline after a
/// daemon restart. `shutting_down` is set first so the spawned reviewer/
/// integrator task bodies short-circuit at entry (the documented defensive
/// guard), keeping the test deterministic and free of background work while
/// `sweep_bundles` still records its routing counters.
#[tokio::test]
async fn sweep_bundles_redrives_stranded_and_counts_terminal_on_cold_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    seed_repo(&repo);
    let ctx = build_ctx(&repo).await;

    // Make every spawned task body no-op at entry.
    ctx.shutting_down.store(true, std::sync::atomic::Ordering::Relaxed);

    for status in [
        BundleStatus::Triaged,  // -> reviewer requeue
        BundleStatus::Accepted, // -> integrator requeue
        BundleStatus::Merged,   // -> terminal count
        BundleStatus::Rejected, // -> terminal count
    ] {
        ctx.store.bundles().create(seed_bundle(status)).await.unwrap();
    }

    let mut report = ReconcileReport::default();
    sweep_bundles(&ctx, &mut report).await.unwrap();

    assert_eq!(report.reviewers_requeued, 1, "Triaged bundle re-driven to reviewer");
    assert_eq!(
        report.integrators_requeued, 1,
        "Accepted bundle re-driven to integrator"
    );
    assert_eq!(report.bundles_terminal, 2, "Merged + Rejected counted as terminal");

    // Drain the (no-op) spawned task pools so nothing leaks past the test.
    ctx.reviewer_tasks.lock().await.shutdown().await;
    ctx.integrator_tasks.lock().await.shutdown().await;
}
