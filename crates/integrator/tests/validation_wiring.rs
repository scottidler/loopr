//! Full round-trip validation wiring tests. Exercises the complete flow:
//! real git tempdir + store + integrate() with validation_commands set.
//!
//! Asserts the two critical outcomes:
//! - Passing commands: Tick written, Bundle is Merged.
//! - Failing commands: no Tick, integration branch reset, Bundle is IntegrationFailed.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, TargetKind, Work};
use integrator::{IntegrationError, IntegratorConfig, IntegratorDeps, integrate};
use store::Store;

// ---------------------------------------------------------------------------
// Shared git helpers
// ---------------------------------------------------------------------------

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
// Fixture
// ---------------------------------------------------------------------------

async fn setup(plan: &Plan) -> (TempDir, Store, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();

    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join(".gitignore"), ".loopr/\n").unwrap();
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);

    let integ = format!("loopr/plan-{}", plan.id);
    git(&path, &["branch", &integ]);

    let loopr_dir = path.join(".loopr");
    std::fs::create_dir_all(&loopr_dir).unwrap();
    let store = Store::open(loopr_dir.to_str().unwrap()).await.unwrap();

    (dir, store, path)
}

fn sample_work(plan: &Plan) -> Work {
    let mut w = Work::new(plan.id.clone(), "implement feature".to_string());
    w.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["feature works".to_string()]);
    w
}

fn create_bundle_branch(repo: &Path, plan: &Plan, branch: &str, file: &str, contents: &str) -> String {
    let integ = format!("loopr/plan-{}", plan.id);
    git(repo, &["checkout", "-q", "-b", branch, &integ]);
    std::fs::write(repo.join(file), contents).unwrap();
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "commit",
            "-q",
            "-m",
            &format!("bundle commit for {branch}"),
            "--no-gpg-sign",
        ],
    );
    let sha = git_capture(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    sha
}

async fn persist_accepted_bundle(store: &Store, work: &Work, branch: &str, head: &str) -> Bundle {
    let mut b = Bundle::new(work.id.clone(), branch.to_string(), vec!["claim".to_string()]);
    b.head_commit = Some(head.to_string());
    b.paths = vec!["feature.rs".to_string()];
    store.bundles().create(b.clone()).await.unwrap();

    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store
        .bundles()
        .update(next.clone(), current.updated_at, Role::Reactor, TargetKind::Normal)
        .await
        .unwrap();

    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();
    store
        .bundles()
        .update(next.clone(), current.updated_at, Role::Reviewer, TargetKind::Normal)
        .await
        .unwrap();

    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Accepted, Role::Reactor).unwrap();
    store
        .bundles()
        .update(next.clone(), current.updated_at, Role::Reactor, TargetKind::Normal)
        .await
        .unwrap();

    store.bundles().get(&b.id).await.unwrap()
}

fn deps_with_validation<'a>(
    store: &'a Store,
    target: &Path,
    commands: Vec<String>,
) -> IntegratorDeps<&'a Store, &'a Store, &'a Store> {
    IntegratorDeps {
        bundle_sink: store,
        works: store,
        ticks: store,
        config: IntegratorConfig {
            validation_commands: commands,
            ..IntegratorConfig::default()
        },
        target: target.to_path_buf(),
        git_lock: Arc::new(AsyncMutex::new(())),
    }
}

// ---------------------------------------------------------------------------
// Round-trip: passing validation → Tick written, Bundle Merged
// ---------------------------------------------------------------------------

#[tokio::test]
async fn passing_validation_produces_tick_and_merged_bundle() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feature.rs", "pub fn f() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head).await;

    let deps = deps_with_validation(&store, &repo, vec!["true".to_string()]);
    let tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    assert_eq!(tick.plan_id, plan.id);

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Merged);

    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert_eq!(ticks.len(), 1);

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip: failing validation → no Tick, branch reset, Bundle IntegrationFailed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failing_validation_resets_branch_and_no_tick_written() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feature.rs", "pub fn f() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head).await;

    let integ = format!("loopr/plan-{}", plan.id);
    let pre_sha = git_capture(&repo, &["rev-parse", &integ]);

    let deps = deps_with_validation(&store, &repo, vec!["false".to_string()]);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;

    match result {
        Err(IntegrationError::ValidationFailed {
            ref command, exit_code, ..
        }) => {
            assert_eq!(command, "false");
            assert_eq!(exit_code, Some(1));
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }

    // Branch back at pre-merge SHA.
    let post_sha = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(pre_sha, post_sha, "integration branch should be reset");

    // Bundle is IntegrationFailed.
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);

    // No Tick.
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert!(ticks.is_empty(), "no Tick should exist after validation failure");

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Round-trip: first command passes, second fails → no Tick, rolled back
// ---------------------------------------------------------------------------

#[tokio::test]
async fn second_command_failure_rolls_back() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feature.rs", "pub fn f() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head).await;

    let integ = format!("loopr/plan-{}", plan.id);
    let pre_sha = git_capture(&repo, &["rev-parse", &integ]);

    let deps = deps_with_validation(&store, &repo, vec!["true".to_string(), "false".to_string()]);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;

    match result {
        Err(IntegrationError::ValidationFailed { ref command, .. }) => {
            assert_eq!(command, "false");
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }

    let post_sha = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(pre_sha, post_sha);

    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert!(ticks.is_empty());

    store.close().await.unwrap();
}
