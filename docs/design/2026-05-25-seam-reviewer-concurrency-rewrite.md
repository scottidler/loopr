# Replace `seam_reviewer_concurrency` with a seam-level error-propagation test

- Crates touched: agents
- Status: Implemented

## Implemented as

After Architect review, the original three-test rewrite collapsed to a single new test. The Architect verified empirically against the codebase:

- `crates/store/src/bundles/tests.rs::update_stale_version_rejected` already pins sequential OCC snapshot-mismatch (the invariant proposed "Test 1" would have duplicated).
- `crates/store/src/bundles/tests.rs::concurrent_updates_produce_exactly_one_winner` already pins true async lock contention through `Store::update`'s internal `update_lock`.
- `llm::ScriptedLlm` already exists and is used by `seam_reviewer.rs`, `seam_implementer.rs`, and `instrumentation.rs` — proposed `FakeLlm` was reinventing it.

What shipped (single commit):

1. Deleted `crates/agents/tests/seam_reviewer_concurrency.rs`.
2. Added `crates/agents/tests/seam_reviewer_stale.rs` with one test, `reviewer_propagates_store_stale`, using `llm::ScriptedLlm` and manufacturing staleness deterministically (no concurrency, no barrier, no hang risk). Tests invariant B (reviewer-error-propagation) only; invariant A (Store OCC) is left to the existing store-level tests.

No new store tests, no `FakeLlm`. Verified: `cargo test -p agents` completes in ~30s with all 14 tests passing.

## Context

`cargo test --workspace` in this repo hung for 7+ hours on 2026-05-24. Postmortem: [`docs/agents-test-hang-2026-05-24.md`](../agents-test-hang-2026-05-24.md). The hung binary was `agents-758f0deb91919c02`. Symptoms: `futex_do_wait`, sleeping, 2 threads, 0 nonvoluntary context switches, ~432 minutes accumulated CPU. Process killed; specific test not identified before kill.

Triage points to `crates/agents/tests/seam_reviewer_concurrency.rs::two_concurrent_reviewers_exactly_one_wins_occ` as the deadlock site.

## The current test

The test pins the OCC invariant: when two reviewers race to update the same `Bundle`, exactly one wins, the other receives `ReviewerError::Update(BundleUpdateError::Stale { .. })`. The invariant itself is real and worth pinning — if a regression in `Store::update` silently allowed last-writer-wins, we'd corrupt Bundle state under daemon concurrency with no test catching it.

The way the test pins the invariant is:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_reviewers_exactly_one_wins_occ() {
    // ... build repo, plan, work, bundle, store ...

    let barrier = Arc::new(Barrier::new(2));  // requires EXACTLY 2 participants

    let h1 = tokio::spawn(async move {
        let deps = ReviewerDeps {
            llm: GatedLlm { barrier: barrier1, response: "...accept..." },
            // ...
        };
        run_reviewer(&stored1, &work1, &deps).await
    });

    let h2 = tokio::spawn(async move {
        let deps = ReviewerDeps {
            llm: GatedLlm { barrier: barrier2, response: "...reject..." },
            // ...
        };
        run_reviewer(&stored2, &work2, &deps).await
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    // assert exactly one Ok, exactly one Stale
}
```

Where `GatedLlm` is:

```rust
impl LlmClient for GatedLlm {
    async fn complete_free(&self, ...) -> Result<(String, Usage), LlmError> {
        self.barrier.wait().await;  // <-- 2-participant barrier
        Ok((self.response.clone(), Usage::default()))
    }
}
```

## Why it deadlocks

`run_reviewer` (`crates/agents/src/reviewer.rs`, lines ~128-216) has **three early-return paths before it ever reaches `complete_free`**:

1. `bundle.work_id.to_string() != work.id.to_string()` → `ReviewerError::Mismatch` (line 146).
2. `git_show(&deps.target, sha, &bundle.paths).await?` (line 152). Both spawned tasks run `git -C <same-repo> show <sha> -- src.rs` concurrently. `git show` is read-only, but git's internal housekeeping (`index.lock`, `packed-refs` rewrites) can produce transient non-zero exits under concurrent invocation on the same repo. A single non-zero exit returns `ReviewerError::Git(stderr)`.
3. `deps.context.build_for_reviewer(bundle, work, &diff, noop_slice)?` (line 164) — can return `ReviewerError`.

If exactly one of the two tasks fails any of those three steps, the **other** task arrives at `self.barrier.wait().await` alone. `tokio::sync::Barrier::new(2)` blocks the first arrival until N arrive — when only 1 ever arrives, the task is parked on the underlying futex forever. There is no timeout anywhere in the path.

This matches the postmortem evidence: futex wait, one tokio worker parked on the barrier, main thread parked on `h1.await` or `h2.await`. The 432-min accumulated CPU is from the other ~9 test files in the `agents` test binary running their git subprocesses to completion in parallel before the binary settled on the one stuck task. libtest's default `--test-threads=N>1` ran them concurrently; once everything else finished, the binary stayed alive forever waiting on this one.

## The conflation

The test commingles two assertions:

- **A.** Store OCC rejects stale writes (`store.update(bundle, expected_updated_at)` returns `Stale` if `updated_at` no longer matches).
- **B.** `run_reviewer` propagates the Store's `Stale` error correctly as `ReviewerError::Update(BundleUpdateError::Stale)`.

The barrier dance buys us a "true" race for both A and B in one test, but at the cost of every pre-LLM step in `run_reviewer` becoming load-bearing on the test's symmetry. Any code change that makes one task fail asymmetrically (a new validation, a different `git show` invocation, prompt-assembly change that errors on edge cases) silently turns this test from "race-passes" to "hangs forever."

A and B are independent invariants. They should be tested independently.

## Proposed change

**Delete** `crates/agents/tests/seam_reviewer_concurrency.rs`.

**Add** two seam-level tests, each pinning exactly one invariant, neither relying on concurrency to manufacture the conflict.

### Test 1 — `crates/store/src/bundles/tests.rs::occ_rejects_stale_update`

Pins invariant **A**: the Store's OCC mechanism rejects writes whose `expected_updated_at` snapshot no longer matches the on-disk row, regardless of how the conflict was produced.

```rust
#[tokio::test]
async fn occ_rejects_stale_update() {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path()).await.unwrap();

    let plan = domain::Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "w".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["ok".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(work.id.clone(), "br".to_string(), vec!["c".to_string()]);
    bundle.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    // Both writers snapshot the same updated_at.
    let snap = store.bundles().get(&bundle.id).await.unwrap();

    let mut a = snap.clone();
    a.verification = "winner".into();
    a.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();

    let mut b = snap.clone();
    b.verification = "loser".into();
    b.transition(BundleStatus::Rejected, Role::Reviewer).unwrap();

    // Sequential calls; OCC operates on the snapshot, not wall-clock concurrency.
    let r1 = store.bundles().update(a, snap.updated_at).await;
    let r2 = store.bundles().update(b, snap.updated_at).await;

    assert!(r1.is_ok(), "first update should win, got {r1:?}");
    assert!(
        matches!(r2, Err(BundleUpdateError::Stale { .. })),
        "second update should be Stale, got {r2:?}"
    );

    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Reviewed);
    assert_eq!(final_bundle.verification, "winner");
}
```

Properties:

- No tokio runtime tricks, no barrier, no spawned tasks, no git subprocess, no LLM mock.
- Tests the OCC invariant **at the Store seam where it actually lives**.
- Cannot hang. Deterministic. Runs in milliseconds.
- Belongs in the `store` crate's test module because the invariant under test is a property of `Store::update`, not of `run_reviewer`.

### Test 2 — `crates/agents/tests/seam_reviewer_stale.rs::reviewer_propagates_store_stale`

Pins invariant **B**: when `Store::update` returns `Stale` from inside `run_reviewer`, the reviewer surfaces it as `ReviewerError::Update(BundleUpdateError::Stale)` (no swallow, no remap).

```rust
#[tokio::test]
async fn reviewer_propagates_store_stale() {
    let (_dir, repo_path, sha) = init_repo_with_commit();
    let store = Arc::new(Store::open(&repo_path).await.unwrap());

    let plan = domain::Plan::new("p".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "w".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["ok".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(work.id.clone(), "br".to_string(), vec!["claim".to_string()]);
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    // Snapshot the bundle BEFORE racing — this is the snapshot `run_reviewer`
    // will pass to `store.update(..., expected_updated_at=stored.updated_at)`.
    let stored = store.bundles().get(&bundle.id).await.unwrap();

    // Manufacture staleness deterministically: bump updated_at on disk via
    // an unrelated mutation, so the snapshot held by run_reviewer no longer
    // matches what's on disk by the time it tries to commit.
    let mut bump = stored.clone();
    bump.verification = "external mutation".into();
    bump.transition(BundleStatus::Reviewed, Role::Reviewer).unwrap();
    store.bundles().update(bump, stored.updated_at).await.unwrap();

    // Now run_reviewer with the original (now-stale) snapshot. No concurrency,
    // no barrier — staleness is pre-loaded into the input.
    let deps = ReviewerDeps {
        llm: FakeLlm::new(r#"{"kind":"accept","summary":"ok"}"#),
        store: Arc::as_ref(&store),
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path,
        path_deny_patterns: Vec::new(),
    };

    let r = run_reviewer(&stored, &work, &deps).await;

    assert!(
        matches!(r, Err(ReviewerError::Update(BundleUpdateError::Stale { .. }))),
        "reviewer should propagate Stale, got {r:?}"
    );
}

struct FakeLlm { response: String }
impl FakeLlm { fn new(r: &str) -> Self { Self { response: r.to_string() } } }

impl LlmClient for FakeLlm {
    async fn complete_with_tool(&self, _s: &str, _u: &str, _t: LlmToolSchema, _m: Option<&str>)
        -> Result<(ToolCall, Usage), LlmError> { panic!("unused") }
    async fn complete_free(&self, _s: &str, _m: &[Message], _model: Option<&str>)
        -> Result<(String, Usage), LlmError> {
        Ok((self.response.clone(), Usage::default()))
    }
}
```

Properties:

- Runs `run_reviewer` end-to-end with a real `Store`, real `git_show`, real prompt assembly — the whole pipeline that the deleted test exercised.
- The conflict is **manufactured deterministically** by bumping `updated_at` on disk before invoking the reviewer. No race, no timing, no barrier.
- Tests exactly one assertion: the reviewer's error propagation. Pre-LLM failure modes (git, context) now fail loudly as test failures, not as silent hangs.
- Cannot hang on its own gating logic. Worst case it fails fast if any of the early-return paths fire.

## Why this is better

1. **Failure modes are visible.** A regression in `git_show`, `build_for_reviewer`, or `Store::update` shows up as a clear test failure with a meaningful assertion message — not as a 7-hour CI hang.
2. **Each test pins one invariant.** Reading the test tells you what it's protecting. The current test conflates the Store invariant and the reviewer propagation invariant inside a concurrency dance that doesn't actually test concurrency — `tokio::sync::Barrier` is a deterministic synchronization point, not a true race.
3. **No exact-participant-count traps.** The current pattern (Barrier::new(2) + spawn + early-return-possible body) is a known hazard. We don't propagate it elsewhere.
4. **Faster.** Two synchronous-with-tokio tests vs. spinning a multi_thread runtime and racing two pipeline executions.

## What's left untested

True wall-clock concurrent races inside the `Store`'s file/SQLite/git-hook anti-corruption layer are not exercised by either test above. That's fine for this rewrite — concurrent stress is a different kind of test (loom-style, fuzz-style, or a focused stress harness inside `store/`), and the deleted test wasn't actually catching that either; it was catching the OCC snapshot mismatch, which the two tests above pin directly.

If we later want a real concurrent stress test for `Store::update`, it should live in `crates/store/tests/` (or its `tests.rs` module) and use a participation-tolerant pattern (e.g. spawn N tasks, wait for all to finish via `JoinSet::join_all`, count `Ok` and `Stale` results, accept N-1 stales) — never a barrier requiring exact participation.

## Rollout

Single commit:

1. Delete `crates/agents/tests/seam_reviewer_concurrency.rs`.
2. Add `crates/store/src/bundles/tests.rs::occ_rejects_stale_update` (or its existing test module — wherever bundle-store tests live today).
3. Add `crates/agents/tests/seam_reviewer_stale.rs`.
4. `otto ci` at workspace root and `cargo test -p agents` / `cargo test -p store` to confirm both new tests pass and the binary no longer hangs.

No coexistence period, no feature flag, no deprecation window. The deleted test is replaced functionally by the two new ones in the same commit.

## Open questions for the Architect

1. Is there a value the deleted concurrency test was providing that neither of the two replacement tests captures? (Specifically: does the test contribute anything beyond pinning A and B?)
2. Should Test 1 live in `crates/store/src/bundles/tests.rs` (closer to the code) or `crates/store/tests/` (true integration test)? The Store crate's existing conventions should decide this; if the existing OCC tests live in one or the other, match that.
3. Is the `FakeLlm` struct in Test 2 worth promoting to a shared test fixture in `agents`? Several seam tests already roll their own minimal `LlmClient` impl.
4. Is there a class of `Store::update` failure (e.g. a write that partially succeeded then the snapshot check fails) that would be missed by both the deleted test and the proposed replacements?
