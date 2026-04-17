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
    // Legacy escalation reads `prompts::store().director` which panics if prompts weren't
    // initialized. init_defaults is idempotent via OnceLock::set.
    crate::prompts::init_defaults();

    let dir = TestDir::new("director-legacy-escalation");
    let stores = test_stores_arc(&dir);
    let (event_tx, _rx) = broadcast::channel(16);
    // target_id doesn't start with pl- → legacy Escalation path.
    let (ctx, _sid) = director_ctx(stores.clone(), event_tx.clone(), None, Some("wk-x1".into()));
    let mut agent = make_director(ctx);

    // Legacy escalation is one-shot; it should return almost immediately.
    let result = tokio::time::timeout(Duration::from_secs(2), agent.run()).await;
    assert!(result.is_ok(), "legacy escalation must complete quickly");
    result.unwrap().unwrap();
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
