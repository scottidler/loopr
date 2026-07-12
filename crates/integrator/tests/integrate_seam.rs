//! Seam tests for `integrate` against real git tempdirs. Covers the
//! happy path, idempotent double-integration, EmptyBranch, the
//! crash-recovery paths (including the stale-merge-abort heal, the
//! trivially-ancestral false-adopt guard, and the multi-bundle
//! partial-crash re-entry), ConflictRetryable, the dirty-tree guard,
//! merge-commit determinism, and bundle-branch cleanup. Structural
//! conflict lives in its own test file because it requires
//! `allow_multi_bundle = true`.

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
    w.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["feature X works".to_string()]);
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
async fn double_integration_is_idempotent_returns_existing_tick() {
    // A re-submitted Merged Bundle is no longer rejected with
    // BundleNotAccepted (finding 9: preflight tolerates Merged so a
    // partial-crash re-entry can pass the full slice). Re-integrating a
    // fully-Merged Bundle is now idempotent: the merge is adopted, the
    // Tick's bundle-set matches, DuplicateTick resolves the existing
    // Tick, and the SAME Tick is returned with the Bundle still Merged.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;

    let deps = deps_for(&store, &repo);
    let first = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    // Re-fetch: Bundle is now Merged.
    let merged = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(merged.status, BundleStatus::Merged);

    // Second call with the (now Merged) Bundle: idempotent.
    let second = integrate(&[merged], &plan, &deps).await.unwrap();
    assert_eq!(second.id, first.id, "re-integration must return the existing Tick");

    // Still exactly one Tick on disk; Bundle still Merged.
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert_eq!(ticks.len(), 1, "no second Tick must be written");
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Merged);

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
    )
    .unwrap();
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

#[tokio::test]
async fn crash_recovery_trivially_ancestral_head_not_falsely_adopted() {
    // is_ancestor alone false-adopts: an Integrating Bundle whose
    // head_commit is trivially ancestral (here: an empty branch pointing
    // at the integration base) reads as already-merged, and the old
    // merge_commit_sha_for grabbed a DIFFERENT bundle's merge commit,
    // wrongly publishing it into a Tick. The second-parent verification
    // in find_adopting_merge rejects the false match and falls through
    // to the normal merge path, where EmptyBranch correctly reports it.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let integ = format!("loopr/plan-{}", plan.id);

    // Prior tick: a DIFFERENT bundle (A) merged onto the integration
    // branch, so HEAD now carries a merge commit whose second parent is
    // A's tip (not our base).
    let branch_a = "loopr/wk-aaaaa";
    git(&repo, &["checkout", "-q", "-b", branch_a, &integ]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bundle A work", "--no-gpg-sign"]);
    git(&repo, &["checkout", "-q", &integ]);
    git(
        &repo,
        &["merge", "--no-ff", branch_a, "-m", "Merge bundle A", "--no-gpg-sign"],
    );
    git(&repo, &["checkout", "-q", "main"]);

    // Bundle B (re-entry): an EMPTY branch pointing at the original base,
    // which is trivially an ancestor of the post-A integration HEAD.
    let branch_b = format!("loopr/wk-{}", work.id);
    git(&repo, &["branch", &branch_b, &base]);
    let bundle = persist_accepted_bundle(&store, &work, &branch_b, &base, vec![]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::EmptyBranch { branch: b, .. }) => assert_eq!(b, branch_b),
        other => panic!("expected EmptyBranch (no false-adopt), got {other:?}"),
    }

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::IntegrationFailed);
    // Critically: no Tick was written - B was not falsely adopted as Merged.
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert!(
        ticks.is_empty(),
        "trivially-ancestral empty branch must not be falsely adopted into a Tick"
    );

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Entry-sequence guards: dirty tree + crash-recovery merge-abort
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dirty_working_tree_refused_in_per_plan_mode() {
    // Per-Plan-branch mode (the default) must refuse a dirty tree, not
    // silently carry the dirt across the checkout. Previously only the
    // no-branch override checked.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;

    // Dirty the working tree with an untracked, non-ignored file.
    std::fs::write(repo.join("uncommitted.txt"), "operator work in progress\n").unwrap();

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::DirtyWorkingTree { branch: b }) => {
            assert_eq!(b, format!("loopr/plan-{}", plan.id));
        }
        other => panic!("expected DirtyWorkingTree, got {other:?}"),
    }

    // No mutation: the Bundle is untouched (still Accepted) and no Tick exists.
    let unchanged = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(unchanged.status, BundleStatus::Accepted);
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert!(ticks.is_empty());

    store.close().await.unwrap();
}

#[tokio::test]
async fn crash_recovery_aborts_stale_merge_before_reentry() {
    // A daemon crash mid-merge leaves a conflicted index + MERGE_HEAD
    // on disk. Without the entry-time merge-abort, re-entry's `git
    // checkout` fails with "you need to resolve your current index
    // first" and wedges integration permanently. With it, the stale
    // merge is aborted and integration proceeds to a clean
    // classification (here: the same conflict re-surfaces as
    // ConflictRetryable - the point is it is NOT a checkout wedge).
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, sha) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    // Integration branch gets a conflicting change to conflict.rs.
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    std::fs::write(repo.join("conflict.rs"), "fn integ_side() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "integ-side change", "--no-gpg-sign"]);

    // Bundle branch from the base sha with a DIFFERENT conflict.rs.
    let branch = format!("loopr/wk-{}", work.id);
    git(&repo, &["checkout", "-q", "-b", &branch, &sha]);
    std::fs::write(repo.join("conflict.rs"), "fn bundle_side() {}\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "bundle-side change", "--no-gpg-sign"]);
    let head = git_capture(&repo, &["rev-parse", "HEAD"]);

    // Simulate the prior crash: start the merge ON the integration
    // branch and leave it conflicted (MERGE_HEAD + conflicted index),
    // never aborting. HEAD parks on the integration branch.
    git(&repo, &["checkout", "-q", &integ]);
    let merge_out = StdCommand::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["merge", "--no-ff", &branch, "-m", "crashed merge", "--no-gpg-sign"])
        .output()
        .unwrap();
    assert!(!merge_out.status.success(), "merge must conflict to leave MERGE_HEAD");
    // Confirm the wedge state exists.
    let merge_head = StdCommand::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .output()
        .unwrap();
    assert!(
        merge_head.status.success(),
        "MERGE_HEAD must be present (mid-merge wedge)"
    );

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["conflict.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = deps_for(&store, &repo);
    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::ConflictRetryable { .. }) => {
            // Reached the merge classification: the stale merge was
            // aborted and checkout succeeded.
        }
        other => panic!("expected ConflictRetryable (healed wedge), got {other:?}"),
    }

    // No MERGE_HEAD should linger after integrate's rollback.
    let merge_head_after = StdCommand::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .output()
        .unwrap();
    assert!(
        !merge_head_after.status.success(),
        "no MERGE_HEAD should linger after integrate"
    );

    store.close().await.unwrap();
}

#[tokio::test]
async fn validation_failure_after_adopt_is_non_terminal_and_preserves_merge() {
    // Crash-recovery: a prior call merged the bundle (AdoptedExisting on
    // re-entry), then this re-entry's validation fails. The merge
    // commits are durable on the integration branch and pre_merge was
    // captured AFTER them, so the old code's reset_hard(pre_merge) +
    // fail_all would mark the Bundle IntegrationFailed while its commits
    // stay merged - divergence. The fix: skip rollback, return the
    // distinct ValidationFailedAfterAdopt, leave the Bundle Integrating.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");

    // Simulate the prior call: merge already landed on the integration branch.
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
    let merged_head = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;
    let bundle = transition_bundle_via_store(&store, &bundle, BundleStatus::Integrating, Role::Integrator).await;

    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config: IntegratorConfig {
            validation_commands: vec!["false".to_string()], // always fails
            ..IntegratorConfig::default()
        },
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };

    let result = integrate(std::slice::from_ref(&bundle), &plan, &deps).await;
    match result {
        Err(IntegrationError::ValidationFailedAfterAdopt {
            ref command, exit_code, ..
        }) => {
            assert_eq!(command, "false");
            assert_eq!(exit_code, Some(1));
        }
        other => panic!("expected ValidationFailedAfterAdopt, got {other:?}"),
    }

    // The merge commits are PRESERVED (not rolled back).
    let post = git_capture(&repo, &["rev-parse", &integ]);
    assert_eq!(post, merged_head, "adopted merge must NOT be rolled back");

    // The Bundle stays Integrating (NOT terminally IntegrationFailed).
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(
        final_bundle.status,
        BundleStatus::Integrating,
        "adopted-then-validation-failed Bundle must stay Integrating, not IntegrationFailed"
    );

    store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Determinism, branch cleanup, multi-bundle partial-crash re-entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn merge_commit_dates_and_identity_are_pinned_for_determinism() {
    // The merge commit's author/committer identity and dates are pinned
    // (identity -> a fixed loopr identity; dates -> the bundle head's
    // committer date) so "same bundles + same base = same Tick SHA".
    // Verifying the pinned inputs is the mechanism; SHA-equality across
    // runs is its consequence.
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

    // Identity is the fixed loopr identity, not the ambient git config.
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%cn", &tick.sha]),
        "loopr-integrator"
    );
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%ce", &tick.sha]),
        "integrator@loopr"
    );
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%an", &tick.sha]),
        "loopr-integrator"
    );
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%ae", &tick.sha]),
        "integrator@loopr"
    );

    // Dates are pinned to the bundle head's committer date.
    let bundle_date = git_capture(&repo, &["log", "-1", "--format=%cI", &head]);
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%cI", &tick.sha]),
        bundle_date,
        "merge committer date pinned to bundle head"
    );
    assert_eq!(
        git_capture(&repo, &["log", "-1", "--format=%aI", &tick.sha]),
        bundle_date,
        "merge author date pinned to bundle head"
    );

    store.close().await.unwrap();
}

#[tokio::test]
async fn merged_bundle_branch_is_deleted_after_tick() {
    // The integrator deletes the per-attempt bundle branch after the
    // Tick publishes (the commits live on the integration branch now).
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work = sample_work(&plan);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    // Branch exists before integration.
    let before = StdCommand::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .unwrap();
    assert!(before.status.success(), "bundle branch should exist before integrate");

    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;
    let deps = deps_for(&store, &repo);
    integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();

    // Branch is gone after the Tick.
    let after = StdCommand::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--verify", "--quiet", &branch])
        .output()
        .unwrap();
    assert!(!after.status.success(), "bundle branch must be deleted after the Tick");

    store.close().await.unwrap();
}

#[tokio::test]
async fn multi_bundle_partial_crash_reentry_finishes_remaining_merged() {
    // A prior multi-Bundle integrate merged BOTH bundles and wrote the
    // Tick, then crashed PART-way through the Phase-4 Merged loop: bundle1
    // got Merged, bundle2 is still Integrating. The full-slice re-entry
    // must tolerate the Merged bundle1, adopt both merges, resolve the
    // existing Tick via DuplicateTick, and finish bundle2 -> Merged.
    let plan = Plan::new("ship".to_string());
    let (_dir, store, repo, _base) = setup(&plan).await;
    store.plans().create(plan.clone()).await.unwrap();
    let work1 = sample_work(&plan);
    let work2 = sample_work(&plan);
    store.works().create(work1.clone()).await.unwrap();
    store.works().create(work2.clone()).await.unwrap();

    let branch1 = format!("loopr/wk-{}", work1.id);
    let branch2 = format!("loopr/wk-{}", work2.id);
    let head1 = create_bundle_branch(&repo, &plan, &branch1, "feat1.rs", "fn one() {}\n");
    let head2 = create_bundle_branch(&repo, &plan, &branch2, "feat2.rs", "fn two() {}\n");

    // Simulate the prior call: both merges already landed on integ.
    let integ = format!("loopr/plan-{}", plan.id);
    git(&repo, &["checkout", "-q", &integ]);
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch1,
            "-m",
            &format!("Merge bundle branch {branch1}"),
            "--no-gpg-sign",
        ],
    );
    let merge1 = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "merge",
            "--no-ff",
            &branch2,
            "-m",
            &format!("Merge bundle branch {branch2}"),
            "--no-gpg-sign",
        ],
    );
    let merge2 = git_capture(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    // Persist both bundles; bundle1 fully Merged (prior Phase-4 wrote it),
    // bundle2 left Integrating (crash before it was written).
    let bundle1 = persist_accepted_bundle(&store, &work1, &branch1, &head1, vec!["feat1.rs".to_string()]).await;
    let bundle1 = transition_bundle_via_store(&store, &bundle1, BundleStatus::Integrating, Role::Integrator).await;
    let bundle1 = transition_bundle_via_store(&store, &bundle1, BundleStatus::Merged, Role::Integrator).await;
    assert_eq!(bundle1.status, BundleStatus::Merged);

    let bundle2 = persist_accepted_bundle(&store, &work2, &branch2, &head2, vec!["feat2.rs".to_string()]).await;
    let bundle2 = transition_bundle_via_store(&store, &bundle2, BundleStatus::Integrating, Role::Integrator).await;
    assert_eq!(bundle2.status, BundleStatus::Integrating);

    // Prior call's Tick covering BOTH bundles (the set key the re-entry
    // must reproduce to hit DuplicateTick).
    let prior_tick = domain::Tick::new(
        plan.id.clone(),
        vec![bundle1.id.clone(), bundle2.id.clone()],
        integ.clone(),
        merge2.clone(),
        vec![merge1.clone(), merge2.clone()],
    )
    .unwrap();
    let _ = store.ticks().create(prior_tick.clone()).await.unwrap();

    // Re-enter with the FULL slice and allow_multi_bundle = true.
    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config: IntegratorConfig {
            allow_multi_bundle: true,
            ..IntegratorConfig::default()
        },
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };
    let tick = integrate(&[bundle1.clone(), bundle2.clone()], &plan, &deps)
        .await
        .unwrap();

    // Adopted the existing Tick (no second Tick written).
    assert_eq!(
        tick.id, prior_tick.id,
        "must resolve the existing Tick via DuplicateTick"
    );
    let ticks = store.ticks().list_by_plan_id(&plan.id).await.unwrap();
    assert_eq!(ticks.len(), 1, "no second Tick must be written");

    // Both bundles end Merged.
    assert_eq!(
        store.bundles().get(&bundle1.id).await.unwrap().status,
        BundleStatus::Merged
    );
    assert_eq!(
        store.bundles().get(&bundle2.id).await.unwrap().status,
        BundleStatus::Merged
    );

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

    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config: IntegratorConfig {
            validation_commands: vec!["false".to_string()], // always fails
            ..IntegratorConfig::default()
        },
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };

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
