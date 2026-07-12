#![allow(clippy::unwrap_used)]
//! Unit tests for the Director: parsing, state assembly, reconcile sweep,
//! and the full `run_director` loop with fake LLM, store, context, and
//! spawner.

use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio::sync::Notify;

use context::{AssembledContext, ContextBuilder, ContextError, DirectorState as CtxDirectorState};
use domain::{
    Bundle, BundleId, BundleStatus, NoteId, OperatorNote, Plan, PlanId, PlanStatus, Work, WorkId, WorkStatus,
    now_millis,
};
use llm::{LlmClient, LlmError, Message, ToolCall, ToolSchema as LlmToolSchema, Usage};
use store::StoreError;

use super::{
    DirectorAction, DirectorDeps, DirectorError, DirectorIterOutcome, DirectorSession, DirectorStore, WorkSpawner,
    build_director_state, parse_director_actions, reconcile_director, run_director,
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
fn parse_actions_wrapped_in_json_fence() {
    // Regression: parse_director_actions previously had no fence-strip,
    // unlike parse_actions/parse_verdict. A response wrapped in a
    // ```json fence - the same shape every other agent's LLM response
    // is wrapped in - failed to parse.
    let resp = format!(
        "```json\n{}\n```",
        json!([{ "action": "accept_bundle", "bundle_id": "bd-001" }])
    );
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::AcceptBundle {
            bundle_id: "bd-001".to_string()
        }]
    );
}

#[test]
fn parse_actions_wrapped_in_bare_fence() {
    let resp = format!("```\n{}\n```", json!([{ "action": "done", "summary": "ok" }]));
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "ok".to_string()
        }]
    );
}

#[test]
fn parse_actions_extracted_from_surrounding_prose() {
    // Regression: parse_director_actions previously had no
    // balanced-bracket extraction fallback, unlike parse_actions. A
    // response with prose the model "helpfully" added around the JSON
    // array failed to parse.
    let resp = format!(
        "Sure! Here's what I'll do: {} - let me know if that works.",
        json!([{ "action": "done", "summary": "ok" }])
    );
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "ok".to_string()
        }]
    );
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
    assert_eq!(cfg.max_work_attempts, 3);
    assert_eq!(cfg.reconcile_grace_secs, 30);
    assert_eq!(cfg.needs_operator_grace_iters, 5);
}

#[test]
fn director_config_yaml_round_trip_needs_operator_grace_iters() {
    // Phase 10 wiring: `agents.director.needs-operator-grace-iters: N`
    // must land on the typed field; an absent field must default to 5.
    let yaml = "needs-operator-grace-iters: 12\n";
    let cfg: crate::config::DirectorConfig = serde_yaml::from_str(yaml).expect("deserialize");
    assert_eq!(
        cfg.needs_operator_grace_iters, 12,
        "kebab-case wire form must populate the field"
    );

    let serialized = serde_yaml::to_string(&cfg).expect("serialize");
    assert!(
        serialized.contains("needs-operator-grace-iters: 12"),
        "serialized output must use kebab-case: {serialized}"
    );

    let empty: crate::config::DirectorConfig = serde_yaml::from_str("{}").expect("deserialize empty");
    assert_eq!(empty.needs_operator_grace_iters, 5, "missing field must default to 5");
}

#[test]
fn director_config_yaml_round_trip_reconcile_grace_secs() {
    // Phase 3 wiring: `agents.director.reconcile-grace-secs: N` must land
    // on the typed field; an absent field must default to 30 seconds.
    let yaml = "reconcile-grace-secs: 90\n";
    let cfg: crate::config::DirectorConfig = serde_yaml::from_str(yaml).expect("deserialize");
    assert_eq!(
        cfg.reconcile_grace_secs, 90,
        "kebab-case wire form must populate the field"
    );

    let serialized = serde_yaml::to_string(&cfg).expect("serialize");
    assert!(
        serialized.contains("reconcile-grace-secs: 90"),
        "serialized output must use kebab-case: {serialized}"
    );

    let empty: crate::config::DirectorConfig = serde_yaml::from_str("{}").expect("deserialize empty");
    assert_eq!(empty.reconcile_grace_secs, 30, "missing field must default to 30");
}

#[test]
fn director_config_yaml_round_trip_max_work_attempts() {
    // Pins the kebab-case wire form for `max_work_attempts` and the
    // serde-default fall-through. An operator's `.loopr/config.yml`
    // setting `agents.director.max-work-attempts: 7` must land on the
    // typed field; an absent field must default to 3.
    let yaml = "max-work-attempts: 7\n";
    let cfg: crate::config::DirectorConfig = serde_yaml::from_str(yaml).expect("deserialize");
    assert_eq!(cfg.max_work_attempts, 7, "kebab-case wire form must populate the field");

    let serialized = serde_yaml::to_string(&cfg).expect("serialize");
    assert!(
        serialized.contains("max-work-attempts: 7"),
        "serialized output must use kebab-case: {serialized}"
    );

    // Absent field falls back to Default::default() (= 3).
    let empty: crate::config::DirectorConfig = serde_yaml::from_str("{}").expect("deserialize empty");
    assert_eq!(empty.max_work_attempts, 3, "missing field must default to 3");
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
    /// Records the LAST message text (the current-iteration user
    /// prompt) seen on every `complete_free` call. Phase 6 tests
    /// assert that the user prompt's mode label changes after the
    /// pattern tracker fires.
    last_user_messages: Mutex<Vec<String>>,
    /// Optional `(notify, threshold)`: when `call_count` reaches
    /// `threshold`, the LLM fires `notify.notify_waiters()` from
    /// inside `complete_free` before returning. Used to drive
    /// deterministic shutdown in tests that previously used a wall-
    /// clock `tokio::spawn(sleep + notify)` and flaked under load.
    shutdown_after: Option<(Arc<Notify>, u32)>,
}

impl FakeLlm {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses),
            repeat_last: false,
            call_count: Mutex::new(0),
            last_user_messages: Mutex::new(Vec::new()),
            shutdown_after: None,
        }
    }

    fn repeating(response: String) -> Self {
        Self {
            responses: Mutex::new(vec![response]),
            repeat_last: true,
            call_count: Mutex::new(0),
            last_user_messages: Mutex::new(Vec::new()),
            shutdown_after: None,
        }
    }

    /// Empty queue + repeat_last=false => calling `complete_free` panics.
    /// Used by tests asserting that the loop must exit before any LLM call.
    fn never_called() -> Self {
        Self {
            responses: Mutex::new(Vec::new()),
            repeat_last: false,
            call_count: Mutex::new(0),
            last_user_messages: Mutex::new(Vec::new()),
            shutdown_after: None,
        }
    }

    /// Fire `notify` after the Nth `complete_free` call. The notify
    /// is sent BEFORE returning the Nth response, so the Director
    /// loop's next `select!` between the LLM-returned action and the
    /// shutdown Notify resolves deterministically without depending
    /// on a wall-clock timer.
    fn with_shutdown_after(mut self, notify: Arc<Notify>, calls: u32) -> Self {
        self.shutdown_after = Some((notify, calls));
        self
    }

    fn calls(&self) -> u32 {
        *self.call_count.lock().unwrap()
    }

    fn last_user_messages(&self) -> Vec<String> {
        self.last_user_messages.lock().unwrap().clone()
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
        messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let count = {
            let mut c = self.call_count.lock().unwrap();
            *c += 1;
            *c
        };
        if let Some(last) = messages.last() {
            // Extract the first text block; the Director's user message
            // is always a single Text content block by construction.
            for block in &last.content {
                if let llm::MessageContent::Text { text } = block {
                    self.last_user_messages.lock().unwrap().push(text.clone());
                    break;
                }
            }
        }
        let payload = {
            let mut q = self.responses.lock().unwrap();
            if q.len() > 1 {
                q.remove(0)
            } else if self.repeat_last {
                q[0].clone()
            } else if q.is_empty() {
                panic!("FakeLlm: response queue exhausted (no responses queued; LLM was not expected to be called)");
            } else {
                q.remove(0)
            }
        };
        // Fire deterministic shutdown after the configured Nth call.
        // MUST use `notify_one`, not `notify_waiters`: the Director isn't
        // currently parked on `shutdown.notified()` when we fire (it's
        // mid-iteration consuming our LLM response). `notify_waiters`
        // wakes only currently-parked waiters and drops the signal if
        // none — the Director would never see it. `notify_one` stores a
        // permit that the next `.notified()` call (the iteration's
        // step-7 sleep select!) consumes immediately, exiting the loop
        // deterministically.
        if let Some((notify, threshold)) = &self.shutdown_after
            && count >= *threshold
        {
            notify.notify_one();
        }
        Ok((payload, Usage::default()))
    }
}

/// Fake `DirectorStore` returning the works/bundles set at construction.
/// Holds an optional Plan so the Layer-2 retry-budget cap test can read +
/// write it; tests that don't exercise the cap leave it at the default.
#[derive(Default)]
struct FakeStore {
    works: Mutex<Vec<Work>>,
    bundles: Mutex<Vec<Bundle>>,
    plan: Mutex<Option<Plan>>,
    notes: Mutex<Vec<OperatorNote>>,
}

impl FakeStore {
    fn with(works: Vec<Work>, bundles: Vec<Bundle>) -> Self {
        Self {
            works: Mutex::new(works),
            bundles: Mutex::new(bundles),
            plan: Mutex::new(None),
            notes: Mutex::new(Vec::new()),
        }
    }

    fn with_plan(works: Vec<Work>, bundles: Vec<Bundle>, plan: Plan) -> Self {
        Self {
            works: Mutex::new(works),
            bundles: Mutex::new(bundles),
            plan: Mutex::new(Some(plan)),
            notes: Mutex::new(Vec::new()),
        }
    }

    fn plan_status(&self) -> Option<PlanStatus> {
        self.plan.lock().unwrap().as_ref().map(|p| p.status)
    }
}

impl DirectorStore for FakeStore {
    async fn list_works_for_plan(&self, _plan_id: &PlanId) -> Result<Vec<Work>, StoreError> {
        Ok(self.works.lock().unwrap().clone())
    }

    async fn list_bundles_for_plan(&self, _plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError> {
        Ok(self.bundles.lock().unwrap().clone())
    }

    async fn get_work(&self, work_id: &WorkId) -> Result<Work, StoreError> {
        self.works
            .lock()
            .unwrap()
            .iter()
            .find(|w| &w.id == work_id)
            .cloned()
            .ok_or(StoreError::RecordNotFound {
                collection: "works",
                id: work_id.to_string(),
            })
    }

    async fn get_plan(&self, plan_id: &PlanId) -> Result<Plan, StoreError> {
        self.plan.lock().unwrap().clone().ok_or(StoreError::RecordNotFound {
            collection: "plans",
            id: plan_id.to_string(),
        })
    }

    async fn update_plan(&self, plan: Plan, _expected_updated_at: i64) -> Result<(), StoreError> {
        *self.plan.lock().unwrap() = Some(plan);
        Ok(())
    }

    async fn list_unread_notes_for_plan(&self, _plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        let mut notes: Vec<OperatorNote> = self
            .notes
            .lock()
            .unwrap()
            .iter()
            .filter(|n| n.is_unread())
            .cloned()
            .collect();
        notes.sort_by_key(|n| n.created_at);
        Ok(notes)
    }

    async fn mark_notes_read(&self, note_ids: &[NoteId]) -> Result<(), StoreError> {
        let mut notes = self.notes.lock().unwrap();
        for n in notes.iter_mut() {
            if note_ids.contains(&n.id) && n.is_unread() {
                n.mark_read(now_millis());
            }
        }
        Ok(())
    }
}

/// Fake `WorkSpawner` that records every call so tests can assert on them.
/// Phase 2/3 stuck-state surface: `spawn_reviewer` and `spawn_integrator`
/// also record; `live_*_ids` are pre-populated by the test to simulate
/// the daemon's sidecar-map state at the moment reconcile reads it.
#[derive(Default)]
struct RecordingSpawner {
    accept_bundle_calls: Mutex<Vec<BundleId>>,
    override_work_calls: Mutex<Vec<(WorkId, WorkStatus, String)>>,
    assign_work_calls: Mutex<Vec<WorkId>>,
    spawn_reviewer_calls: Mutex<Vec<BundleId>>,
    spawn_integrator_calls: Mutex<Vec<BundleId>>,
    recover_in_progress_calls: Mutex<Vec<(WorkId, String)>>,
    live_work_ids: Mutex<Vec<WorkId>>,
    live_reviewer_bundle_ids: Mutex<Vec<BundleId>>,
    live_integrator_bundle_ids: Mutex<Vec<BundleId>>,
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

    fn spawn_reviewer(&self, bundle_id: BundleId) {
        self.spawn_reviewer_calls.lock().unwrap().push(bundle_id);
    }

    fn spawn_integrator(&self, bundle_id: BundleId) {
        self.spawn_integrator_calls.lock().unwrap().push(bundle_id);
    }

    fn recover_in_progress_work(&self, work_id: WorkId, reason: String) {
        self.recover_in_progress_calls.lock().unwrap().push((work_id, reason));
    }

    fn list_running_work_ids(&self) -> Vec<WorkId> {
        self.live_work_ids.lock().unwrap().clone()
    }

    fn list_running_reviewer_bundle_ids(&self) -> Vec<BundleId> {
        self.live_reviewer_bundle_ids.lock().unwrap().clone()
    }

    fn list_running_integrator_bundle_ids(&self) -> Vec<BundleId> {
        self.live_integrator_bundle_ids.lock().unwrap().clone()
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
        // Phase 6: surface the mode label so tests can assert the
        // pattern tracker / mode FSM transitioned without poking
        // run_director_inner internals. The production user.pmt
        // renders `**Director mode:** {{mode}}`; this stub matches
        // the label literal so a contains() check works on either.
        let mut user = format!(
            "plan={} **Director mode:** {} works={} bundles={}",
            state.plan_id,
            if state.mode.is_empty() { "Normal" } else { &state.mode },
            state.works.len(),
            state.bundles.len()
        );
        // Phase 9: mirror the production user.pmt's `### Operator
        // Notes` section so tests asserting on the rendered notes work
        // against the same shape they would see in production.
        if !state.operator_notes.is_empty() {
            user.push_str("\n\n### Operator Notes\n");
            for n in &state.operator_notes {
                user.push_str("- ");
                user.push_str(n);
                user.push('\n');
            }
        }
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
        operator_notify: Arc::new(Notify::new()),
        director_statuses: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
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

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
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

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
    assert!(goal_complete);
    assert!(spawner.override_work_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reconcile_zero_works_returns_false() {
    let plan_id = PlanId::new();
    let store = FakeStore::with(vec![], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let goal_complete = reconcile_director(&plan_id, &store, &spawner, 0).await.expect("ok");
    assert!(!goal_complete, "zero works is not GoalComplete");
}

// ---------------------------------------------------------------------------
// Phase 3 stuck-state recovery: Triaged-no-Reviewer, Accepted-no-Integrator,
// InProgress-no-Implementer. Each case has three permutations:
//   * past grace AND no live task -> recovery fires
//   * within grace -> recovery skipped regardless of live state
//   * past grace AND live task -> recovery skipped (sidecar map covers it)
//
// Grace is `grace_ms` parameter; tests use 30_000 (the default 30s) or 0
// depending on what they want to assert. `updated_at` is mutated post-
// construction; the helpers themselves don't expose it.
// ---------------------------------------------------------------------------

const PAST_GRACE: i64 = 60_000;

fn aged_work(plan_id: PlanId, title: &str, status: WorkStatus, age_ms: i64) -> Work {
    let mut w = make_work(plan_id, title, status);
    w.updated_at = now_millis() - age_ms;
    w
}

fn aged_bundle(work_id: WorkId, status: BundleStatus, age_ms: i64) -> Bundle {
    let mut b = make_bundle(work_id, status);
    b.updated_at = now_millis() - age_ms;
    b
}

#[tokio::test]
async fn reconcile_stuck_triaged_bundle_respawns_reviewer() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    // live_reviewer_bundle_ids stays empty -> bundle is stuck.

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.spawn_reviewer_calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![bundle.id],
        "stuck Triaged Bundle must trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_triaged_bundle_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    // age=0 -> fresh, well inside any grace window > 0.
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, 0);
    let store = FakeStore::with(vec![work], vec![bundle]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_reviewer_calls.lock().unwrap().is_empty(),
        "Triaged Bundle within grace window must NOT trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_triaged_bundle_with_live_reviewer_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Triaged, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_reviewer_bundle_ids.lock().unwrap().push(bundle.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_reviewer_calls.lock().unwrap().is_empty(),
        "Triaged Bundle WITH a live Reviewer must NOT trigger spawn_reviewer"
    );
}

#[tokio::test]
async fn reconcile_stuck_accepted_bundle_spawns_integrator() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.spawn_integrator_calls.lock().unwrap();
    assert_eq!(
        *calls,
        vec![bundle.id],
        "stuck Accepted Bundle must trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_accepted_bundle_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, 0);
    let store = FakeStore::with(vec![work], vec![bundle]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_integrator_calls.lock().unwrap().is_empty(),
        "Accepted Bundle within grace window must NOT trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_accepted_bundle_with_live_integrator_is_skipped() {
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = aged_bundle(work.id.clone(), BundleStatus::Accepted, PAST_GRACE);
    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_integrator_bundle_ids.lock().unwrap().push(bundle.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.spawn_integrator_calls.lock().unwrap().is_empty(),
        "Accepted Bundle WITH a live Integrator must NOT trigger spawn_integrator"
    );
}

#[tokio::test]
async fn reconcile_stuck_in_progress_work_recovers_via_reactor_role() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, PAST_GRACE);
    let store = FakeStore::with(vec![work.clone()], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    let calls = spawner.recover_in_progress_calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "stuck InProgress Work must trigger one recover_in_progress_work call"
    );
    assert_eq!(calls[0].0, work.id);
    assert!(
        calls[0].1.contains("no live Implementer"),
        "reason must explain the recovery: {}",
        calls[0].1
    );

    // The Director-role `override_work` path must NOT fire for InProgress
    // recovery (the override table is Reactor-only for InProgress -> Ready).
    assert!(
        spawner.override_work_calls.lock().unwrap().is_empty(),
        "InProgress recovery must NOT route through override_work (Director role)"
    );
}

#[tokio::test]
async fn reconcile_in_progress_work_within_grace_window_is_skipped() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, 0);
    let store = FakeStore::with(vec![work], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.recover_in_progress_calls.lock().unwrap().is_empty(),
        "InProgress Work within grace window must NOT trigger recover_in_progress_work"
    );
}

#[tokio::test]
async fn reconcile_in_progress_work_with_live_implementer_is_skipped() {
    let plan_id = PlanId::new();
    let work = aged_work(plan_id.clone(), "wk-1", WorkStatus::InProgress, PAST_GRACE);
    let store = FakeStore::with(vec![work.clone()], vec![]);
    let spawner = Arc::new(RecordingSpawner::default());
    spawner.live_work_ids.lock().unwrap().push(work.id);

    let _ = reconcile_director(&plan_id, &store, &spawner, 30_000)
        .await
        .expect("ok");

    assert!(
        spawner.recover_in_progress_calls.lock().unwrap().is_empty(),
        "InProgress Work WITH a live Implementer must NOT trigger recover_in_progress_work"
    );
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
async fn run_director_done_action_does_not_terminate_loop() {
    // Pending Work + LLM emits {action: done}. The `done` action is a
    // no-op at the iteration boundary: it must NOT invoke the spawner
    // AND it must NOT terminate the loop. The latter is captured by
    // asserting `run_once` returns `Continue`, not `GoalDone`. Single-
    // iteration assertion via DirectorSession::run_once — no shutdown
    // plumbing required.
    let plan_id = PlanId::new();
    let pending = make_work(plan_id.clone(), "wk-pending", WorkStatus::Pending);
    let store = FakeStore::with(vec![pending], vec![]);
    let llm = FakeLlm::repeating(json!([{ "action": "done", "summary": "no-op" }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let deps = make_deps(llm, store, spawner.clone(), fast_config(), Arc::new(Notify::new()));

    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    let outcome = session.run_once(&deps).await.expect("run_once Ok");

    assert_eq!(
        outcome,
        DirectorIterOutcome::Continue { took_action: false },
        "done must surface as a non-terminating iteration"
    );
    assert!(deps.llm.calls() >= 1, "LLM should have been called at least once");
    assert!(
        spawner.accept_bundle_calls.lock().unwrap().is_empty(),
        "Done action must not invoke spawner"
    );
}

#[tokio::test]
async fn run_director_accept_bundle_invokes_spawner() {
    // Single-iteration assertion: one Director iteration must route the
    // LLM's accept_bundle action into the spawner. Uses DirectorSession::
    // run_once directly so there is no shutdown plumbing for this test to
    // forget — the previous repeated-CI-hang failure mode.
    let plan_id = PlanId::new();
    let work = make_work(plan_id.clone(), "wk-1", WorkStatus::InReview);
    let bundle = make_bundle(work.id.clone(), BundleStatus::Reviewed);
    let bundle_id_s = bundle.id.to_string();

    let store = FakeStore::with(vec![work], vec![bundle.clone()]);
    let llm = FakeLlm::repeating(json!([{ "action": "accept_bundle", "bundle_id": bundle_id_s }]).to_string());
    let spawner = Arc::new(RecordingSpawner::default());
    let deps = make_deps(llm, store, spawner.clone(), fast_config(), Arc::new(Notify::new()));

    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

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
    let mut plan = Plan::new("need-help-test".to_string());
    plan.id = plan_id.clone();
    let store = FakeStore::with_plan(vec![pending], vec![], plan);
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
    // Bullet 6: an LLM-emitted need_help persists Plan -> Stalled before
    // returning, so the daemon doesn't show "not running (plan is Active)".
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Stalled),
        "need_help must stall the Plan before exiting"
    );
}

// ---------------------------------------------------------------------------
// Layer 2 (Director-layer) retry-budget cap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn override_to_ready_below_cap_dispatches_through_spawner() {
    // attempt_count=2, cap=3 (1-based): 2 < 3, so the cap is NOT tripped
    // and the override falls through to `spawner.override_work`.
    let plan_id = PlanId::new();
    let mut work = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    work.attempt_count = 2;
    let work_id_s = work.id.to_string();
    let plan = Plan::new("retry-budget-test".to_string());

    let store = FakeStore::with_plan(vec![work.clone()], vec![], plan);
    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());

    let mut config = fast_config();
    config.max_work_attempts = 3;
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

    let calls = spawner.override_work_calls.lock().unwrap();
    assert!(!calls.is_empty(), "below-cap override must reach spawner");
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Active),
        "Plan must NOT be Stalled below cap"
    );
}

#[tokio::test]
async fn override_to_ready_at_cap_stalls_plan_and_returns_need_help() {
    // attempt_count=3, cap=3: 3 >= 3 trips the cap. Plan must transition
    // to Stalled and `run_director` must return NeedHelp without invoking
    // the spawner.
    let plan_id = PlanId::new();
    let mut work = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    work.attempt_count = 3;
    let work_id_s = work.id.to_string();
    let plan = Plan::new("retry-budget-exhausted".to_string());

    let store = FakeStore::with_plan(vec![work.clone()], vec![], plan);
    let llm = FakeLlm::new(vec![
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    ]);
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let mut config = fast_config();
    config.max_work_attempts = 3;
    let deps = make_deps(llm, store, spawner.clone(), config, shutdown);

    let err = run_director(&plan_id, &deps).await.expect_err("must exit NeedHelp");
    match err {
        DirectorError::NeedHelp(reason) => {
            assert!(reason.contains("retry budget exhausted"), "NeedHelp reason: {reason}")
        }
        other => panic!("expected NeedHelp, got {other:?}"),
    }
    assert!(
        spawner.override_work_calls.lock().unwrap().is_empty(),
        "spawner.override_work MUST NOT be called when cap trips"
    );
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Stalled),
        "Plan must transition to Stalled BEFORE NeedHelp returns"
    );
}

#[tokio::test]
async fn override_to_non_ready_skips_cap_check() {
    // Cap only fires on `target == Ready`; an override to Blocked must
    // dispatch through the spawner regardless of attempt_count.
    let plan_id = PlanId::new();
    let mut work = make_work(plan_id.clone(), "wk-1", WorkStatus::InProgress);
    work.attempt_count = 99;
    let work_id_s = work.id.to_string();
    let plan = Plan::new("non-ready-target".to_string());

    let store = FakeStore::with_plan(vec![work.clone()], vec![], plan);
    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Blocked",
            "reason": "stuck on dep"
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());

    let mut config = fast_config();
    config.max_work_attempts = 3;
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

    let calls = spawner.override_work_calls.lock().unwrap();
    assert!(!calls.is_empty(), "non-Ready override must reach spawner");
    assert!(calls.iter().all(|(_, status, _)| *status == WorkStatus::Blocked));
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Active),
        "Plan must NOT be touched"
    );
}

#[tokio::test]
async fn override_to_ready_with_zero_cap_trips_on_first_attempt() {
    // cap=0: any attempt_count (>= 0) trips. Operator-tunable corner
    // case — a config that disables retries entirely must reject the
    // very first Director-issued override -> Ready and Stall the Plan.
    let plan_id = PlanId::new();
    let mut work = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    work.attempt_count = 0;
    let work_id_s = work.id.to_string();
    let plan = Plan::new("zero-cap".to_string());

    let store = FakeStore::with_plan(vec![work.clone()], vec![], plan);
    let llm = FakeLlm::new(vec![
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    ]);
    let spawner = Arc::new(RecordingSpawner::default());
    let shutdown = Arc::new(Notify::new());

    let mut config = fast_config();
    config.max_work_attempts = 0;
    let deps = make_deps(llm, store, spawner.clone(), config, shutdown);

    let err = run_director(&plan_id, &deps).await.expect_err("must exit NeedHelp");
    assert!(
        matches!(err, DirectorError::NeedHelp(_)),
        "expected NeedHelp, got {err:?}"
    );
    assert!(
        spawner.override_work_calls.lock().unwrap().is_empty(),
        "cap=0 must refuse the very first Ready override"
    );
    assert_eq!(deps.store.plan_status(), Some(PlanStatus::Stalled));
}

#[tokio::test]
async fn override_to_ready_with_high_cap_below_attempt_dispatches() {
    // cap=5, attempt_count=4: 4 < 5, below the cap. The override falls
    // through to the spawner and the Plan stays Active. Mirror of the
    // `at_cap_stalls` test for an operator-raised soft cap.
    let plan_id = PlanId::new();
    let mut work = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    work.attempt_count = 4;
    let work_id_s = work.id.to_string();
    let plan = Plan::new("high-cap".to_string());

    let store = FakeStore::with_plan(vec![work.clone()], vec![], plan);
    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    );
    let spawner = Arc::new(RecordingSpawner::default());

    let mut config = fast_config();
    config.max_work_attempts = 5;
    let deps = make_deps(llm, store, spawner.clone(), config, Arc::new(Notify::new()));

    let mut session = DirectorSession::new(plan_id.clone(), &deps.config);
    session.run_once(&deps).await.expect("run_once Ok");

    assert!(
        !spawner.override_work_calls.lock().unwrap().is_empty(),
        "cap=5 with attempt=4 must NOT trip"
    );
    assert_eq!(
        deps.store.plan_status(),
        Some(PlanStatus::Active),
        "Plan must NOT be Stalled when below the (raised) cap"
    );
}

// ---------------------------------------------------------------------------
// Phase 6: user-prompt mode label propagation
//
// The pattern tracker observes the LLM's actions and the Plan state hash
// every iteration; once `same_action_threshold` consecutive identical
// fingerprints arrive, `next_mode` promotes Normal -> Conservative and
// the next iteration's user prompt carries the new label. This is the
// end-to-end observable that the Phase 5 FSM is wired into the loop.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_director_repeated_override_promotes_mode_to_conservative() {
    let plan_id = PlanId::new();
    let blocked = make_work(plan_id.clone(), "wk-1", WorkStatus::Blocked);
    let work_id_s = blocked.id.to_string();
    let store = FakeStore::with(vec![blocked], vec![]);
    let shutdown = Arc::new(Notify::new());

    // Deterministic shutdown: fire after the LLM's 4th call. The Director
    // FSM transitions Normal -> Conservative on the 3rd identical override
    // (same_action_threshold=3); the 4th iteration's user prompt is the
    // one this test asserts carries the Conservative label. Firing
    // shutdown after the 4th LLM call guarantees we observe both the
    // pre-transition prompt (iteration 1, Normal) and the post-transition
    // prompt (iteration 4, Conservative). The earlier `tokio::spawn(sleep
    // 150ms + notify)` version flaked under parallel test load because
    // the timer task could be starved.
    let llm = FakeLlm::repeating(
        json!([{
            "action": "override_work",
            "work_id": work_id_s,
            "target_status": "Ready",
            "reason": "retry"
        }])
        .to_string(),
    )
    .with_shutdown_after(shutdown.clone(), 4);
    let spawner = Arc::new(RecordingSpawner::default());

    // Tight pattern config so the FSM transitions within a handful of
    // iterations. `same_action_threshold=3` means the 3rd identical
    // override fires SameActionTripped on iteration 3; iteration 4's
    // user prompt should carry the Conservative label.
    let mut config = fast_config();
    config.patterns = crate::config::DirectorConfig::default().patterns;
    config.patterns.same_action_threshold = 3;
    // Raise the retry budget so the Layer-2 cap doesn't preempt the
    // pattern tracker; we want at least 4 iterations of overrides.
    config.max_work_attempts = 100;
    let deps = make_deps(llm, store, spawner.clone(), config, shutdown.clone());

    run_director(&plan_id, &deps).await.expect("Ok on shutdown");

    let prompts = deps.llm.last_user_messages();
    assert!(
        prompts.len() >= 4,
        "expected at least 4 LLM calls; got {}",
        prompts.len()
    );
    assert!(
        prompts.iter().any(|p| p.contains("**Director mode:** Conservative")),
        "after SameActionTripped, the user prompt must carry Conservative; got prompts: {prompts:?}"
    );
    // Earliest prompt is still Normal — the FSM only transitions AFTER
    // the tracker observes >= same_action_threshold matching iterations.
    assert!(
        prompts[0].contains("**Director mode:** Normal"),
        "first iteration must show Normal mode; got: {}",
        prompts[0]
    );
}

// Phase 9-10 tests (operator-note path + NeedsOperator -> Stalled
// grace) live in the `operator` submodule to keep this file under the
// 1500-line bloat-task cap.
mod operator;

// Phase 2 restart-budget-reset regression lives in its own submodule,
// same reason (keeps this file under the bloat-task cap).
mod restart;
