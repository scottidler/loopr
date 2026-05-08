//! Seam tests for `integrate` against real git tempdirs. Covers the
//! happy path, double-integration rejection, EmptyBranch, the four
//! crash-recovery paths, and ConflictRetryable. Structural conflict
//! lives in its own test file because it requires `allow_multi_bundle
//! = true`.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, Work};
use integrator::{IntegrationError, IntegratorConfig, IntegratorDeps, integrate};
use store::Store;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Initializes a git repo with one initial commit, creates an
/// integration branch `loopr/plan-<plan_id>`, and returns the repo's
/// path plus the initial commit SHA.
fn init_repo_with_integration_branch(plan: &Plan) -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    // Keep the Store's .loopr/taskstore/*.jsonl files out of every
    // commit; otherwise switching branches would remove them from the
    // working tree and invalidate the Store's file handles.
    std::fs::write(path.join(".gitignore"), ".loopr/\n").unwrap();
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);

    let integ = format!("loopr/plan-{}", plan.id);
    git(&path, &["branch", &integ]);

    (dir, path, sha)
}

/// Creates a Bundle branch off the integration branch with a single
/// commit. Returns the head commit SHA and the branch name.
fn create_bundle_branch(repo: &Path, plan: &Plan, bundle_branch: &str, file: &str, contents: &str) -> String {
    let integ = format!("loopr/plan-{}", plan.id);
    git(repo, &["checkout", "-q", "-b", bundle_branch, &integ]);
    std::fs::write(repo.join(file), contents).unwrap();
    git(repo, &["add", "-A"]);
    git(
        repo,
        &[
            "commit",
            "-q",
            "-m",
            &format!("bundle commit for {bundle_branch}"),
            "--no-gpg-sign",
        ],
    );
    let sha = git_capture(repo, &["rev-parse", "HEAD"]);
    // Leave us checked out on main so the integrator's own checkout
    // of the integration branch is exercised.
    git(repo, &["checkout", "-q", "main"]);
    sha
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

async fn setup(plan: &Plan) -> (TempDir, Store, PathBuf, String) {
    let (dir, path, sha) = init_repo_with_integration_branch(plan);
    let store = Store::open(&path).await.unwrap();
    (dir, store, path, sha)
}

fn sample_work(plan: &Plan) -> Work {
    let mut w = Work::new(plan.id.clone(), "ship feature X".to_string());
    w.acceptance_criteria = AcceptanceCriteria(vec!["feature X works".to_string()]);
    w
}

/// Builds a Bundle at `Accepted` pre-persisted via `bundles().create`
/// and transitioned through Triaged -> Reviewed -> Accepted via OCC
/// updates. Returns the post-`Accepted` Bundle value fetched from the
/// store.
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

    let _id = store.bundles().create(b.clone()).await.unwrap();

    // Drive Proposed -> Triaged -> Reviewed -> Accepted via OCC.
    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();

    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();

    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Accepted, Role::Reactor).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();

    store.bundles().get(&b.id).await.unwrap()
}

fn deps_for<'a>(store: &'a Store, target: &Path) -> IntegratorDeps<&'a Store, &'a Store, &'a Store> {
    IntegratorDeps {
        bundle_sink: store,
        works: store,
        ticks: store,
        config: IntegratorConfig::default(),
        target: target.to_path_buf(),
        git_lock: Arc::new(AsyncMutex::new(())),
    }
}

// ---------------------------------------------------------------------------
// Happy path + double-integration rejection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_merges_bundle_and_writes_tick() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;

    let deps = deps_for(&store, &repo);
    let tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    // Tick shape.
    assert_eq!(tick.plan_id, plan.id);
    assert_eq!(tick.branch, format!("loopr/plan-{}", plan.id));
    assert_eq!(tick.bundles, vec![bundle.id.clone()]);
    assert_eq!(tick.merge_commits.len(), 1);

    // Integration branch HEAD matches tick.sha.
    git(&repo, &["checkout", "-q", &format!("loopr/plan-{}", plan.id)]);
    let head = git_capture(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(head, tick.sha);

    // Merge commit has two parents (pre_merge + bundle_head).
    let parents = git_capture(&repo, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "merge commit must have 2 parents: '{parents}'"
    );

    // Bundle on disk is Merged.
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Merged);

    store.close().await.unwrap();
}

#[tokio::test]
async fn double_integration_rejected_with_bundle_not_accepted_merged() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;

    let deps = deps_for(&store, &repo);
    let _tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    // Re-fetch: Bundle is now Merged.
    let merged = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(merged.status, BundleStatus::Merged);

    // Second call with the (now Merged) Bundle.
    let result = integrate(&[merged], &plan, &deps).await;
    match result {
        Err(IntegrationError::BundleNotAccepted {
            current: BundleStatus::Merged,
            ..
        }) => {}
        other => panic!("expected BundleNotAccepted(Merged), got {other:?}"),
    }

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Precondition failures (reached after pre-flight shape + status)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn integration_branch_missing_returns_typed_error() {
    let plan = Plan::new("ship".to_string());
    let (dir, _path, _base) = {
        // Init a repo WITHOUT creating the integration branch.
        let dir = TempDir::new().unwrap();
        let p = dir.path().to_path_buf();
        git(&p, &["init", "-q", "-b", "main"]);
        git(&p, &["config", "user.email", "test@example.com"]);
        git(&p, &["config", "user.name", "Test"]);
        git(&p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join(".gitignore"), ".loopr/\n").unwrap();
        std::fs::write(p.join("README.md"), "initial\n").unwrap();
        git(&p, &["add", "-A"]);
        git(&p, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
        let s = git_capture(&p, &["rev-parse", "HEAD"]);
        (dir, p, s)
    };
    let repo = dir.path().to_path_buf();
    let store = Store::open(&repo).await.unwrap();
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    // Bundle branch doesn't exist either; we'll never reach that check
    // because branch-missing is the first branch check.
    let mut b = Bundle::new(work.id.clone(), "loopr/wk-fake".to_string(), vec![]);
    b.head_commit = Some("deadbeef".to_string());
    store.bundles().create(b.clone()).await.unwrap();
    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();
    let current = store.bundles().get(&b.id).await.unwrap();
    let mut next = current.clone();
    next.transition(BundleStatus::Accepted, Role::Reactor).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();
    let accepted = store.bundles().get(&b.id).await.unwrap();

    let deps = deps_for(&store, &repo);
    let result = integrate(&[accepted], &plan, &deps).await;
    match result {
        Err(IntegrationError::IntegrationBranchMissing { branch }) => {
            assert_eq!(branch, format!("loopr/plan-{}", plan.id));
        }
        other => panic!("expected IntegrationBranchMissing, got {other:?}"),
    }

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// EmptyBranch (bundle branch has no commits beyond merge base)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_branch_detected_before_merge_attempt() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, sha) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    // Create a bundle branch pointing at the integration branch's HEAD
    // with no extra commits.
    let branch = format!("loopr/wk-{}", work.id);
    git(&repo, &["branch", &branch, &format!("loopr/plan-{}", plan.id)]);

    let bundle = persist_accepted_bundle(&store, &work, &branch, &sha, vec![]).await;

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::EmptyBranch { branch: b, .. }) => assert_eq!(b, branch),
        other => panic!("expected EmptyBranch, got {other:?}"),
    }

    // Bundle transitioned to IntegrationFailed (fail_all).
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// ConflictRetryable (merge fails, no peer overlap)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_conflict_rolls_back_and_marks_integration_failed() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, sha) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    // Pre-populate the integration branch with a change to conflict.rs.
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    std::fs::write(repo.join("conflict.rs"), "fn integration() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "integ-side change", "--no-gpg-sign"]);
    let integ_head = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Bundle branch starts from sha (before the integ-side change)
    // and writes the SAME file with DIFFERENT content -> textual conflict.
    let branch = format!("loopr/wk-{}", work.id);
    git(&repo, &["checkout", "-q", "-b", &branch, &sha]);
    std::fs::write(repo.join("conflict.rs"), "fn bundle_side() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bundle-side change", "--no-gpg-sign"]);
    let head = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Single-Bundle slice: no peers, so classification is Retryable.
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["conflict.rs".to_string()]).await;

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::ConflictRetryable { branch: b, .. }) => {
            assert_eq!(b, branch);
        }
        other => panic!("expected ConflictRetryable, got {other:?}"),
    }

    // Integration branch rolled back to its pre-merge HEAD.
    let post = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(post, integ_head, "integration branch must be rolled back");

    // Bundle is IntegrationFailed.
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Crash-recovery paths (Integrating as re-entry point)
// ---------------------------------------------------------------------------
//
// Each recovery case is set up by (1) manually transitioning the
// Bundle from Accepted -> Integrating (simulating a prior call's
// Phase 2 write), (2) manipulating git state to match the scenario,
// (3) calling integrate again with the now-`Integrating` Bundle.

async fn transition_bundle_via_store(store: &Store, bundle: &Bundle, target: BundleStatus, role: Role) -> Bundle {
    let current = store.bundles().get(&bundle.id).await.unwrap();
    let mut next = current.clone();
    next.transition(target, role).unwrap();
    store.bundles().update(next.clone(), current.updated_at).await.unwrap();
    store.bundles().get(&bundle.id).await.unwrap()
}

#[tokio::test]
async fn crash_recovery_a_merge_landed_tick_landed_merged_write_lost() {
    // Prior call: merged + wrote Tick, died before Merged transition.
    // Re-entry: integrate must see ancestry=true AND DuplicateTick,
    // resolve the existing Tick, transition Bundle to Merged.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");

    // Simulate prior call: merge happened on the integration branch.
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    let pre_merge = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch,
            "-m",
            &format!("Merge bundle branch {branch}"),
            "--no-gpg-sign",
        ],
    );
    let sha = git_capture(&repo, &["rev-parse", "HEAD"]);
    assert_ne!(pre_merge, sha);

    // Persist a Bundle, drive it to Integrating (simulating the prior
    // Phase 2 Accepted -> Integrating write), then invoke integrate.
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;
    assert_eq!(bundle.status, BundleStatus::Integrating);

    // And persist a Tick matching (plan_id, [bundle.id]) — simulating
    // the prior call's Phase 3 Tick write.
    let pre_existing_tick = domain::Tick::new(
        plan.id.clone(),
        vec![bundle.id.clone()],
        integ.clone(),
        sha.clone(),
        vec![sha.clone()],
    );
    let _ = store.ticks().create(pre_existing_tick.clone()).await.unwrap();

    // Check out main so integrate does its own checkout.
    git(&repo, &["checkout", "-q", "main"]);

    // integrate sees: Integrating bundle + ancestry=true + Tick already exists.
    // Expected: Integrator resolves the existing Tick via
    // TickSink::get, completes the Merged transition, returns
    // Ok(existing_tick). This honors the design's Crash-recovery
    // invariant (a): "Phase 3 transitions the Bundle to Merged and
    // the call succeeds."
    let deps = deps_for(&store, &repo);
    let tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();
    assert_eq!(tick.id, pre_existing_tick.id, "must return the pre-existing Tick");

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(
        final_bundle.status,
        BundleStatus::Merged,
        "Bundle must be transitioned to Merged after DuplicateTick resolution"
    );

    store.close().await.unwrap();
}

#[tokio::test]
async fn crash_recovery_b_merge_landed_tick_never_landed() {
    // Prior call: merge completed, died before Tick write.
    // Re-entry: integrate sees ancestry=true, no existing Tick,
    // writes Tick + transitions Merged.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");

    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch,
            "-m",
            &format!("Merge bundle branch {branch}"),
            "--no-gpg-sign",
        ],
    );
    git(&repo, &["checkout", "-q", "main"]);

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = deps_for(&store, &repo);
    let tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    // Tick was written with one AdoptedExisting outcome.
    assert_eq!(tick.bundles, vec![bundle.id.clone()]);

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Merged);

    store.close().await.unwrap();
}

#[tokio::test]
async fn crash_recovery_c_merge_never_landed_falls_through_to_normal_merge() {
    // Prior call: transitioned Accepted -> Integrating, died BEFORE
    // the merge ran. Re-entry: integrate sees ancestry=false, falls
    // through to the normal merge path.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");

    // NO merge; integration branch is untouched.

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = deps_for(&store, &repo);
    let tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    assert_eq!(tick.bundles, vec![bundle.id.clone()]);
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Merged);

    // Integration branch actually advanced.
    git(&repo, &["checkout", "-q", &format!("loopr/plan-{}", plan.id)]);
    let post = git_capture(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(post, tick.sha);

    store.close().await.unwrap();
}

#[tokio::test]
async fn crash_recovery_d_merge_never_landed_merge_fails_produces_conflict() {
    // Integrating bundle, ancestry=false, but the merge itself fails.
    // Crash-recovery does not shield a legitimately-failing merge.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, sha) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    // Pre-populate the integration branch with conflict.rs.
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    std::fs::write(repo.join("conflict.rs"), "fn integ_side() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "integ-side change", "--no-gpg-sign"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Bundle branch from sha with a DIFFERENT conflict.rs.
    let branch = format!("loopr/wk-{}", work.id);
    git(&repo, &["checkout", "-q", "-b", &branch, &sha]);
    std::fs::write(repo.join("conflict.rs"), "fn bundle_side() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bundle-side change", "--no-gpg-sign"]);
    let head = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["conflict.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::ConflictRetryable { .. }) => {}
        other => panic!("expected ConflictRetryable, got {other:?}"),
    }

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Validation failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_failure_rolls_back_and_marks_bundle_integration_failed() {
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feature.rs", "pub fn f() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feature.rs".to_string()]).await;

    // Capture integration branch SHA before the call.
    let integ = format!("loopr/plan-{}", plan.id);
    let pre_sha = git_capture(&repo, &["rev-parse", &integ]);

    let mut config = IntegratorConfig::default();
    config.validation_commands = vec!["false".to_string()]; // always fails
    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config,
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };

    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::ValidationFailed { ref command, exit_code, .. }) => {
            assert_eq!(command, "false");
            assert_eq!(exit_code, Some(1));
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }

    // Integration branch must be back at pre_sha (merge rolled back).
    let post_sha = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(pre_sha, post_sha, "integration branch should be reset to pre-merge SHA");

    // Bundle must be IntegrationFailed.
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);

    // No Tick must have been written.
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert!(ticks.is_empty(), "no Tick should exist after validation failure");

    store.close().await.unwrap();
}
