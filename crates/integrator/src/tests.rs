//! Unit tests for pre-flight + classify_conflict. Git-interaction
//! tests live in Phase 4 seam tests (`crates/integrator/tests/`) so
//! they can use real `tokio::process::Command` + tempdir repos, the
//! same convention the Implementer/Reviewer crates follow.

#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, Tick, Work, WorkId};
use store::{BundleUpdateError, BundleUpdateSink, StoreError};

use crate::classify::{ConflictKind, classify_conflict};
use crate::{IntegrationError, IntegratorConfig, IntegratorDeps, TickSink, WorkLookup};

// ---------------------------------------------------------------------------
// IntegrationError Display coverage
// ---------------------------------------------------------------------------

#[test]
fn integrator_config_defaults() {
    let cfg = IntegratorConfig::default();
    assert_eq!(cfg.git_timeout, Duration::from_secs(60));
    assert!(!cfg.allow_multi_bundle);
}

#[test]
fn integration_error_display_covers_every_variant() {
    let cases: Vec<(String, &str)> = vec![
        (IntegrationError::NoBundles.to_string(), "no bundles"),
        (IntegrationError::MultiBundleNotSupported { count: 3 }.to_string(), "3"),
        (
            IntegrationError::BundleNotAccepted {
                bundle_id: "bd-abc".to_string(),
                current: BundleStatus::Proposed,
            }
            .to_string(),
            "bd-abc",
        ),
        (
            IntegrationError::WorkNotFound {
                bundle_id: "bd-abc".to_string(),
                work_id: "wk-xyz".to_string(),
            }
            .to_string(),
            "wk-xyz",
        ),
        (
            IntegrationError::PlanBundleMismatch {
                bundle_id: "bd-abc".to_string(),
                work_plan_id: "pl-1".to_string(),
                plan_id: "pl-2".to_string(),
            }
            .to_string(),
            "pl-1",
        ),
        (
            IntegrationError::IntegrationBranchMissing {
                branch: "loopr/plan-xxx".to_string(),
            }
            .to_string(),
            "loopr/plan-xxx",
        ),
        (
            IntegrationError::EmptyBranch {
                bundle_id: "bd-abc".to_string(),
                branch: "loopr/wk-xyz".to_string(),
            }
            .to_string(),
            "loopr/wk-xyz",
        ),
        (
            IntegrationError::ConflictStructural {
                bundle_id: "bd-abc".to_string(),
                files: vec!["README.md".to_string()],
                peer_bundle_ids: vec!["bd-peer".to_string()],
            }
            .to_string(),
            "README.md",
        ),
        (
            IntegrationError::ConflictRetryable {
                bundle_id: "bd-abc".to_string(),
                branch: "loopr/wk-xyz".to_string(),
                stderr: "merge conflict".to_string(),
            }
            .to_string(),
            "merge conflict",
        ),
        (
            IntegrationError::Git("rev-parse failed".to_string()).to_string(),
            "rev-parse",
        ),
        (
            IntegrationError::Transition("Accepted -> Merged not allowed".to_string()).to_string(),
            "Accepted",
        ),
    ];
    for (msg, needle) in cases {
        assert!(
            msg.contains(needle),
            "Display for IntegrationError variant missing needle '{needle}': {msg}"
        );
    }
}

// ---------------------------------------------------------------------------
// classify_conflict (pure)
// ---------------------------------------------------------------------------

fn bundle_with_paths(paths: Vec<&str>) -> Bundle {
    let mut b = Bundle::new(WorkId::new(), "loopr/wk-test".to_string(), vec!["claim".to_string()]);
    b.paths = paths.into_iter().map(str::to_string).collect();
    b
}

#[test]
fn classify_no_overlap_is_retryable() {
    let a = bundle_with_paths(vec!["src/a.rs"]);
    let b = bundle_with_paths(vec!["src/b.rs"]);
    let kind = classify_conflict(&a, &[a.clone(), b]);
    assert_eq!(kind, ConflictKind::Retryable);
}

#[test]
fn classify_single_bundle_slice_is_retryable() {
    let a = bundle_with_paths(vec!["src/a.rs"]);
    let kind = classify_conflict(&a, std::slice::from_ref(&a));
    assert_eq!(
        kind,
        ConflictKind::Retryable,
        "single Bundle has no peers to overlap with"
    );
}

#[test]
fn classify_full_overlap_is_structural() {
    let a = bundle_with_paths(vec!["src/a.rs", "src/b.rs"]);
    let b = bundle_with_paths(vec!["src/a.rs", "src/b.rs"]);
    let peer_id = b.id.as_ref().to_string();
    match classify_conflict(&a, &[a.clone(), b]) {
        ConflictKind::Structural { files, peer_bundle_ids } => {
            assert_eq!(files.len(), 2);
            assert!(files.contains(&"src/a.rs".to_string()));
            assert!(files.contains(&"src/b.rs".to_string()));
            assert_eq!(peer_bundle_ids, vec![peer_id]);
        }
        other => panic!("expected Structural, got {other:?}"),
    }
}

#[test]
fn classify_partial_overlap_structural_with_intersection() {
    let a = bundle_with_paths(vec!["src/a.rs", "src/b.rs"]);
    let b = bundle_with_paths(vec!["src/b.rs", "src/c.rs"]);
    match classify_conflict(&a, &[a.clone(), b]) {
        ConflictKind::Structural { files, peer_bundle_ids } => {
            assert_eq!(files, vec!["src/b.rs".to_string()]);
            assert_eq!(peer_bundle_ids.len(), 1);
        }
        other => panic!("expected Structural, got {other:?}"),
    }
}

#[test]
fn classify_self_is_never_peer() {
    let a = bundle_with_paths(vec!["src/a.rs"]);
    // Same Bundle twice in the slice: classifier skips self-match.
    let kind = classify_conflict(&a, &[a.clone(), a.clone()]);
    assert_eq!(kind, ConflictKind::Retryable);
}

// ---------------------------------------------------------------------------
// Pre-flight tests with fake stores (no git calls reached)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeBundleSink {
    writes: Mutex<Vec<Bundle>>,
}

impl BundleUpdateSink for FakeBundleSink {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        _expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a {
        async move {
            self.writes.lock().unwrap().push(bundle);
            Ok(())
        }
    }
}

struct FakeWorks {
    by_id: std::collections::HashMap<String, Work>,
}

impl FakeWorks {
    fn new() -> Self {
        Self {
            by_id: std::collections::HashMap::new(),
        }
    }
    fn with(mut self, work: Work) -> Self {
        self.by_id.insert(work.id.as_ref().to_string(), work);
        self
    }
}

impl WorkLookup for FakeWorks {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(&'a self, work_id: &'a str) -> impl Future<Output = Result<Option<Work>, StoreError>> + Send + 'a {
        async move { Ok(self.by_id.get(work_id).cloned()) }
    }
}

struct FakeTicks;

impl TickSink for FakeTicks {
    #[allow(clippy::manual_async_fn)]
    fn get<'a>(
        &'a self,
        _tick_id: &'a domain::TickId,
    ) -> impl Future<Output = Result<Option<Tick>, StoreError>> + Send + 'a {
        async move { Ok(None) }
    }
    #[allow(clippy::manual_async_fn)]
    fn create<'a>(&'a self, tick: Tick) -> impl Future<Output = Result<Tick, StoreError>> + Send + 'a {
        async move { Ok(tick) }
    }
}

fn fake_deps() -> IntegratorDeps<FakeBundleSink, FakeWorks, FakeTicks> {
    IntegratorDeps {
        bundle_sink: FakeBundleSink::default(),
        works: FakeWorks::new(),
        ticks: FakeTicks,
        config: IntegratorConfig::default(),
        target: PathBuf::from("/tmp/unit-test-nonexistent"),
        git_lock: Arc::new(AsyncMutex::new(())),
    }
}

fn accepted_bundle(work_id: WorkId) -> Bundle {
    let mut b = Bundle::new(work_id, "loopr/wk-test".to_string(), vec!["claim".to_string()]);
    b.head_commit = Some("abc123".to_string());
    b.paths = vec!["src/a.rs".to_string()];
    b.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    b.transition(BundleStatus::Accepted, Role::Reactor).unwrap();
    b
}

#[tokio::test]
async fn preflight_empty_slice_returns_no_bundles() {
    let deps = fake_deps();
    let plan = Plan::new("x".to_string());
    let result = crate::integrate(&[], &plan, &deps).await;
    assert!(matches!(result, Err(IntegrationError::NoBundles)));
}

#[tokio::test]
async fn preflight_two_bundles_without_allow_multi_rejected() {
    let mut deps = fake_deps();
    deps.config.allow_multi_bundle = false;
    let plan = Plan::new("x".to_string());
    let b1 = accepted_bundle(WorkId::new());
    let b2 = accepted_bundle(WorkId::new());
    let result = crate::integrate(&[b1, b2], &plan, &deps).await;
    assert!(matches!(
        result,
        Err(IntegrationError::MultiBundleNotSupported { count: 2 })
    ));
}

#[tokio::test]
async fn preflight_bundle_in_proposed_rejected() {
    let deps = fake_deps();
    let plan = Plan::new("x".to_string());
    let b = Bundle::new(WorkId::new(), "loopr/wk".to_string(), vec!["c".to_string()]);
    assert_eq!(b.status, BundleStatus::Proposed);
    let result = crate::integrate(&[b], &plan, &deps).await;
    match result {
        Err(IntegrationError::BundleNotAccepted {
            current: BundleStatus::Proposed,
            ..
        }) => {}
        other => panic!("expected BundleNotAccepted(Proposed), got {other:?}"),
    }
}

#[tokio::test]
async fn preflight_bundle_in_reviewed_rejected() {
    let deps = fake_deps();
    let plan = Plan::new("x".to_string());
    let mut b = Bundle::new(WorkId::new(), "loopr/wk".to_string(), vec![]);
    b.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    b.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();
    let result = crate::integrate(&[b], &plan, &deps).await;
    match result {
        Err(IntegrationError::BundleNotAccepted {
            current: BundleStatus::Reviewed,
            ..
        }) => {}
        other => panic!("expected BundleNotAccepted(Reviewed), got {other:?}"),
    }
}

#[tokio::test]
async fn preflight_bundle_in_integrating_accepted_as_reentry() {
    // Integrating is NOT rejected: it's a crash-recovery re-entry.
    // This test confirms pre-flight passes; the call will progress to
    // git which will fail (no real repo at /tmp/unit-test-nonexistent),
    // producing a Git error rather than BundleNotAccepted. The
    // important signal is "pre-flight did NOT reject Integrating."
    let deps = fake_deps();
    let mut plan = Plan::new("x".to_string());
    let work = Work::new(plan.id.clone(), "w".to_string());
    // Override FakeWorks with this Work so pre-flight passes through.
    let deps = IntegratorDeps {
        works: FakeWorks::new().with(work.clone()),
        ..deps
    };
    plan.id = work.parent_id.clone();
    let mut b = accepted_bundle(work.id.clone());
    b.transition(BundleStatus::Integrating, Role::Integrator).unwrap();
    let result = crate::integrate(&[b], &plan, &deps).await;
    // Must NOT be BundleNotAccepted.
    match result {
        Err(IntegrationError::BundleNotAccepted { .. }) => {
            panic!("Integrating must NOT be rejected; it is a crash-recovery re-entry point");
        }
        _ => {
            // Any other error (Git, etc.) is expected - the target dir
            // doesn't exist. The test passes if pre-flight lets us
            // through.
        }
    }
}

#[tokio::test]
async fn preflight_work_not_found() {
    let deps = fake_deps();
    let plan = Plan::new("x".to_string());
    let b = accepted_bundle(WorkId::new());
    let result = crate::integrate(&[b], &plan, &deps).await;
    match result {
        Err(IntegrationError::WorkNotFound { .. }) => {}
        other => panic!("expected WorkNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn preflight_plan_bundle_mismatch() {
    let plan_a = Plan::new("a".to_string());
    let plan_b = Plan::new("b".to_string());
    // Work belongs to plan_a but we call integrate with plan_b.
    let mut work = Work::new(plan_a.id.clone(), "w".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["done".to_string()]);
    let b = accepted_bundle(work.id.clone());
    let deps = IntegratorDeps {
        works: FakeWorks::new().with(work.clone()),
        ..fake_deps()
    };
    let result = crate::integrate(&[b], &plan_b, &deps).await;
    match result {
        Err(IntegrationError::PlanBundleMismatch {
            work_plan_id, plan_id, ..
        }) => {
            assert_eq!(work_plan_id, plan_a.id.as_ref());
            assert_eq!(plan_id, plan_b.id.as_ref());
        }
        other => panic!("expected PlanBundleMismatch, got {other:?}"),
    }
}
