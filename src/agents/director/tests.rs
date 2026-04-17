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
    t.observe_abandonment("sp-9", "wk-99");

    t.clear();

    assert!(t.work_failure_history.is_empty());
    assert!(t.rejection_history.is_empty());
    assert!(t.spec_revision_count.is_empty());
    assert!(t.spec_abandoned_works.is_empty());
    assert!(t.spec_total_works.is_empty());
}

#[test]
fn unique_theme_count_groups_same_reviewer_complaint() {
    let mut t = DirectorPatternTracker::new();
    t.rejection_history.insert(
        "wk-1".into(),
        vec![
            // Same first sentence, different tail -> same theme.
            "Missing tests for error path. The happy path is covered.".into(),
            "Missing tests for error path. Need negative cases too.".into(),
            // Distinct first sentence -> second theme.
            "Inadequate error handling in the parser.".into(),
        ],
    );
    assert_eq!(
        t.unique_theme_count("wk-1"),
        2,
        "two reviewers citing 'Missing tests for error path' should collapse to one theme"
    );
}

#[test]
fn abandonment_ratio_respects_sample_size() {
    let mut t = DirectorPatternTracker::new();

    // Empty spec -> ratio 0, sample 0
    let (r, n) = t.abandonment_ratio("sp-1");
    assert_eq!((r, n), (0.0, 0));

    // One total, one abandoned -> 1.0, 1
    t.observe_spec_work("sp-1", "wk-1");
    t.observe_abandonment("sp-1", "wk-1");
    let (r, n) = t.abandonment_ratio("sp-1");
    assert_eq!((r, n), (1.0, 1));

    // Four total, two abandoned -> 0.5, 4
    t.observe_spec_work("sp-1", "wk-2");
    t.observe_spec_work("sp-1", "wk-3");
    t.observe_spec_work("sp-1", "wk-4");
    t.observe_abandonment("sp-1", "wk-2");
    let (r, n) = t.abandonment_ratio("sp-1");
    assert!((r - 0.5).abs() < 1e-9);
    assert_eq!(n, 4);
}

#[test]
fn observe_abandonment_is_idempotent() {
    let mut t = DirectorPatternTracker::new();
    t.observe_abandonment("sp-1", "wk-1");
    t.observe_abandonment("sp-1", "wk-1"); // replay - should not double-count
    let (r, n) = t.abandonment_ratio("sp-1");
    assert_eq!((r, n), (1.0, 1));
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
async fn monitoring_missing_plan_exits_loop() {
    // Regression: a Director whose plan has been deleted from Stores must exit cleanly
    // rather than hot-loop forever. plan.get returns RpcError::not_found (code -32001);
    // is_plan_terminal must map that to terminal=true.
    let dir = TestDir::new("director-monitoring-missing");
    let stores = test_stores_arc(&dir);

    // NB: no plan inserted. The Director carries a plan_id that points at nothing.
    // Use the `pl-` prefix so determine_initial_mode routes this into Monitoring (not
    // Escalation); Monitoring is the mode that exercises is_plan_terminal.
    let missing_plan_id = "pl-ghost-never-inserted".to_string();

    let (event_tx, _rx) = broadcast::channel(16);
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some(missing_plan_id));
    let mut agent = make_director(ctx);

    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(
        result.is_ok(),
        "Director must exit quickly when plan is missing (not_found must map to terminal)"
    );
    result.unwrap().unwrap();
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
            serde_json::json!({ "plan-id": plan_id_for_driver }),
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
            serde_json::json!({ "plan-id": driver_pid }),
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
            && ev.data.get("session-id").and_then(|v| v.as_str()) == Some(session_id.as_str())
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
    // Phase 7 semantics: the signature threshold is tripped by a same-root-cause loop -
    // `failure_total >= failure_signature_threshold` AND `unique_signature_count == 1`.
    // Emit 3 copies of the same error to exercise that branch.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for err in [
            "compile error: missing trait",
            "compile error: missing trait",
            "compile error: missing trait",
        ] {
            let _ = driver_tx.send(DaemonEvent::new(
                "agent.status_changed",
                serde_json::json!({
                    "session_id": driver_sid,
                    "status": "failed",
                    "error": err,
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
    // Phase 7 semantics: the theme threshold is tripped by a same-theme reviewer loop -
    // `rejection_total >= rejection_theme_threshold` AND `unique_theme_count == 1`.
    // Emit three rejections with the same theme to exercise that branch.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for reason in [
            "diff-too-large for single bundle",
            "diff-too-large for single bundle",
            "diff-too-large for single bundle",
        ] {
            let _ = driver_tx.send(DaemonEvent::new(
                "bundle.rejected",
                serde_json::json!({
                    "bundle_work_id": "wk-rej",
                    "reason": reason,
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
        "three distinct bundle rejection themes must emit director.mode_changed mode=escalation"
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

#[test]
fn parse_actions_accepts_re_decompose_phase() {
    use crate::agents::director::actions::{DirectorAction, ReDecomposeTarget, parse_actions};
    let payload = r#"{"actions": [
        {"type": "re-decompose", "target_type": "phase", "target_id": "ph-7", "reason": "AC too vague"}
    ]}"#;
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        DirectorAction::ReDecompose {
            target_type,
            target_id,
            reason,
        } => {
            assert_eq!(*target_type, ReDecomposeTarget::Phase);
            assert_eq!(target_id, "ph-7");
            assert_eq!(reason.as_deref(), Some("AC too vague"));
        }
        other => panic!("expected ReDecompose, got {:?}", other),
    }
}

#[test]
fn parse_actions_accepts_re_decompose_spec() {
    use crate::agents::director::actions::{DirectorAction, ReDecomposeTarget, parse_actions};
    let payload = r#"{"actions": [
        {"type": "re-decompose", "target_type": "spec", "target_id": "sp-2"}
    ]}"#;
    let actions = parse_actions(payload).unwrap();
    assert_eq!(actions.len(), 1);
    match &actions[0] {
        DirectorAction::ReDecompose {
            target_type,
            target_id,
            reason,
        } => {
            assert_eq!(*target_type, ReDecomposeTarget::Spec);
            assert_eq!(target_id, "sp-2");
            assert!(reason.is_none());
        }
        other => panic!("expected ReDecompose, got {:?}", other),
    }
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
async fn re_decompose_action_transitions_phase_to_draft() {
    use crate::agents::director::actions::{DirectorAction, ReDecomposeTarget, execute_actions};
    use crate::domain::phase::Phase;
    use crate::domain::spec::Spec;
    use crate::worktree::manager::WorktreeManager;

    let dir = TestDir::new("director-redecompose-phase");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(32);

    let plan = Plan::new("rd-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "rd-spec".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

    let mut phase = Phase::new(spec_id.clone(), "rd-phase".into());
    phase.force_status(HierarchyStatus::Active);
    let phase_id = phase.id.clone();
    stores.write_phases().unwrap().insert(phase_id.clone(), phase);

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

    let action = DirectorAction::ReDecompose {
        target_type: ReDecomposeTarget::Phase,
        target_id: phase_id.clone(),
        reason: Some("missing coverage".into()),
    };
    let report = tokio::task::spawn_blocking(move || execute_actions(&[action], &bridge, &event_tx, "director-test"))
        .await
        .unwrap();

    assert_eq!(report.ok, 1, "re-decompose should succeed: {:?}", report.details);
    assert_eq!(report.failed, 0);

    let phases = stores.read_phases().unwrap();
    let status = phases.get(&phase_id).unwrap().status();
    assert_eq!(status, HierarchyStatus::Draft, "phase must be Draft after re-decompose");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_decompose_action_transitions_spec_to_draft() {
    use crate::agents::director::actions::{DirectorAction, ReDecomposeTarget, execute_actions};
    use crate::domain::spec::Spec;
    use crate::worktree::manager::WorktreeManager;

    let dir = TestDir::new("director-redecompose-spec");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(32);

    let plan = Plan::new("rd-plan-s".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "rd-spec-s".into());
    spec.force_status(HierarchyStatus::Active);
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

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

    let action = DirectorAction::ReDecompose {
        target_type: ReDecomposeTarget::Spec,
        target_id: spec_id.clone(),
        reason: None,
    };
    let report = tokio::task::spawn_blocking(move || execute_actions(&[action], &bridge, &event_tx, "director-test"))
        .await
        .unwrap();

    assert_eq!(report.ok, 1, "re-decompose should succeed: {:?}", report.details);

    let specs = stores.read_specs().unwrap();
    let status = specs.get(&spec_id).unwrap().status();
    assert_eq!(status, HierarchyStatus::Draft, "spec must be Draft after re-decompose");
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
    // Phase 7 semantics: same-root-cause loop - three copies of the same error trip
    // the signature threshold (`failure_total >= N` AND `unique_signature_count == 1`).
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        for err in [
            "compile error: missing trait",
            "compile error: missing trait",
            "compile error: missing trait",
        ] {
            let _ = driver_tx.send(DaemonEvent::new(
                "agent.status_changed",
                serde_json::json!({
                    "session_id": driver_sid,
                    "status": "failed",
                    "error": err,
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

// ─── Phase 7: Cross-session patterns + reconcile_from_ipc ─────────────────

#[test]
fn error_signature_normalizes_case_and_whitespace() {
    use crate::agents::director::error_signature;
    assert_eq!(
        error_signature("Compile Error: mismatched types"),
        error_signature("compile error: different detail")
    );
    assert_eq!(error_signature("  Compile\tError  "), error_signature("compile error"));
}

#[test]
fn error_signature_distinguishes_different_root_causes() {
    use crate::agents::director::error_signature;
    assert_ne!(
        error_signature("network error: connection refused"),
        error_signature("compile error: missing trait impl")
    );
}

#[test]
fn unique_signature_count_groups_same_root_cause() {
    let mut t = DirectorPatternTracker::new();
    t.work_failure_history.insert(
        "wk-1".into(),
        vec![
            ("ag-1".into(), "Compile error: missing trait".into()),
            ("ag-2".into(), "compile error: also missing trait".into()),
            ("ag-3".into(), "Network error: timeout".into()),
        ],
    );
    assert_eq!(t.failure_count("wk-1"), 3, "raw failure count is 3");
    assert_eq!(
        t.unique_signature_count("wk-1"),
        2,
        "compile-error and network-error are 2 distinct root causes"
    );
}

// Property test: populating the tracker by events vs rebuilding via reconcile_from_ipc
// must produce the same rejection history and the same failure counts per work.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_from_ipc_matches_event_driven_counts() {
    use crate::domain::bundle::{Bundle, BundleStatus};
    use crate::domain::phase::Phase;
    use crate::domain::spec::Spec;
    use crate::domain::work::Work;

    let dir = TestDir::new("director-reconcile-property");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(64);

    // Build a minimal Plan -> Spec -> Phase -> Work -> Bundle hierarchy.
    let mut plan = Plan::new("recon-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    plan.force_status(HierarchyStatus::Draft);
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let mut spec = Spec::new(plan_id.clone(), "recon-spec".into());
    spec.decomposition_attempts = 2;
    let spec_id = spec.id.clone();
    stores.write_specs().unwrap().insert(spec_id.clone(), spec);

    let phase = Phase::new(spec_id.clone(), "recon-phase".into());
    let phase_id = phase.id.clone();
    stores.write_phases().unwrap().insert(phase_id.clone(), phase);

    let make_work = |id_marker: &str, failures: u32| {
        let mut w = Work::new(phase_id.clone(), format!("work-{}", id_marker));
        w.session_failure_count = failures;
        w
    };
    let w1 = make_work("a", 2);
    let w1_id = w1.id.clone();
    let w2 = make_work("b", 0);
    let w2_id = w2.id.clone();
    stores.write_works().unwrap().insert(w1_id.clone(), w1);
    stores.write_works().unwrap().insert(w2_id.clone(), w2);

    let mut bundle = Bundle::new(w1_id.clone(), Some("t-1".into()), "branch".into(), vec![]);
    bundle.force_status(BundleStatus::Rejected);
    stores.write_bundles().unwrap().insert(bundle.id.clone(), bundle);
    // Second rejected bundle on the same work so the rejection restart-degradation
    // assertion has a meaningful sample (one bundle trivially satisfies unique==1).
    let mut bundle2 = Bundle::new(w1_id.clone(), Some("t-1".into()), "branch-2".into(), vec![]);
    bundle2.force_status(BundleStatus::Rejected);
    stores.write_bundles().unwrap().insert(bundle2.id.clone(), bundle2);

    // Build a Director that enters Monitoring via pl-* target_id. The event_loop's entry
    // reconciliation fires on the first iteration; we cancel quickly to observe state.
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_id.clone()));
    let mut agent = make_director(ctx);

    // Cancel almost immediately. Reconcile runs before the first select!, so it completes.
    let cancel_stores = stores.clone();
    let cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut sessions = cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(3), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // Reconciled tracker should show: 2 failures for w1, 0 for w2, 2 rejections for w1,
    // and spec_revision_count contains the spec with decomposition_attempts=2.
    assert_eq!(agent.pattern_tracker.failure_count(&w1_id), 2);
    assert_eq!(agent.pattern_tracker.failure_count(&w2_id), 0);
    assert_eq!(agent.pattern_tracker.rejection_count(&w1_id), 2);
    assert_eq!(agent.pattern_tracker.rejection_count(&w2_id), 0);
    assert_eq!(
        agent.pattern_tracker.spec_revision_count.get(&spec_id).copied(),
        Some(2)
    );

    // Restart-degradation invariant: historical placeholders must not collapse to a
    // single signature/theme. If they did, the same-root-cause predicate in
    // `check_thresholds` (`unique == 1`) would falsely fire the moment the daemon
    // restarts. Each synthetic entry must be unique so the signature/theme paths
    // gracefully degrade to the total-count safety net.
    assert_eq!(
        agent.pattern_tracker.unique_signature_count(&w1_id),
        2,
        "each historical failure placeholder must contribute a distinct signature"
    );
    assert_eq!(
        agent.pattern_tracker.unique_theme_count(&w1_id),
        2,
        "each historical rejection placeholder must contribute a distinct theme"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_scopes_to_current_plan() {
    use crate::domain::phase::Phase;
    use crate::domain::spec::Spec;
    use crate::domain::work::Work;

    let dir = TestDir::new("director-reconcile-scoping");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(32);

    // Plan A (target) and Plan B (should be ignored) each with their own hierarchy.
    let plan_a = Plan::new("plan-a".into(), AcceptanceCriteria::default());
    let plan_a_id = plan_a.id.clone();
    stores.write_plans().unwrap().insert(plan_a_id.clone(), plan_a);
    let plan_b = Plan::new("plan-b".into(), AcceptanceCriteria::default());
    let plan_b_id = plan_b.id.clone();
    stores.write_plans().unwrap().insert(plan_b_id.clone(), plan_b);

    for (pid, marker, failures) in &[(plan_a_id.clone(), "a", 1u32), (plan_b_id.clone(), "b", 5u32)] {
        let spec = Spec::new(pid.clone(), format!("spec-{}", marker));
        let spec_id = spec.id.clone();
        stores.write_specs().unwrap().insert(spec_id.clone(), spec);
        let phase = Phase::new(spec_id, format!("phase-{}", marker));
        let phase_id = phase.id.clone();
        stores.write_phases().unwrap().insert(phase_id.clone(), phase);
        let mut w = Work::new(phase_id.clone(), format!("work-{}", marker));
        w.session_failure_count = *failures;
        let w_id = w.id.clone();
        stores.write_works().unwrap().insert(w_id, w);
    }

    // Director scoped to Plan A.
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), None, Some(plan_a_id.clone()));
    let mut agent = make_director(ctx);

    let cancel_stores = stores.clone();
    let cancel_id = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut sessions = cancel_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&cancel_id) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(3), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // Plan A's work has 1 failure. Plan B's work has 5 but must not leak in.
    let total_failures: usize = agent
        .pattern_tracker
        .work_failure_history
        .values()
        .map(|v| v.len())
        .sum();
    assert_eq!(
        total_failures, 1,
        "Plan A reconciliation must not pull in Plan B's failures (got {} total)",
        total_failures
    );
}

// ─── Phase 6: UserIntervention ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_message_in_monitoring_enters_intervention_and_returns() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-user-intervention");
    let stores = test_stores_arc(&dir);
    let (event_tx, mut rx) = broadcast::channel(64);

    let plan = Plan::new("intervention-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let (tx_msg, rx_msg) = mpsc::channel::<String>(8);
    // Scripted response: single message-user action confirming receipt.
    let response = r#"{"actions": [{"type": "message-user", "text": "acknowledged"}]}"#;
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), Some(rx_msg), Some(plan_id.clone()));
    let mut agent = make_scripted_director(ctx, response);

    let driver_stores = stores.clone();
    let driver_session = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx_msg.send("prioritize the lint fix please".to_string()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut sessions = driver_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_session) {
            s.force_status(crate::agents::AgentStatus::Cancelled);
        }
    });

    let result = tokio::time::timeout(Duration::from_secs(5), agent.run()).await;
    assert!(result.is_ok());
    result.unwrap().unwrap();

    // Mode transitions fired: Monitoring -> UserIntervention -> Monitoring
    let mut modes: Vec<String> = Vec::new();
    let mut saw_diagnosis = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.event == "director.mode_changed"
            && let Some(m) = ev.data.get("mode").and_then(|v| v.as_str())
        {
            modes.push(m.to_string());
        }
        if ev.event == "director.diagnosis" && ev.data.get("text").and_then(|v| v.as_str()) == Some("acknowledged") {
            saw_diagnosis = true;
        }
    }
    assert!(
        modes.contains(&"user-intervention".to_string()),
        "user-intervention mode must be entered (modes: {:?})",
        modes
    );
    assert_eq!(
        modes.last().map(|s| s.as_str()),
        Some("monitoring"),
        "final mode must be monitoring (modes: {:?})",
        modes
    );
    assert!(saw_diagnosis, "director.diagnosis must carry the LLM's acknowledgement");

    let sessions = stores.agent_sessions.read().unwrap();
    let sess = sessions.get(&session_id).unwrap();
    assert_eq!(
        sess.director_mode,
        Some(DirectorMode::Monitoring),
        "Director must return to Monitoring after UserIntervention"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_intervention_tolerates_unparseable_llm() {
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-intervention-bad-json");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);

    let plan = Plan::new("bad-json-plan".into(), AcceptanceCriteria::default());
    let plan_id = plan.id.clone();
    stores.write_plans().unwrap().insert(plan_id.clone(), plan);

    let (tx_msg, rx_msg) = mpsc::channel::<String>(8);
    let (ctx, session_id) = director_ctx(stores.clone(), event_tx.clone(), Some(rx_msg), Some(plan_id.clone()));
    let mut agent = make_scripted_director(ctx, "not JSON");

    let driver_stores = stores.clone();
    let driver_session = session_id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = tx_msg.send("help me".to_string()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut sessions = driver_stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&driver_session) {
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
        "parse failure must still flip back to Monitoring"
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
