use std::str::FromStr;

use tempfile::TempDir;

use domain::{Plan, PlanId, PlanStatus};
use store::{PlansStore, Store, StoreError, TASKSTORE_SUBPATH};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn plans_store_is_send_sync() {
    assert_send_sync::<Store>();
    assert_send_sync::<PlansStore<'static>>();
}

#[test]
fn taskstore_subpath_is_loopr_taskstore() {
    // Pin the layout: if this changes, target-repo git exclusion rules,
    // docs, and the merge-driver install-path all need to move in
    // lockstep. Better to fail one test than to silently split the
    // "where does the taskstore live?" contract.
    assert_eq!(TASKSTORE_SUBPATH, ".loopr/taskstore");
}

async fn fresh_store() -> (TempDir, Store) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    (dir, store)
}

#[tokio::test]
async fn open_creates_nested_taskstore_directory_under_loopr() {
    // Verifies two things at once:
    // 1. The seam with the upstream `AsyncStore::open_at(path, opts)` API —
    //    if taskstore regresses to an API that ignores or re-derives the
    //    path, this test fails because the directory appears somewhere else
    //    (e.g. `<target>/.taskstore/` under the legacy behavior).
    // 2. That `Store::open` actually uses `TASKSTORE_SUBPATH` — a subtle
    //    `target.join("taskstore")` or `target.join(".taskstore")` typo in
    //    the wrapper would make the round-trip tests still pass but would
    //    put the committed state in the wrong place.
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();

    let loopr_dir = target.join(".loopr");
    let taskstore_dir = target.join(".loopr").join("taskstore");
    let legacy_dir = target.join(".taskstore");

    assert!(!loopr_dir.exists(), "pre-open: no .loopr/ at target");
    assert!(!taskstore_dir.exists(), "pre-open: no .loopr/taskstore/ at target");
    assert!(!legacy_dir.exists(), "pre-open: no legacy .taskstore/ at target");

    let store = Store::open(target).await.expect("open");

    assert!(loopr_dir.is_dir(), "post-open: .loopr/ created at target root");
    assert!(
        taskstore_dir.is_dir(),
        "post-open: .loopr/taskstore/ created (nested under .loopr/)"
    );
    assert!(
        !legacy_dir.exists(),
        "post-open: bare .taskstore/ at the target root must NOT be created — that would be the pre-v0.5.14 layout"
    );

    store.close().await.expect("close");
}

#[tokio::test]
async fn open_persists_plans_jsonl_under_loopr_taskstore() {
    // Corollary to the directory-layout test: not only must the
    // taskstore live at `.loopr/taskstore/`, the JSONL truth files have
    // to land inside it. If taskstore ever nests a further subdirectory
    // (unlikely, but the seam is worth pinning) this fails loudly.
    let dir = TempDir::new().expect("tempdir");
    let target = dir.path();
    let store = Store::open(target).await.expect("open");

    let plan = Plan::new("test goal for on-disk verification".to_string());
    store.plans().create(plan.clone()).await.expect("create");
    store.close().await.expect("close");

    let plans_jsonl = target.join(".loopr").join("taskstore").join("plans.jsonl");
    assert!(
        plans_jsonl.is_file(),
        "plans.jsonl exists at the nested location: {}",
        plans_jsonl.display()
    );
    let body = std::fs::read_to_string(&plans_jsonl).expect("read jsonl");
    assert!(
        body.contains(plan.id.as_ref()),
        "plans.jsonl contains the plan id: body={body}"
    );
    assert!(body.contains("test goal for on-disk verification"), "goal round-trips");

    // No competing copy at the pre-v0.5.14 legacy path.
    assert!(
        !target.join(".taskstore").join("plans.jsonl").exists(),
        "legacy .taskstore/plans.jsonl must NOT exist"
    );
}

#[tokio::test]
async fn two_stores_under_two_different_target_roots_are_isolated() {
    // Targets are repo-scoped; opening Store::open against two distinct
    // tempdirs must produce two independent taskstore directories with
    // no shared state. This is the positive case for target isolation
    // and also a sanity check that `open_at` honors the caller-provided
    // path instead of some process-global default.
    let dir_a = TempDir::new().expect("tempdir A");
    let dir_b = TempDir::new().expect("tempdir B");

    let store_a = Store::open(dir_a.path()).await.expect("open A");
    let store_b = Store::open(dir_b.path()).await.expect("open B");

    let plan_a = Plan::new("in target A".to_string());
    let plan_b = Plan::new("in target B".to_string());
    store_a.plans().create(plan_a.clone()).await.expect("create A");
    store_b.plans().create(plan_b.clone()).await.expect("create B");

    // A sees only A; B sees only B.
    let listed_a = store_a.plans().list().await.expect("list A");
    let listed_b = store_b.plans().list().await.expect("list B");
    assert_eq!(listed_a.len(), 1);
    assert_eq!(listed_b.len(), 1);
    assert_eq!(listed_a[0].goal, "in target A");
    assert_eq!(listed_b[0].goal, "in target B");

    // And the on-disk paths are distinct files.
    let jsonl_a = dir_a.path().join(".loopr").join("taskstore").join("plans.jsonl");
    let jsonl_b = dir_b.path().join(".loopr").join("taskstore").join("plans.jsonl");
    assert!(jsonl_a.is_file(), "A's plans.jsonl exists in A");
    assert!(jsonl_b.is_file(), "B's plans.jsonl exists in B");
    let canon_a = jsonl_a.canonicalize().expect("canon A");
    let canon_b = jsonl_b.canonicalize().expect("canon B");
    assert_ne!(canon_a, canon_b, "A's and B's plans.jsonl are different files");

    store_a.close().await.expect("close A");
    store_b.close().await.expect("close B");
}

#[tokio::test]
async fn close_returns_ok_on_fresh_store() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(dir.path()).await.expect("open");
    store.close().await.expect("close");
}

#[tokio::test]
async fn empty_store_list_returns_empty_vec() {
    let (_dir, store) = fresh_store().await;
    let plans = store.plans().list().await.expect("list");
    assert!(plans.is_empty());
}

#[tokio::test]
async fn create_returns_id_that_resolves() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("test goal".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let got = store.plans().get(&id).await.expect("get");
    assert_eq!(got.id, plan.id);
    assert_eq!(got.goal, plan.goal);
}

#[tokio::test]
async fn create_then_get_roundtrip() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("roundtrip".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let got = store.plans().get(&id).await.expect("get");
    assert_eq!(got.id, plan.id);
    assert_eq!(got.updated_at, plan.updated_at);
    assert_eq!(got.created_at, plan.created_at);
    assert_eq!(got.goal, plan.goal);
    assert_eq!(got.status, plan.status);
}

#[tokio::test]
async fn create_then_list_sees_record() {
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("seen in list".to_string());
    let id = store.plans().create(plan.clone()).await.expect("create");
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].goal, plan.goal);
}

#[tokio::test]
async fn create_two_then_list_returns_both() {
    let (_dir, store) = fresh_store().await;
    let p1 = Plan::new("first".to_string());
    let p2 = Plan::new("second".to_string());
    let id1 = store.plans().create(p1).await.expect("create p1");
    let id2 = store.plans().create(p2).await.expect("create p2");
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 2);
    let ids: Vec<_> = listed.iter().map(|p| p.id.clone()).collect();
    assert!(ids.contains(&id1), "id1 should be in list");
    assert!(ids.contains(&id2), "id2 should be in list");
}

#[tokio::test]
async fn get_missing_returns_record_not_found() {
    let (_dir, store) = fresh_store().await;
    let nonexistent = PlanId::from_str("pl-xxxxx").expect("infallible");
    let err = store.plans().get(&nonexistent).await.expect_err("should miss");
    match err {
        StoreError::RecordNotFound { collection, id } => {
            assert_eq!(collection, "plans");
            assert_eq!(id, "pl-xxxxx");
        }
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn create_same_id_twice_returns_already_exists() {
    let (_dir, store) = fresh_store().await;
    let p1 = Plan::new("first".to_string());
    let p2 = Plan {
        id: p1.id.clone(),
        updated_at: p1.updated_at,
        created_at: p1.created_at,
        goal: "second with same id".to_string(),
        status: p1.status,
    };
    store.plans().create(p1).await.expect("first create");
    let err = store.plans().create(p2).await.expect_err("second should reject");
    match err {
        StoreError::AlreadyExists { collection, id: _ } => {
            assert_eq!(collection, "plans");
        }
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), 1, "duplicate should not have overwritten");
}

#[tokio::test]
async fn serde_roundtrip_survives_jsonl() {
    let (_dir, store) = fresh_store().await;

    let cases = [
        ("unicode: 日本語 émojis 🌊", PlanStatus::Active),
        ("", PlanStatus::Draft),
        ("pending", PlanStatus::Pending),
        ("done", PlanStatus::Complete),
        ("replaced", PlanStatus::Superseded),
        ("given up", PlanStatus::Abandoned),
    ];

    for (goal, status) in cases.iter() {
        let mut plan = Plan::new(goal.to_string());
        plan.status = *status;
        let id = store.plans().create(plan.clone()).await.expect("create");
        let got = store.plans().get(&id).await.expect("get");
        assert_eq!(got.goal, plan.goal, "goal should roundtrip for status {status:?}");
        assert_eq!(got.status, plan.status, "status should roundtrip for {goal}");
    }

    let listed = store.plans().list().await.expect("list");
    assert_eq!(listed.len(), cases.len());
}

// ---------------------------------------------------------------------------
// Phase 3 F1/F2/F3: Plan OCC, monotonic floor, missing-id guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_floors_updated_at_strictly_above_prior() {
    // Phase 3 F2 (plans): the floor lands exactly at prior + 1 when the
    // on-disk value is in the future relative to the wall clock — the
    // deterministic stand-in for a same-millisecond write.
    let (_dir, store) = fresh_store().await;
    let mut plan = Plan::new("p".to_string());
    let id = plan.id.clone();
    let future = domain::now_millis() + 1_000_000;
    plan.updated_at = future;
    plan.created_at = future;
    store.plans().create(plan.clone()).await.expect("create");

    plan.status = PlanStatus::Complete;
    let new_ts = store.plans().update(plan, future).await.expect("update");
    assert_eq!(new_ts, future + 1, "floor lands exactly at prior + 1");
    let got = store.plans().get(&id).await.expect("get");
    assert_eq!(got.updated_at, future + 1);
    assert_eq!(got.status, PlanStatus::Complete);
}

#[tokio::test]
async fn update_stale_expected_is_rejected() {
    // Phase 3 F1: a writer holding a stale `expected_updated_at` (a
    // concurrent winner already advanced the Plan) must be rejected with
    // Stale rather than silently clobbering.
    let (_dir, store) = fresh_store().await;
    let plan = Plan::new("p".to_string());
    let id = plan.id.clone();
    let snapshot = plan.updated_at;
    store.plans().create(plan).await.expect("create");

    // Winner advances the Plan.
    let mut winner = store.plans().get(&id).await.expect("get");
    winner.status = PlanStatus::Complete;
    store.plans().update(winner, snapshot).await.expect("winner update");

    // Loser still holds the original snapshot -> Stale.
    let mut loser = Plan::new("p".to_string());
    loser.id = id.clone();
    loser.status = PlanStatus::Stalled;
    let err = store
        .plans()
        .update(loser, snapshot)
        .await
        .expect_err("stale must reject");
    match err {
        StoreError::Stale { expected, actual } => {
            assert_eq!(expected, snapshot);
            assert!(actual > expected);
        }
        other => panic!("expected Stale, got {other:?}"),
    }
    // The winner's Complete must survive.
    let after = store.plans().get(&id).await.expect("get after");
    assert_eq!(
        after.status,
        PlanStatus::Complete,
        "stale loser must not clobber winner"
    );
}

#[tokio::test]
async fn update_missing_id_returns_record_not_found() {
    // Phase 3 F3: taskstore `update` is an upsert and would silently
    // CREATE a missing id; the OCC pre-`get` converts that into an
    // explicit RecordNotFound (matching Works/Bundles).
    let (_dir, store) = fresh_store().await;
    let mut ghost = Plan::new("never persisted".to_string());
    let expected = ghost.updated_at;
    ghost.status = PlanStatus::Complete;
    let err = store
        .plans()
        .update(ghost, expected)
        .await
        .expect_err("missing id must reject");
    match err {
        StoreError::RecordNotFound { collection, .. } => assert_eq!(collection, "plans"),
        other => panic!("expected RecordNotFound, got {other:?}"),
    }
    assert!(
        store.plans().list().await.expect("list").is_empty(),
        "no phantom create"
    );
}
