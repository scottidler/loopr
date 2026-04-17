//! Unit tests for the Director agent (Phase 2 run loop).
//!
//! These tests exercise the event-driven loop without real LLM calls. The LLM trait is
//! stubbed via `NoopLlm`. Broadcast + mpsc channels are constructed directly so tests can
//! inject events and user messages deterministically.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use eyre::Result;
use tokio::sync::{broadcast, mpsc};

use crate::agents::bridge::AgentIpcBridge;
use crate::agents::cache::ReadCache;
use crate::agents::director::{DirectorAgent, DirectorMode, DirectorPatternTracker};
use crate::agents::implementer::LlmClient;
use crate::agents::{Agent, AgentContext, AgentKind, AgentSession};
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::plan::{HierarchyStatus, Plan};
use crate::ipc::protocol::DaemonEvent;
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

/// Stubs the LlmClient trait. Returns a fixed response.
struct NoopLlm;

impl LlmClient for NoopLlm {
    fn call<'a>(
        &'a self,
        _system_prompt: &'a str,
        _user_message: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
        async move { Ok("stubbed".to_string()) }
    }
}

/// Returns a canned response on every `call`. Used to exercise escalation's
/// parse-and-execute path without a real LLM.
struct ScriptedLlm {
    response: String,
}

impl LlmClient for ScriptedLlm {
    fn call<'a>(
        &'a self,
        _system_prompt: &'a str,
        _user_message: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
        let r = self.response.clone();
        async move { Ok(r) }
    }
}

/// Always fails, to verify the escalation path tolerates LLM outages.
struct FailingLlm;

impl LlmClient for FailingLlm {
    fn call<'a>(
        &'a self,
        _system_prompt: &'a str,
        _user_message: &'a str,
    ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
        async move { Err(eyre::eyre!("simulated LLM failure")) }
    }
}

fn test_stores_arc(dir: &TestDir) -> Arc<Stores> {
    use crate::config::{Config, ProjectConfig};
    let config = Config {
        project: ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..ProjectConfig::default()
        },
        ..Config::default()
    };
    let mut stores = Stores::new();
    stores.config = config;
    Arc::new(stores)
}

fn director_ctx(
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    user_message_rx: Option<mpsc::Receiver<String>>,
    target_id: Option<String>,
) -> (AgentContext, String) {
    let mut session = AgentSession::new(AgentKind::Director, "test-model".into());
    session.target_id = target_id;
    let session_id = session.id.clone();
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session_id.clone(), session.clone());

    let worktree_mgr = WorktreeManager::new(
        stores.config.project.repo_path.clone(),
        stores.config.project.repo_path.join(".worktrees"),
    );
    let bridge = AgentIpcBridge::new(
        stores.clone(),
        event_tx.clone(),
        worktree_mgr,
        stores.config.clone(),
        stores.fsm.clone(),
    );

    let ctx = AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx: event_tx.clone(),
        event_rx: Some(event_tx.subscribe()),
        user_message_rx,
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        read_cache: std::sync::Mutex::new(ReadCache::default()),
    };
    (ctx, session_id)
}

fn make_director(ctx: AgentContext) -> DirectorAgent<NoopLlm> {
    DirectorAgent::new(ctx, NoopLlm, AgentRoleConfig::default_director())
}

fn make_scripted_director(ctx: AgentContext, response: impl Into<String>) -> DirectorAgent<ScriptedLlm> {
    DirectorAgent::new(
        ctx,
        ScriptedLlm {
            response: response.into(),
        },
        AgentRoleConfig::default_director(),
    )
}

fn make_failing_director(ctx: AgentContext) -> DirectorAgent<FailingLlm> {
    DirectorAgent::new(ctx, FailingLlm, AgentRoleConfig::default_director())
}

// ─── DirectorPatternTracker ────────────────────────────────────────────────

#[test]
fn pattern_tracker_default_empty() {
    let t = DirectorPatternTracker::new();
    assert_eq!(t.failure_count("wk-1"), 0);
    assert_eq!(t.rejection_count("wk-1"), 0);
}

#[test]
fn pattern_tracker_clear_resets_all() {
    let mut t = DirectorPatternTracker::new();
    t.work_failure_history
        .insert("wk-1".into(), vec![("ag-1".into(), "err".into())]);
    t.rejection_history.insert("wk-2".into(), vec!["stale".into()]);
    t.spec_revision_count.insert("sp-1".into(), 3);

    t.clear();

    assert!(t.work_failure_history.is_empty());
    assert!(t.rejection_history.is_empty());
    assert!(t.spec_revision_count.is_empty());
}

// ─── Mode dispatch ─────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_intake_event_loop_exits_on_cancel() {
    let dir = TestDir::new("director-plan-intake-cancel");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, None);
    let mut agent = make_director(ctx);

    // Spawn the agent and then cancel the session; the loop must observe the cancel and exit.
    let cancel_stores = stores.clone();
    let cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut sessions = cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    // Use a test timeout so a regression that loses the cancel signal fails fast.
    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok(), "Director must exit within 5s after cancel");
    result.unwrap().expect("run returned Err");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitoring_plan_terminal_exits_loop() {
    let dir = TestDir::new("director-monitoring-terminal");
    let stores = test_stores_arc(&dir);

    // Seed a Plan in Complete status.
    let mut plan = Plan::new("done-plan".into(), AcceptanceCriteria::default());
    plan.force_status(HierarchyStatus::Complete);
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_director(ctx);

    // Director detects the plan is terminal on the first iteration and exits immediately.
    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(
        result.is_ok(),
        "Director must exit quickly when plan is terminal at start"
    );
    result.unwrap().unwrap();
    assert_eq!(agent.ctx.session.director_mode, None); // sessions may not be updated post-exit in this short path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_accepted_event_transitions_to_monitoring() {
    let dir = TestDir::new("director-plan-accepted-transition");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, None);
    let mut agent = make_director(ctx);

    // Seed a non-terminal plan so the loop keeps running after transitioning.
    let plan = Plan::new("active-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    // Drive the loop: emit plan_accepted, then cancel the session so the loop exits.
    let stores_for_driver = stores.clone();
    let session_for_driver = session_id.clone();
    let event_tx_for_driver = event_tx.clone();
    let plan_id_for_driver = plan_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = event_tx_for_driver.send(DaemonEvent::new(
            "doc.plan_accepted",
            serde_json::json!({ "plan_id": plan_id_for_driver }),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sessions = stores_for_driver.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&session_for_driver) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok(), "Director must complete");
    result.unwrap().unwrap();

    // After running, the session's director_mode should be Monitoring (persisted during process_event).
    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "doc.plan_accepted must transition PlanIntake → Monitoring"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_message_is_received() {
    let dir = TestDir::new("director-user-message");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    let (tx_msg, rx_msg) = mpsc::channel::<String>(8);
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), Some(rx_msg), None);
    let mut agent = make_director(ctx);

    let driver_stores = stores.clone();
    let driver_session = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx_msg.send("hello".to_string()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sessions = driver_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_session) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();
    // Phase 2 only logs user messages; successful receipt is covered by the loop not panicking.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_escalation_short_circuits_event_loop() {
    // Legacy-style escalation (non-pl-* target_id) now runs one `enter_escalation` pass
    // via the shared Phase 5 path. A FailingLlm keeps the run short without touching IPC.
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-legacy-escalation");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some("wk-x1".into()));
    let mut agent = make_failing_director(ctx);

    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(result.is_ok(), "escalation-mode run must complete quickly");
    result.unwrap().unwrap();
}

// ─── Phase 4: Monitoring intelligence ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_change_emits_director_mode_changed_event() {
    let dir = TestDir::new("director-mode-change-event");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(32);
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, None);
    let mut agent = make_director(ctx);

    // Seed a non-terminal plan so Monitoring keeps the loop alive.
    let plan = Plan::new("to-monitor".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let driver_stores = stores.clone();
    let driver_session = session_id.clone();
    let driver_tx = event_tx.clone();
    let driver_pid = plan_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let _ = driver_tx.send(DaemonEvent::new(
            "doc.plan_accepted",
            serde_json::json!({ "plan_id": driver_pid }),
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sessions = driver_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_session) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // Drain the receiver and verify director.mode_changed was emitted with mode="monitoring".
    let mut saw_mode_changed = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.mode_changed"
            && ev.data.get("mode").and_then(|v| v.as_str()) == Some("monitoring")
            && ev.data.get("session_id").and_then(|v| v.as_str()) == Some(session_id.as_str())
        {
            saw_mode_changed = true;
            break;
        }
    }
    assert!(
        saw_mode_changed,
        "director.mode_changed event must fire on PlanIntake → Monitoring"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_threshold_flips_mode_to_escalation() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-failure-threshold");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(64);

    let plan = Plan::new("threshold-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut impl_session = AgentSession::new(AgentKind::Implementer, "test-model".into());
    impl_session.work_id = Some("wk-threshold".into());
    let impl_sid = impl_session.id.clone();
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(impl_sid.clone(), impl_session);

    // FailingLlm keeps enter_escalation from hitting real IPC - the test only needs to
    // observe that Escalation was entered (via the mode_changed event), not that actions
    // ran. After the pass the loop flips back to Monitoring (that's Phase 5's return path).
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_failing_director(ctx);

    let driver_tx = event_tx.clone();
    let driver_sid = impl_sid.clone();
    let driver_cancel_stores = stores.clone();
    let driver_cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for _ in 0..3 {
            let _ = driver_tx.send(DaemonEvent::new(
                "agent.status_changed",
                serde_json::json!({
                    "session_id": driver_sid,
                    "status": "failed",
                    "error": "compile error",
                }),
            ));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut sessions = driver_cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // Threshold breach must have emitted mode_changed(escalation) at some point.
    let mut saw_escalation = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.mode_changed" && ev.data.get("mode").and_then(|v| v.as_str()) == Some("escalation") {
            saw_escalation = true;
        }
    }
    assert!(
        saw_escalation,
        "threshold breach must emit director.mode_changed with mode=escalation"
    );
    // Final mode returns to Monitoring after the escalation pass.
    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "Director must return to Monitoring after Escalation pass"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejection_threshold_flips_mode_to_escalation() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-rejection-threshold");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(64);

    let plan = Plan::new("rej-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_failing_director(ctx);

    let driver_tx = event_tx.clone();
    let driver_cancel_stores = stores.clone();
    let driver_cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for _ in 0..3 {
            let _ = driver_tx.send(DaemonEvent::new(
                "bundle.rejected",
                serde_json::json!({
                    "bundle_work_id": "wk-rej",
                    "reason": "diff-too-large",
                }),
            ));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut sessions = driver_cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    let mut saw_escalation = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.mode_changed" && ev.data.get("mode").and_then(|v| v.as_str()) == Some("escalation") {
            saw_escalation = true;
        }
    }
    assert!(
        saw_escalation,
        "three bundle rejections must emit director.mode_changed mode=escalation"
    );
    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "Director must return to Monitoring after Escalation pass"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_threshold_stays_in_monitoring() {
    let dir = TestDir::new("director-below-threshold");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(32);

    let plan = Plan::new("below-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut impl_session = AgentSession::new(AgentKind::Implementer, "test-model".into());
    impl_session.work_id = Some("wk-below".into());
    let impl_sid = impl_session.id.clone();
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(impl_sid.clone(), impl_session);

    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_director(ctx);

    let driver_tx = event_tx.clone();
    let driver_sid = impl_sid.clone();
    let driver_cancel_stores = stores.clone();
    let driver_cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Only 2 failures; threshold is 3, so mode should stay Monitoring.
        for _ in 0..2 {
            let _ = driver_tx.send(DaemonEvent::new(
                "agent.status_changed",
                serde_json::json!({
                    "session_id": driver_sid,
                    "status": "failed",
                    "error": "compile error",
                }),
            ));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sessions = driver_cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "two failures must not trigger the 3-failure threshold"
    );
}

// ─── Phase 5: Action parsing ───────────────────────────────────────────────

#[test]
fn parse_actions_accepts_actions_object_shape() {
    use crate::agents::director::actions::{DirectorAction, parse_actions};
    let payload = r#"{
        "actions": [
            {"type": "revise-work", "work_id": "wk-1", "acceptance_criteria": ["a", "b"]},
            {"type": "abandon-work", "work_id": "wk-2", "reason": "unfixable"},
            {"type": "spawn-researcher", "query": "why does lint fail", "scope": "plan"},
            {"type": "message-user", "text": "need a human"}
        ]
    }"#;
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 4);
    match &actions[0] {
        DirectorAction::ReviseWork {
            work_id,
            acceptance_criteria,
            title: _,
        } => {
            assert_eq!(work_id, "wk-1");
            assert_eq!(acceptance_criteria.as_ref().unwrap().len(), 2);
        }
        other => panic!("expected ReviseWork, got {:?}", other),
    }
    assert!(matches!(actions[1], DirectorAction::AbandonWork { .. }));
    assert!(matches!(actions[2], DirectorAction::SpawnResearcher { .. }));
    assert!(matches!(actions[3], DirectorAction::MessageUser { .. }));
}

#[test]
fn parse_actions_accepts_bare_array_shape() {
    use crate::agents::director::actions::parse_actions;
    let payload = r#"[{"type": "message-user", "text": "hello"}]"#;
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn parse_actions_strips_markdown_fence() {
    use crate::agents::director::actions::parse_actions;
    let payload =
        "Here's what I found:\n\n```json\n{\"actions\": [{\"type\": \"message-user\", \"text\": \"ok\"}]}\n```\n";
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 1);
}

#[test]
fn parse_actions_skips_unknown_types() {
    use crate::agents::director::actions::parse_actions;
    let payload = r#"{"actions": [
        {"type": "message-user", "text": "ok"},
        {"type": "unknown-action", "data": 123}
    ]}"#;
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 1, "unknown types must be skipped, not error");
}

#[test]
fn parse_actions_errors_on_non_json() {
    use crate::agents::director::actions::parse_actions;
    assert!(parse_actions("this is not json").is_err());
}

// ─── Phase 5: Escalation flow ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalation_emits_diagnosis_and_action_events() {
    // Escalation reads `prompts::store().director`. init_defaults is idempotent.
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-escalation-message-user");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(64);

    // Scripted LLM response that contains a single message-user action, which doesn't
    // require IPC state and so it's the simplest to exercise end-to-end.
    let response = r#"{"actions": [{"type": "message-user", "text": "something is stuck"}]}"#;
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some("wk-direct".into()));
    let mut agent = make_scripted_director(ctx, response);

    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    let mut saw_diagnosis = false;
    let mut saw_action_taken = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.diagnosis"
            && ev.data.get("text").and_then(|v| v.as_str()) == Some("something is stuck")
        {
            saw_diagnosis = true;
        }
        if ev.event == "director.action_taken" && ev.data.get("action").and_then(|v| v.as_str()) == Some("message-user")
        {
            saw_action_taken = true;
        }
    }
    assert!(saw_diagnosis, "message-user must emit director.diagnosis");
    assert!(saw_action_taken, "every action must emit director.action_taken");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalation_tolerates_unparseable_llm_response() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-escalation-bad-json");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some("wk-bad".into()));
    let mut agent = make_scripted_director(ctx, "not json at all");

    // Parser failure must not crash the agent; it completes and returns Ok.
    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threshold_escalation_returns_to_monitoring() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-escalation-return");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(64);

    // Seed plan + implementer session tied to a work_id.
    let plan = Plan::new("esc-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut impl_session = AgentSession::new(AgentKind::Implementer, "test-model".into());
    impl_session.work_id = Some("wk-return".into());
    let impl_sid = impl_session.id.clone();
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(impl_sid.clone(), impl_session);

    // Director uses a message-user escalation so we don't need the FSM to accept a
    // transition. The post-handler hook flips us back to Monitoring after the pass.
    let response = r#"{"actions": [{"type": "message-user", "text": "try a researcher"}]}"#;
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_scripted_director(ctx, response);

    let driver_tx = event_tx.clone();
    let driver_sid = impl_sid.clone();
    let driver_cancel_stores = stores.clone();
    let driver_cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for _ in 0..3 {
            let _ = driver_tx.send(DaemonEvent::new(
                "agent.status_changed",
                serde_json::json!({
                    "session_id": driver_sid,
                    "status": "failed",
                    "error": "compile error",
                }),
            ));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mut sessions = driver_cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // The final persisted mode should be Monitoring - Escalation runs then flips back.
    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "Director must return to Monitoring after Escalation completes"
    );
    drop(sessions);

    // Order check: mode_changed(escalation) must precede mode_changed(monitoring-second-time).
    let mut mode_sequence: Vec<String> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.mode_changed"
            && let Some(m) = ev.data.get("mode").and_then(|v| v.as_str())
        {
            mode_sequence.push(m.to_string());
        }
    }
    assert!(
        mode_sequence.contains(&"escalation".to_string()),
        "escalation mode_changed must fire (sequence: {:?})",
        mode_sequence
    );
    assert_eq!(
        mode_sequence.last().map(|s| s.as_str()),
        Some("monitoring"),
        "final mode_changed must be back to monitoring (sequence: {:?})",
        mode_sequence
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_scoped_target_id_enters_monitoring_with_plan_id() {
    let dir = TestDir::new("director-plan-target-id");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);

    // Seed a non-terminal plan.
    let plan = Plan::new("supervised".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_director(ctx);

    // Drive a cancel soon so the loop exits.
    let cancel_stores = stores.clone();
    let cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let mut sessions = cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(3), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "pl-* target_id must enter Monitoring directly"
    );
}
