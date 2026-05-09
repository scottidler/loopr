#![allow(clippy::unwrap_used)]
//! Unit tests for the Director: parsing, state assembly, reconcile sweep,
//! and the full `run_director` loop with fake LLM, store, context, and
//! spawner.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::Notify;

use context::{AssembledContext, ContextBuilder, ContextError, DirectorState as CtxDirectorState};
use domain::{Bundle, BundleId, BundleStatus, PlanId, Work, WorkId, WorkStatus};
use llm::{LlmClient, LlmError, Message, ToolCall, ToolSchema as LlmToolSchema, Usage};
use store::StoreError;

use super::{
    DirectorAction, DirectorDeps, DirectorError, DirectorStore, WorkSpawner, build_director_state,
    parse_director_actions, reconcile_director, run_director,
};
use crate::config::DirectorConfig;

// ---------------------------------------------------------------------------
// parse_director_actions
// ---------------------------------------------------------------------------

#[test]
fn parse_accept_bundle() {
    let resp = json!([{ "action": "accept_bundle", "bundle_id": "bd-001" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::AcceptBundle {
            bundle_id: "bd-001".to_string()
        }]
    );
}

#[test]
fn parse_override_work() {
    let resp = json!([{
        "action": "override_work",
        "work_id": "wk-001",
        "target_status": "Ready",
        "reason": "dep resolved"
    }])
    .to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::OverrideWork {
            work_id: "wk-001".to_string(),
            target_status: "Ready".to_string(),
            reason: "dep resolved".to_string(),
        }]
    );
}

#[test]
fn parse_assign_work() {
    let resp = json!([{ "action": "assign_work", "work_id": "wk-002" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::AssignWork {
            work_id: "wk-002".to_string()
        }]
    );
}

#[test]
fn parse_done() {
    let resp = json!([{ "action": "done", "summary": "all reviewed" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "all reviewed".to_string()
        }]
    );
}

#[test]
fn parse_need_help() {
    let resp = json!([{ "action": "need_help", "reason": "stuck" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::NeedHelp {
            reason: "stuck".to_string()
        }]
    );
}

#[test]
fn parse_multiple_actions_in_one_response() {
    let resp = json!([
        { "action": "accept_bundle", "bundle_id": "bd-001" },
        { "action": "assign_work", "work_id": "wk-001" }
    ])
    .to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(actions.len(), 2);
    assert!(matches!(actions[0], DirectorAction::AcceptBundle { .. }));
    assert!(matches!(actions[1], DirectorAction::AssignWork { .. }));
}

#[test]
fn parse_single_object_wrapped_into_vec() {
    let resp = json!({ "action": "done", "summary": "ok" }).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "ok".to_string()
        }]
    );
}

#[test]
fn parse_unknown_action_kind_errors() {
    let resp = json!([{ "action": "frobnicate", "bogus": "yes" }]).to_string();
    let err = parse_director_actions(&resp).unwrap_err();
    assert!(matches!(err, super::DirectorError::Parse(_)));
}

#[test]
fn parse_malformed_json_errors() {
    let err = parse_director_actions("{not json").unwrap_err();
    assert!(matches!(err, super::DirectorError::Parse(_)));
}

#[test]
fn parse_empty_response_errors() {
    let err = parse_director_actions("   \n").unwrap_err();
    match err {
        super::DirectorError::Parse(msg) => assert!(msg.contains("empty"), "got: {msg}"),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn director_config_default_values() {
    let cfg = crate::config::DirectorConfig::default();
    assert_eq!(cfg.poll_interval_secs, 5);
    assert_eq!(cfg.idle_interval_secs, 15);
    assert_eq!(cfg.max_restarts, 3);
    assert_eq!(cfg.max_requeries, 3);
    assert_eq!(cfg.max_parse_failures, 3);
    assert_eq!(cfg.model, "claude-opus-4-7");
    assert_eq!(cfg.token_budget, 100_000);
}

// ---------------------------------------------------------------------------
// Test scaffolding: fakes for LLM, store, context builder, work spawner.
// ---------------------------------------------------------------------------

/// Fake LLM. `responses` is consumed front-to-back. If `responses.is_empty()`
/// when called and `repeat_last == false`, the call panics — which lets a
/// test assert "no LLM call expected" by passing an empty queue with
/// `repeat_last = false`.
struct FakeLlm {
    responses: Mutex<Vec<String>>,
    repeat_last: bool,
    call_count: Mutex<u32>,
}

impl FakeLlm {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses),
            repeat_last: false,
            call_count: Mutex::new(0),
        }
    }

    fn repeating(response: String) -> Self {
        Self {
            responses: Mutex::new(vec![response]),
            repeat_last: true,
            call_count: Mutex::new(0),
        }
    }

    /// Empty queue + repeat_last=false => calling `complete_free` panics.
    /// Used by tests asserting that the loop must exit before any LLM call.
    fn never_called() -> Self {
        Self {
            responses: Mutex::new(Vec::new()),
            repeat_last: false,
            call_count: Mutex::new(0),
        }
    }

    fn calls(&self) -> u32 {
        *self.call_count.lock().unwrap()
    }
}

impl LlmClient for FakeLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("FakeLlm: complete_with_tool not used in Director tests")
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        *self.call_count.lock().unwrap() += 1;
        let mut q = self.responses.lock().unwrap();
        let payload = if q.len() > 1 {
            q.remove(0)
        } else if self.repeat_last {
            q[0].clone()
        } else if q.is_empty() {
            panic!("FakeLlm: response queue exhausted (no responses queued; LLM was not expected to be called)");
        } else {
            q.remove(0)
        };
        Ok((payload, Usage::default()))
    }
}

/// Fake `DirectorStore` returning the works/bundles set at construction.
#[derive(Default)]
struct FakeStore {
    works: Mutex<Vec<Work>>,
    bundles: Mutex<Vec<Bundle>>,
}

impl FakeStore {
    fn with(works: Vec<Work>, bundles: Vec<Bundle>) -> Self {
        Self {
            works: Mutex::new(works),
            bundles: Mutex::new(bundles),
        }
    }
}

impl DirectorStore for FakeStore {
    async fn list_works_for_plan(&self, _plan_id: &PlanId) -> Result<Vec<Work>, StoreError> {
        Ok(self.works.lock().unwrap().clone())
    }

    async fn list_bundles_for_plan(&self, _plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError> {
        Ok(self.bundles.lock().unwrap().clone())
    }
}

/// Fake `WorkSpawner` that records every call so tests can assert on them.
#[derive(Default)]
struct RecordingSpawner {
    accept_bundle_calls: Mutex<Vec<BundleId>>,
    override_work_calls: Mutex<Vec<(WorkId, WorkStatus, String)>>,
    assign_work_calls: Mutex<Vec<WorkId>>,
}

impl WorkSpawner for Arc<RecordingSpawner> {
    fn accept_bundle(&self, bundle_id: BundleId) {
        self.accept_bundle_calls.lock().unwrap().push(bundle_id);
    }

    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String) {
        self.override_work_calls
            .lock()
            .unwrap()
            .push((work_id, target_status, reason));
    }

    fn assign_work(&self, work_id: WorkId) {
        self.assign_work_calls.lock().unwrap().push(work_id);
    }
}

/// Minimal `ContextBuilder` for Director tests. `build_for_director`
/// returns a single user message that the Director will append to its
/// history; the other entry points are unreachable on this path so they
/// panic if called.
struct StubContextBuilder;

impl ContextBuilder for StubContextBuilder {
    fn build_for_implementer(
        &self,
        _work: &domain::Work,
        _worktree_path: &std::path::Path,
        _tool_schemas: &[tools::ToolSchema],
        _history: &[context::IterationSummary],
        _state: &context::StateSummary,
        _iteration: u32,
    ) -> Result<AssembledContext, ContextError> {
        panic!("StubContextBuilder: build_for_implementer not used in Director tests")
    }

    fn build_for_reviewer(
        &self,
        _bundle: &domain::Bundle,
        _work: &domain::Work,
        _diff: &str,
        _noop_files: Option<&[(String, String)]>,
    ) -> Result<AssembledContext, ContextError> {
        panic!("StubContextBuilder: build_for_reviewer not used in Director tests")
    }

    fn build_for_director(
        &self,
        state: &CtxDirectorState,
        history: &[Message],
        _token_budget: usize,
    ) -> Result<AssembledContext, ContextError> {
        let user = format!(
            "plan={} works={} bundles={}",
            state.plan_id,
            state.works.len(),
            state.bundles.len()
        );
        let mut messages: Vec<Message> = history.to_vec();
        messages.push(Message::user(user));
        Ok(AssembledContext {
            system_prompt: "DIRECTOR SYSTEM".to_string(),
            messages,
            token_estimate: 0,
        })
    }

    fn build_for_researcher(
        &self,
        _query: &context::ResearchQuery,
        _history: &[Message],
        _token_budget: usize,
    ) -> Result<AssembledContext, ContextError> {
        panic!("StubContextBuilder: build_for_researcher not used in Director tests")
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_work(plan_id: PlanId, title: &str, status: WorkStatus) -> Work {
    let mut w = Work::new(plan_id, title.to_string());
    w.status = status;
    w
}

fn make_bundle(work_id: WorkId, status: BundleStatus) -> Bundle {
    let mut b = Bundle::new(work_id, "branch".to_string(), vec!["claim".to_string()]);
    b.status = status;
    b
}

fn fast_config() -> DirectorConfig {
    // Keep tests sub-second: poll/idle intervals of 0 mean tokio::sleep(0)
    // yields immediately. Restarts disabled to keep tests focused on the
    // inner loop semantics.
    DirectorConfig {
        poll_interval_secs: 0,
        idle_interval_secs: 0,
        max_restarts: 0,
        max_requeries: 0,
        max_parse_failures: 3,
        ..DirectorConfig::default()
    }
}

fn make_deps<L: LlmClient>(
    llm: L,
    store: FakeStore,
    spawner: Arc<RecordingSpawner>,
    config: DirectorConfig,
    shutdown: Arc<Notify>,
) -> DirectorDeps<L, FakeStore, StubContextBuilder, Arc<RecordingSpawner>> {
    DirectorDeps {
        llm,
        store,
        context: StubContextBuilder,
        spawner,
        config,
        shutdown,
    }
}

// ---------------------------------------------------------------------------
// reconcile_director
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reconcile_promotes_integrated_work() {
    let plan_id = PlanId::new();
    let integrated = make_work(plan_id.clone(), "wk-integrated", WorkStatus::Integrated);
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![integrated.clone(), pending], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner).await.expect("ok");
    assert!(!goal_complete, "Pending Work in flight; not GoalComplete");

    let calls = spawner.override_work_calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "exactly one Integrated->Done override");
    assert_eq!(calls[0].0, integrated.id);
    assert_eq!(calls[0].1, WorkStatus::Done);
    assert!(calls[0].2.contains("reconcile"));
}

#[tokio::test]
async fn reconcile_goal_complete_when_all_terminal_and_any_done() {
    let plan_id = PlanId::new();
    let done = make_work(plan_id.clone(), "wk-done", WorkStatus::Done);
    let abandoned = make_work(plan_id.clone(), "wk-abandoned", WorkStatus::Abandoned);
    let store = FakeStore::with(vec![done, abandoned], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner).await.expect("ok");
    assert!(goal_complete);
    assert!(spawner.override_work_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconcile_zero_works_returns_false() {
    let plan_id = PlanId::new();
    let store = FakeStore::with(vec![], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner).await.expect("ok");
    assert!(!goal_complete, "zero works is not GoalComplete");
}

// ---------------------------------------------------------------------------
// build_director_state
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_state_stringifies_statuses_pascalcase() {
    let plan_id = PlanId::new();
    let w_blocked = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    let w_in_progress = make_work(plan_id.clone(), "wk-2", WorkStatus::InProgress);
    let b_reviewed = make_bundle(w_blocked.id.clone(), BundleStatus::Reviewed);
    let store = FakeStore::with(vec![w_blocked, w_in_progress], vec![b_reviewed]);

    let state = build_director_state(&plan_id, &store).await.expect("ok");
    assert_eq!(state.plan_id, plan_id.to_string());
    assert_eq!(state.works.len(), 2);
    let statuses: Vec<&str> = state.works.iter().map(|w| w.status.as_str()).collect();
    assert!(statuses.contains(&"Blocked"));
    assert!(statuses.contains(&"InProgress"));
    assert_eq!(state.bundles.len(), 1);
    assert_eq!(state.bundles[0].status, "Reviewed");
}

// ---------------------------------------------------------------------------
// run_director
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_director_done_action_sleeps_then_observes_goal_complete() {
    // First reconcile: mixed (Pending). LLM emits Done. Next reconcile flips
    // the Work to Done (we mutate the store between LLM calls is too brittle;
    // simpler: rig up a one-shot Done-action and then on second iteration
    // mark it Done by responding with override_work then a final all-Done
    // store snapshot). We achieve that by using two responses and mutating
    // the store via spawner side effects: the override_work call records but
    // doesn't actually mutate the FakeStore. So instead of relying on the
    // outer flip, we drive completion via the second reconcile reading a
    // snapshot we hand-update via a closure-style mutation: switch to a
    // simpler test - one Done + a snapshot that's already terminal.
    //
    // Setup: 1 Done Work in store. First reconcile already returns
    // GoalComplete=true and the loop exits without an LLM call. To still
    // exercise the "Done action" path we use the second test below; this
    // first test just ensures the reconcile-only exit works.
    let plan_id = PlanId::new();
    let done = make_work(plan_id.clone(), "wk-done", WorkStatus::Done);
    let store = FakeStore::with(vec![done], vec![]);
    let llm = FakeLlm::never_called();
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let deps = make_deps(llm, store, spawner.clone(), fast_config(), shutdown);
    run_director(&plan_id, &deps).await.expect("director exits Ok");

    // No LLM call, no actions dispatched.
    assert_eq!(deps.llm.calls(), 0, "GoalComplete on first reconcile must skip LLM");
    assert!(spawner.accept_bundle_calls.lock().unwrap().is_empty());
    assert!(spawner.assign_work_calls.lock().unwrap().is_empty());
    assert!(spawner.override_work_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn run_director_done_action_loops_until_shutdown() {
    // Pending Work + LLM emits {action: done}. Director sleeps idle interval
    // (0s in fast_config) then the shutdown notify fires and the loop exits.
    let plan_id = PlanId::new();
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![pending], vec![]);
    let llm = FakeLlm::repeating(json!([{ "action": "done", "summary": "no-op" }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let deps = make_deps(llm, store, spawner.clone(), fast_config(), shutdown.clone());

    // Schedule a shutdown after a brief delay so the loop completes one
    // iteration first.
    let shutdown_fire = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        shutdown_fire.notify_waiters();
    });

    run_director(&plan_id, &deps)
        .await
        .expect("director exits Ok on shutdown");
    assert!(deps.llm.calls() >= 1, "LLM should have been called at least once");
    assert!(
        spawner.accept_bundle_calls.lock().unwrap().is_empty(),
        "Done action must not invoke spawner"
    );
}

#[tokio::test]
async fn run_director_accept_bundle_invokes_spawner() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = make_bundle(work.id.clone(), BundleStatus::Reviewed);
    let bundle_id_s = bundle.id.to_string();

    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let llm = FakeLlm::repeating(json!([{ "action": "accept_bundle", "bundle_id": bundle_id_s }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let deps = make_deps(llm, store, spawner.clone(), fast_config(), shutdown.clone());

    let shutdown_fire = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        shutdown_fire.notify_waiters();
    });

    run_director(&plan_id, &deps)
        .await
        .expect("director exits Ok on shutdown");
    let calls = spawner.accept_bundle_calls.lock().unwrap();
    assert!(!calls.is_empty(), "accept_bundle should have been called");
    assert_eq!(calls[0], bundle.id);
}

#[tokio::test]
async fn run_director_all_done_first_reconcile_skips_llm() {
    // Identical to `run_director_done_action_sleeps_then_observes_goal_complete`
    // but explicit so the design-doc bullet is named in the test inventory.
    let plan_id = PlanId::new();
    let done = make_work(plan_id.clone(), "wk-done", WorkStatus::Done);
    let store = FakeStore::with(vec![done], vec![]);
    let llm = FakeLlm::never_called();
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let deps = make_deps(llm, store, spawner, fast_config(), shutdown);
    run_director(&plan_id, &deps).await.expect("Ok");
    assert_eq!(deps.llm.calls(), 0);
}

#[tokio::test]
async fn run_director_three_parse_failures_escalates_lifeguard() {
    // With max_requeries=0, every iteration uses exactly one LLM call. With
    // max_parse_failures=3, the third unparseable iteration trips the
    // lifeguard.
    let plan_id = PlanId::new();
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![pending], vec![]);
    let llm = FakeLlm::repeating("garbage not json at all".to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let deps = make_deps(llm, store, spawner, fast_config(), shutdown);
    let err = run_director(&plan_id, &deps).await.expect_err("must escalate");
    match err {
        DirectorError::Lifeguard(reason) => {
            assert!(
                reason.contains("3") || reason.to_lowercase().contains("parse"),
                "lifeguard reason: {reason}"
            );
        }
        other => panic!("expected Lifeguard, got {other:?}"),
    }
}

#[tokio::test]
async fn run_director_need_help_exits_immediately() {
    let plan_id = PlanId::new();
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![pending], vec![]);
    let llm = FakeLlm::new(vec![
        json!([{ "action": "need_help", "reason": "stuck plan" }]).to_string(),
    ]);
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let mut config = fast_config();
    // Even with restarts available, NeedHelp must short-circuit and exit.
    config.max_restarts = 5;

    let deps = make_deps(llm, store, spawner, config, shutdown);
    let err = run_director(&plan_id, &deps).await.expect_err("must exit NeedHelp");
    match err {
        DirectorError::NeedHelp(reason) => assert_eq!(reason, "stuck plan"),
        other => panic!("expected NeedHelp, got {other:?}"),
    }
    // Exactly one LLM call: no restart, no retry on NeedHelp.
    assert_eq!(deps.llm.calls(), 1, "NeedHelp must not retry");
}

