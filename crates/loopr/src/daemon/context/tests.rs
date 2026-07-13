//! Unit tests for `daemon/context.rs` — Phase 2 sidecar-map RAII guard.

use std::collections::HashMap;
use std::sync::{Arc, RwLock as StdRwLock};

use domain::WorkId;

use super::ScopedIdGuard;

#[test]
fn scoped_id_guard_inserts_on_construction() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    let _guard = ScopedIdGuard::new(Arc::clone(&map), key.clone());

    let snapshot = map.read().unwrap();
    assert_eq!(snapshot.len(), 1, "guard should insert exactly one entry");
    assert!(snapshot.contains_key(&key), "inserted key must be present");
}

#[test]
fn scoped_id_guard_removes_on_normal_drop() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    {
        let _guard = ScopedIdGuard::new(Arc::clone(&map), key.clone());
        assert_eq!(map.read().unwrap().len(), 1, "live during scope");
    }
    assert!(
        map.read().unwrap().is_empty(),
        "guard Drop must remove the entry on normal scope exit"
    );
}

#[test]
fn scoped_id_guard_removes_on_panic_unwind() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let key = WorkId::new();
    let map_for_thread = Arc::clone(&map);
    let key_for_thread = key.clone();

    // Run the panic-prone code on a thread so the test process survives.
    let join = std::thread::spawn(move || {
        let _guard = ScopedIdGuard::new(Arc::clone(&map_for_thread), key_for_thread);
        // The thread panics while the guard is in scope; Rust's unwind
        // must invoke ScopedIdGuard::drop on the way out, removing the
        // entry. This is the panic-unwind correctness guarantee.
        panic!("simulated task panic");
    });
    let result = join.join();
    assert!(result.is_err(), "thread should have panicked");

    assert!(
        map.read().unwrap().is_empty(),
        "guard Drop must remove the entry even when the surrounding scope panics"
    );
}

#[test]
fn scoped_id_guards_keep_independent_keys() {
    let map: Arc<StdRwLock<HashMap<WorkId, ()>>> = Arc::new(StdRwLock::new(HashMap::new()));
    let k1 = WorkId::new();
    let k2 = WorkId::new();
    let k3 = WorkId::new();

    let g1 = ScopedIdGuard::new(Arc::clone(&map), k1.clone());
    let _g2 = ScopedIdGuard::new(Arc::clone(&map), k2.clone());
    let g3 = ScopedIdGuard::new(Arc::clone(&map), k3.clone());

    {
        let snap = map.read().unwrap();
        assert_eq!(snap.len(), 3);
        assert!(snap.contains_key(&k1));
        assert!(snap.contains_key(&k2));
        assert!(snap.contains_key(&k3));
    }

    drop(g1);
    drop(g3);

    let snap = map.read().unwrap();
    assert_eq!(snap.len(), 1, "g2 alone remains live");
    assert!(snap.contains_key(&k2));
    assert!(!snap.contains_key(&k1));
    assert!(!snap.contains_key(&k3));
}

#[test]
fn panic_message_extracts_str_payload() {
    // `panic!("...")` boxes a &str payload.
    let payload = std::panic::catch_unwind(|| panic!("implementer boom")).expect_err("should panic");
    assert_eq!(super::panic_message(&*payload), "implementer boom");
}

#[test]
fn panic_message_extracts_string_payload() {
    let s = "dynamic boom".to_string();
    let payload = std::panic::catch_unwind(move || panic!("{s}")).expect_err("should panic");
    assert_eq!(super::panic_message(&*payload), "dynamic boom");
}

#[test]
fn panic_message_handles_opaque_payload() {
    // `panic_any` with a non-string payload is opaque.
    let payload = std::panic::catch_unwind(|| std::panic::panic_any(42u32)).expect_err("should panic");
    assert_eq!(super::panic_message(&*payload), "<non-string panic payload>");
}

// ---------------------------------------------------------------------------
// Phase 5: shutdown drain guards + single-span hygiene.
// ---------------------------------------------------------------------------

mod shutdown {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use agents::{DirectorConfig, ImplementerConfig, ReviewerConfig};
    use context::InlineContextBuilder;
    use domain::{Plan, Work, WorkStatus};
    use llm::{AnthropicClient, LlmConfig};
    use serde_json::Value;
    use store::Store;
    use telemetry::{ProcessId, SessionId};
    use tempfile::TempDir;
    use tools::{BashDenylist, LaneRouter, SandboxMode};
    use worktree::AttemptCleanupPolicy;

    use crate::daemon::DaemonContext;
    use crate::daemon::context::promote_unblocked_siblings;

    fn dummy_llm() -> Arc<AnthropicClient> {
        let cfg = LlmConfig {
            api_base_url: "http://127.0.0.1:1".to_string(),
            ..LlmConfig::default()
        };
        Arc::new(AnthropicClient::new(cfg, "test-key".to_string()).unwrap())
    }

    async fn ctx_for_test(target: PathBuf) -> Arc<DaemonContext<AnthropicClient>> {
        let store = Store::open(&target).await.unwrap();
        let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
        let bash_denylist = Arc::new(BashDenylist::with_base());
        let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
            "test-stub-model",
        )));
        Arc::new(DaemonContext::new(
            target,
            SessionId::parse("20260419-000000").unwrap(),
            "-test-target".to_string(),
            ProcessId::parse("pc-test01").unwrap(),
            std::process::id(),
            store,
            dummy_llm(),
            router,
            bash_denylist,
            Vec::new(),
            SandboxMode::Off,
            Arc::new(InlineContextBuilder::new()),
            ImplementerConfig::default(),
            ReviewerConfig::default(),
            integrator::IntegratorConfig::default(),
            DirectorConfig::default(),
            decomposer::DecomposerConfig::default(),
            AttemptCleanupPolicy::default(),
            snapshot,
            crate::transport::ServerTimeouts::default(),
            None,
            4,
        ))
    }

    fn read_events(run_dir: &Path) -> Vec<Value> {
        let body = std::fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
        body.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("parse JSONL"))
            .collect()
    }

    /// Names of every span in scope for `event` (the `spans` array plus the
    /// `span` current-span object), as the JSON fmt layer serializes them.
    fn span_names(event: &Value) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(spans) = event.get("spans").and_then(Value::as_array) {
            for s in spans {
                if let Some(n) = s.get("name").and_then(Value::as_str) {
                    names.push(n.to_string());
                }
            }
        }
        names
    }

    /// The integrator's post-`Done` continuation (`promote_unblocked_siblings`)
    /// must NOT spawn an implementer into an already-draining pool once the
    /// shutdown signal has landed. Break-to-prove: without the pre-spawn
    /// guard the sibling is spawned, the JoinSet is non-empty, and the
    /// leaked task strands an `Arc<DaemonContext>` clone so `try_unwrap`
    /// fails.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn promote_skips_spawn_during_shutdown_and_arc_unwraps() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_for_test(td.path().to_path_buf()).await;

        // Plan with two siblings: w1 Done, w2 Pending depending on w1 — so
        // w2 is dep-unblocked and would normally be promoted+spawned.
        let plan = Plan::new("shutdown-drain".to_string());
        ctx.store.plans().create(plan.clone()).await.unwrap();
        let mut w1 = Work::new(plan.id.clone(), "w1".to_string());
        w1.status = WorkStatus::Done;
        let mut w2 = Work::new(plan.id.clone(), "w2".to_string());
        w2.dependencies = vec![w1.id.clone()];
        ctx.store.works().create(w1).await.unwrap();
        ctx.store.works().create(w2).await.unwrap();

        // Signal has landed: the daemon is winding down.
        ctx.shutting_down.store(true, Ordering::Relaxed);

        promote_unblocked_siblings(Arc::clone(&ctx), plan.id.clone()).await;

        assert!(
            ctx.implementer_tasks.lock().await.is_empty(),
            "no implementer may be spawned into a draining pool during shutdown"
        );

        // Sole remaining reference: the daemon can reclaim the Store.
        assert!(
            Arc::try_unwrap(ctx).is_ok(),
            "a leaked spawn would strand an Arc<DaemonContext> clone"
        );
    }

    /// `spawn_implementer_for_work` carries exactly ONE
    /// `daemon.spawn_implementer_for_work` span (the duplicate default-named
    /// `#[tracing::instrument]` was removed). Break-to-prove: with the
    /// duplicate attribute present the entry event is wrapped by a second
    /// span named `spawn_implementer_for_work`, tripping the zero-assertion.
    ///
    /// Driven through the shutdown guard so the body no-ops immediately
    /// (emitting its debug event inside the span) without needing a git
    /// worktree or an LLM round-trip.
    #[tokio::test(flavor = "current_thread")]
    async fn spawn_implementer_emits_single_span() {
        let run_dir = TempDir::new().unwrap();
        let td = TempDir::new().unwrap();
        {
            let _guard = telemetry::init_for_test(run_dir.path(), "debug").expect("init_for_test");
            let ctx = ctx_for_test(td.path().to_path_buf()).await;
            let plan = Plan::new("span-hygiene".to_string());
            let work = Work::new(plan.id.clone(), "w1".to_string());

            ctx.shutting_down.store(true, Ordering::Relaxed);
            // Spawn into a JoinSet exactly as production does: this boxes the
            // recursive spawn-chain future (implementer -> reviewer ->
            // integrator -> promote -> implementer) behind a tokio task, so
            // the compiler never has to lay out the fully-inlined future
            // (a direct `.await` here overflows the type-layout depth). On a
            // current-thread runtime the task is polled on this same thread,
            // so the thread-local test subscriber still captures its events.
            let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            set.spawn(Arc::clone(&ctx).spawn_implementer_for_work(work));
            while set.join_next().await.is_some() {}
        }

        let events = read_events(run_dir.path());
        let entry = events
            .iter()
            .find(|ev| {
                ev.get("fields").and_then(|f| f.get("message")).and_then(Value::as_str)
                    == Some("shutdown in progress; skipping implementer spawn")
            })
            .expect("the shutdown-skip debug event must be captured");

        let names = span_names(entry);
        let named = names
            .iter()
            .filter(|n| *n == "daemon.spawn_implementer_for_work")
            .count();
        assert_eq!(
            named, 1,
            "exactly one daemon.spawn_implementer span per spawn; got {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "spawn_implementer_for_work"),
            "the duplicate default-named instrument span must be gone; got {names:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 15 (`docs/design/2026-07-11-verified-swarm.md`): `budget.reset`'s
// underlying `reset_budget_event` mechanism.
// ---------------------------------------------------------------------------

mod budget {
    use std::path::PathBuf;
    use std::sync::Arc;

    use agents::{DirectorConfig, ImplementerConfig, ReviewerConfig};
    use context::InlineContextBuilder;
    use llm::{AnthropicClient, LlmConfig};
    use store::Store;
    use telemetry::{ProcessId, SessionId};
    use tempfile::TempDir;
    use tools::{BashDenylist, LaneRouter, SandboxMode};
    use worktree::AttemptCleanupPolicy;

    use crate::daemon::DaemonContext;

    fn dummy_llm() -> Arc<AnthropicClient> {
        let cfg = LlmConfig {
            api_base_url: "http://127.0.0.1:1".to_string(),
            ..LlmConfig::default()
        };
        Arc::new(AnthropicClient::new(cfg, "test-key".to_string()).unwrap())
    }

    /// Same shape as the `shutdown` module's `ctx_for_test`, parameterized
    /// by the per-run cost cap so budget-brake tests can drive a live trip.
    async fn ctx_with_cap(target: PathBuf, per_run_cost_usd: Option<f64>) -> Arc<DaemonContext<AnthropicClient>> {
        let store = Store::open(&target).await.unwrap();
        let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
        let bash_denylist = Arc::new(BashDenylist::with_base());
        let snapshot = Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
            "test-stub-model",
        )));
        Arc::new(DaemonContext::new(
            target,
            SessionId::parse("20260419-000000").unwrap(),
            "-test-target".to_string(),
            ProcessId::parse("pc-test01").unwrap(),
            std::process::id(),
            store,
            dummy_llm(),
            router,
            bash_denylist,
            Vec::new(),
            SandboxMode::Off,
            Arc::new(InlineContextBuilder::new()),
            ImplementerConfig::default(),
            ReviewerConfig::default(),
            integrator::IntegratorConfig::default(),
            DirectorConfig::default(),
            decomposer::DecomposerConfig::default(),
            AttemptCleanupPolicy::default(),
            snapshot,
            crate::transport::ServerTimeouts::default(),
            per_run_cost_usd,
            4,
        ))
    }

    /// A daemon that never breached its cap has nothing to reset.
    #[tokio::test]
    async fn reset_on_a_never_tripped_daemon_is_a_no_op_reporting_false() {
        let td = TempDir::new().unwrap();
        let ctx = ctx_with_cap(td.path().to_path_buf(), None).await;
        assert!(!ctx.reset_budget_event(), "an unconfigured (unlimited) cap never trips");
    }

    /// Break-to-prove the OTHER half of Phase 15's success criterion: reset
    /// alone (cap unchanged) must NOT unblock a genuinely over-cap daemon —
    /// `budget_blocks_spawn` re-derives its answer from
    /// `per_run_cost_usd` vs. the live (monotonic) snapshot cost on every
    /// call; `budget_event_sent` only dedupes the WARN/event emission. Only
    /// once the cap itself is raised (here: the sole `Arc<DaemonContext>`
    /// clone in scope, mutated directly via `Arc::get_mut` to stand in for
    /// the config-raise + restart a real deployment requires today) does
    /// the next `budget_blocks_spawn` call return `false` — dispatch
    /// resumes. See this phase's implementation notes (Deviations) for the
    /// full reasoning.
    #[tokio::test]
    async fn reset_clears_the_flag_but_dispatch_resumes_only_after_the_cap_is_raised() {
        let td = TempDir::new().unwrap();
        let mut ctx = ctx_with_cap(td.path().to_path_buf(), Some(0.0)).await;

        // First spawn-gate check: cost (0) >= cap (0.0) trips it and emits
        // the one-shot event; the daemon must not spawn.
        assert!(
            ctx.budget_blocks_spawn("implementer", "wk-test"),
            "a zero cap trips immediately (0 spend >= 0.0 cap)"
        );

        // Reset clears the one-shot flag...
        assert!(
            ctx.reset_budget_event(),
            "the guard was tripped; reset must report that"
        );
        // ...but the cap is still 0.0 and cost is still >= it, so the very
        // next gate check re-trips and blocks again. Resetting alone does
        // not resume dispatch.
        assert!(
            ctx.budget_blocks_spawn("implementer", "wk-test"),
            "cap unchanged: the daemon must still be blocked immediately after reset"
        );

        // Now raise the cap (standing in for the operator's config change +
        // restart) and reset again.
        Arc::get_mut(&mut ctx)
            .expect("sole Arc clone in this test")
            .per_run_cost_usd = Some(1_000_000.0);
        assert!(
            ctx.reset_budget_event(),
            "the re-trip above set the flag again; reset must report that"
        );

        // Dispatch resumes: the cap is no longer exceeded.
        assert!(
            !ctx.budget_blocks_spawn("implementer", "wk-test"),
            "with the cap raised, spawns must no longer be blocked"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 1 (`docs/design/2026-07-12-reviewer-occ-stale-race.md`):
// `transition_and_persist_bundle` — the `updated_at` re-sync and the
// Unchanged-skip that together de-fang the reviewer OCC self-stale doom loop.
// ---------------------------------------------------------------------------

mod bundle_helper {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use domain::{Bundle, BundleStatus, Role, TargetKind, WorkId};
    use store::{BundleUpdateError, BundleUpdateSink, Store};
    use tempfile::TempDir;

    use crate::daemon::context::{BundleTransitionError, transition_and_persist_bundle};

    fn fresh_bundle() -> Bundle {
        Bundle::new(
            WorkId::new(),
            "loopr/test-branch".to_string(),
            vec!["test claim".to_string()],
        )
    }

    /// A `BundleUpdateSink` that records how many times `update` was invoked
    /// and never touches disk — lets the "zero writes on Unchanged" criterion
    /// be asserted at the exact seam the reviewer writes through.
    struct CountingSink {
        calls: AtomicUsize,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
        fn count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl BundleUpdateSink for CountingSink {
        async fn update(
            &self,
            bundle: Bundle,
            _expected_updated_at: i64,
            _role: Role,
            _kind: TargetKind,
        ) -> Result<i64, BundleUpdateError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // A plausible floored value so a Changed caller could re-sync;
            // the Unchanged path under test never reaches this.
            Ok(bundle.updated_at + 1)
        }
    }

    /// Success criterion 1: after a `Changed` transition the in-memory
    /// bundle's `updated_at` equals the floored value persisted on disk — the
    /// re-sync the pre-fix hand-rolled site dropped. Driven against a real
    /// `Store` (its own `BundleUpdateSink` impl) so "on disk" is literal.
    #[tokio::test]
    async fn changed_transition_resyncs_updated_at_to_disk() {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path()).await.unwrap();

        let created = fresh_bundle();
        let id = store.bundles().create(created).await.unwrap();
        let mut in_mem = store.bundles().get(&id).await.unwrap();
        let before = in_mem.updated_at;

        transition_and_persist_bundle(&store, &mut in_mem, BundleStatus::Triaged, Role::Reactor)
            .await
            .expect("Proposed -> Triaged persists");

        let on_disk = store.bundles().get(&id).await.unwrap();
        assert_eq!(on_disk.status, BundleStatus::Triaged);
        assert_eq!(
            in_mem.updated_at, on_disk.updated_at,
            "helper must re-sync the in-memory updated_at to the floored disk value"
        );
        assert!(in_mem.updated_at > before, "a Changed transition advances updated_at");
    }

    /// Success criterion 2: an already-Triaged bundle re-triaged to Triaged
    /// produces ZERO store writes (the Unchanged-skip that removes the
    /// reconcile age-clock reset), asserted against the counting sink. A
    /// no-op transition is `Ok`, not an error.
    #[tokio::test]
    async fn unchanged_transition_skips_the_store_write() {
        let sink = CountingSink::new();
        let mut bundle = fresh_bundle();
        // Advance to Triaged in memory (a legal Proposed -> Triaged hop); this
        // is setup, NOT the transition under test.
        bundle.transition(BundleStatus::Triaged, Role::Reactor).expect("triage");
        let token_before = bundle.updated_at;

        transition_and_persist_bundle(&sink, &mut bundle, BundleStatus::Triaged, Role::Reactor)
            .await
            .expect("Triaged -> Triaged is Unchanged, not an error");

        assert_eq!(
            sink.count(),
            0,
            "Unchanged transition must skip the store write entirely"
        );
        assert_eq!(
            bundle.updated_at, token_before,
            "no write means no updated_at bump — the reconcile age clock stays honest"
        );
    }

    /// The `BundleTransitionError` variants are bundle-worded (not a reuse of
    /// the Work-worded `TransitionError`): a names-tell-the-truth guard so a
    /// log line about a Bundle never says "work".
    #[test]
    fn error_messages_are_bundle_worded() {
        let stale = BundleTransitionError::Stale {
            expected: 10,
            actual: 11,
        };
        let msg = stale.to_string();
        assert!(msg.contains("stale bundle"), "Stale must be bundle-worded: {msg}");
        assert!(!msg.contains("work"), "must not leak Work wording: {msg}");
    }
}
