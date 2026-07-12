//! Structural conflict seam test. Multi-Bundle slice with two Bundles
//! that both touch README.md; first merges, second conflicts on
//! README.md. Requires `allow_multi_bundle = true`.
//!
//! The batched-commit invariant (store writes deferred until the full
//! git sequence succeeds) means the Store and git agree after
//! rollback: git is reset to `pre_merge` (first Bundle's merge
//! rolled out), and EVERY Bundle in the slice is IntegrationFailed in
//! the Store. No FSM-git divergence. This was the original motivation
//! for the Phase 2/Phase 3 split in the design doc.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, TargetKind, Work};
use integrator::{IntegrationError, IntegratorConfig, IntegratorDeps, integrate};
use store::Store;

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

async fn persist_accepted_bundle(
    store: &Store,
    work: &Work,
    branch_name: &str,
    head_commit: &str,
    paths: Vec<String>,
) -> Bundle {
    let mut b = Bundle::new(work.id.clone(), branch_name.to_string(), vec!["claim".to_string()]);
    b.head_commit = Some(head_commit.to_string());
    b.paths = paths;
    store.bundles().create(b.clone()).await.unwrap();

    for (target, role) in [
        (BundleStatus::Triaged, Role::Reactor),
        (BundleStatus::Reviewed, Role::Reviewer),
        (BundleStatus::Accepted, Role::Reactor),
    ] {
        let current = store.bundles().get(&b.id).await.unwrap();
        let mut next = current.clone();
        next.transition(target, role).unwrap();
        store
            .bundles()
            .update(next.clone(), current.updated_at, role, TargetKind::Normal)
            .await
            .unwrap();
    }

    store.bundles().get(&b.id).await.unwrap()
}

#[tokio::test]
async fn structural_conflict_two_bundles_touching_readme() {
    // Setup: repo with initial commit + integration branch.
    let dir = TempDir::new().unwrap();
    let repo: PathBuf = dir.path().to_path_buf();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    // Keep the Store's .loopr/taskstore/*.jsonl files out of every
    // commit; otherwise switching branches would remove them from the
    // working tree and invalidate the Store's file handles.
    std::fs::write(repo.join(".gitignore"), ".loopr/\n").unwrap();
    std::fs::write(repo.join("README.md"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let base = git_capture(&repo, &["rev-parse", "HEAD"]);

    let plan = Plan::new("ship".to_string());
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["branch", &integ]);

    let store = Store::open(&repo).await.unwrap();
    store.plans().create(plan.clone()).await.unwrap();

    // Two Works, two Bundles, each touching README.md with different
    // content.
    let mut work_a = Work::new(plan.id.clone(), "feature A".to_string());
    work_a.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["a works".to_string()]);
    store.works().create(work_a.clone()).await.unwrap();

    let mut work_b = Work::new(plan.id.clone(), "feature B".to_string());
    work_b.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["b works".to_string()]);
    store.works().create(work_b.clone()).await.unwrap();

    let branch_a = format!("loopr/wk-{}", work_a.id);
    git(&repo, &["checkout", "-q", "-b", &branch_a, &base]);
    std::fs::write(repo.join("README.md"), "A-side\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "A changes readme", "--no-gpg-sign"]);
    let head_a = git_capture(&repo, &["rev-parse", "HEAD"]);

    let branch_b = format!("loopr/wk-{}", work_b.id);
    git(&repo, &["checkout", "-q", "-b", &branch_b, &base]);
    std::fs::write(repo.join("README.md"), "B-side\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "B changes readme", "--no-gpg-sign"]);
    let head_b = git_capture(&repo, &["rev-parse", "HEAD"]);

    git(&repo, &["checkout", "-q", "main"]);

    let bundle_a = persist_accepted_bundle(&store, &work_a, &branch_a, &head_a, vec!["README.md".to_string()]).await;
    let bundle_b = persist_accepted_bundle(&store, &work_b, &branch_b, &head_b, vec!["README.md".to_string()]).await;

    // Capture pre-call integration branch SHA for rollback assertion.
    let pre_call_integ = git_capture(&repo, &["rev-parse", &integ]);

    // Deps: allow_multi_bundle = true for this test only.
    let cfg = IntegratorConfig {
        allow_multi_bundle: true,
        ..IntegratorConfig::default()
    };
    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config: cfg,
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };

    let result = integrate(&[bundle_a.clone(), bundle_b.clone()], &plan, &deps).await;
    match result {
        Err(IntegrationError::ConflictStructural {
            bundle_id,
            files,
            peer_bundle_ids,
        }) => {
            // The failing Bundle is the SECOND (b); its peer in the
            // slice is the first (a).
            assert_eq!(bundle_id, bundle_b.id.as_ref());
            assert_eq!(files, vec!["README.md".to_string()]);
            assert_eq!(peer_bundle_ids, vec![bundle_a.id.as_ref().to_string()]);
        }
        other => panic!("expected ConflictStructural, got {other:?}"),
    }

    // Integration branch rolled back to pre-call state.
    let post_integ = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(
        post_integ, pre_call_integ,
        "integration branch must be reset to pre_merge; git rolled back both merges"
    );

    // Batched-commit invariant: store and git agree. Both Bundles are
    // IntegrationFailed in the store; git has reset to pre_merge
    // (verified above); bundle_a was never observed at status
    // `Merged` in the store because the `Merged` transition is
    // batched in Phase 3 and only runs when the full git sequence
    // succeeds. If bundle_a had been observed at `Merged` mid-loop,
    // that would be the FSM-git divergence the design doc set out
    // to eliminate.
    let a_final = store.bundles().get(&bundle_a.id).await.unwrap();
    let b_final = store.bundles().get(&bundle_b.id).await.unwrap();
    assert_eq!(a_final.status, BundleStatus::IntegrationFailed);
    assert_eq!(b_final.status, BundleStatus::IntegrationFailed);

    store.close().await.unwrap();
}
