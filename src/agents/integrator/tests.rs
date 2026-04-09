use super::*;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::{AgentContext, AgentKind, AgentSession, AgentStatus};
use crate::config::{Config, ProjectConfig};
use crate::daemon::context::Stores;
use crate::domain::bundle::Bundle;
use crate::domain::tick::Tick;
use crate::domain::work::{Work, WorkStatus};
use crate::test_util::TestDir;
use crate::tools::ToolRunner;
use crate::worktree::manager::WorktreeManager;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};
use taskstore::Store;
use tokio::sync::broadcast;

fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
    let config = Config {
        project: ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..ProjectConfig::default()
        },
        ..Config::default()
    };
    let store = Store::open(dir).unwrap();
    let mut stores = Stores::new();
    stores.store = Some(Arc::new(StdMutex::new(store)));
    stores.config = config;
    Arc::new(stores)
}

fn test_config() -> IntegratorConfig {
    IntegratorConfig {
        validation_commands: vec!["true".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    }
}

fn failing_config() -> IntegratorConfig {
    IntegratorConfig {
        validation_commands: vec!["false".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    }
}

/// Create an IntegratorAgent for testing. The session is inserted into stores.
fn test_integrator(dir: &std::path::Path, stores: Arc<Stores>, intg_config: IntegratorConfig) -> IntegratorAgent {
    let (event_tx, _) = broadcast::channel(64);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    let session = AgentSession::new(AgentKind::Integrator, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session.clone());
    let ctx = AgentContext {
        session,
        stores,
        bridge,
        event_tx,
        tool_runner: Arc::new(ToolRunner::new(&[])),
        tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),

        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    };
    IntegratorAgent::new(ctx, intg_config)
}

/// Create an IntegratorAgent with a custom config for Stores.
fn test_integrator_with_stores_config(
    dir: &std::path::Path,
    stores: Arc<Stores>,
    intg_config: IntegratorConfig,
) -> (IntegratorAgent, broadcast::Sender<DaemonEvent>) {
    let (event_tx, _) = broadcast::channel(64);
    let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

    let session = AgentSession::new(AgentKind::Integrator, "model".into());
    stores
        .agent_sessions
        .write()
        .unwrap()
        .insert(session.id.clone(), session.clone());
    let ctx = AgentContext {
        session,
        stores,
        bridge,
        event_tx: event_tx.clone(),
        tool_runner: Arc::new(ToolRunner::new(&[])),
        tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),

        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    };
    (IntegratorAgent::new(ctx, intg_config), event_tx)
}

// --- is_cancelled tests (via AgentContext) ---

#[tokio::test(flavor = "multi_thread")]
async fn test_is_cancelled_false() {
    let dir = TestDir::new("loopr-intg-canc1");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Integrator, "model".into());
    let _ = session.transition_to(AgentStatus::Running);
    let sid = session.id.clone();
    stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

    let agent = test_integrator(&dir, stores, test_config());
    assert!(!agent.ctx.is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_is_cancelled_true() {
    let dir = TestDir::new("loopr-intg-canc2");
    let stores = test_stores(&dir);

    let mut session = AgentSession::new(AgentKind::Integrator, "model".into());
    let _ = session.transition_to(AgentStatus::Running);
    let _ = session.transition_to(AgentStatus::Cancelled);
    let sid = session.id.clone();
    stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

    let agent = test_integrator(&dir, stores, test_config());
    // The agent's own session is Running (created by test_integrator), but the
    // pre-inserted cancelled session is a different ID. Test via AgentContext's
    // general is_cancelled which checks the agent's own session.
    // For this test, we need to cancel the agent's own session.
    {
        let mut sessions = agent.ctx.stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&agent.ctx.session.id) {
            let _ = s.transition_to(AgentStatus::Running);
            let _ = s.transition_to(AgentStatus::Cancelled);
        }
    }
    assert!(agent.ctx.is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_is_cancelled_missing() {
    let dir = TestDir::new("loopr-intg-canc3");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores.clone(), test_config());
    // Remove the session so it's "missing"
    stores.agent_sessions.write().unwrap().remove(&agent.ctx.session.id);
    assert!(agent.ctx.is_cancelled());
}

// --- Helper method tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_latest_published_tick_id_none() {
    let dir = TestDir::new("loopr-intg-latest1");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores, test_config());
    assert!(agent.latest_published_tick_id().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_latest_published_tick_id_some() {
    let dir = TestDir::new("loopr-intg-latest2");
    let stores = test_stores(&dir);

    let mut tick1 = Tick::new(1);
    tick1.force_status(TickStatus::Published);
    let mut tick2 = Tick::new(2);
    tick2.force_status(TickStatus::Published);
    let tick2_id = tick2.id.clone();
    let mut tick3 = Tick::new(3);
    tick3.force_status(TickStatus::Failed);

    stores.ticks.write().unwrap().insert(tick1.id.clone(), tick1);
    stores.ticks.write().unwrap().insert(tick2.id.clone(), tick2);
    stores.ticks.write().unwrap().insert(tick3.id.clone(), tick3);

    let agent = test_integrator(&dir, stores, test_config());
    assert_eq!(agent.latest_published_tick_id(), Some(tick2_id));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_next_tick_number_empty() {
    let dir = TestDir::new("loopr-intg-next1");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores, test_config());
    assert_eq!(agent.next_tick_number().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_next_tick_number_with_existing() {
    let dir = TestDir::new("loopr-intg-next2");
    let stores = test_stores(&dir);

    let tick = Tick::new(5);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores, test_config());
    assert_eq!(agent.next_tick_number().unwrap(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_has_tick_in_progress_false() {
    let dir = TestDir::new("loopr-intg-tip1");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores.clone(), test_config());
    assert!(!agent.has_tick_in_progress().unwrap());

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
    assert!(!agent.has_tick_in_progress().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_has_tick_in_progress_true() {
    let dir = TestDir::new("loopr-intg-tip2");
    let stores = test_stores(&dir);

    let tick = Tick::new(1);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores, test_config());
    assert!(agent.has_tick_in_progress().unwrap());
}

// --- recover_stuck_ticks tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_recover_stuck_ticks_none() {
    let dir = TestDir::new("loopr-intg-recov1");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores, test_config());
    assert_eq!(agent.recover_stuck_ticks().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_recover_stuck_ticks_sealing() {
    let dir = TestDir::new("loopr-intg-recov2");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Sealing);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    assert_eq!(agent.recover_stuck_ticks().unwrap(), 1);

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_recover_stuck_ticks_validating() {
    let dir = TestDir::new("loopr-intg-recov3");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(2);
    tick.force_status(TickStatus::Validating);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    assert_eq!(agent.recover_stuck_ticks().unwrap(), 1);

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
}

// --- run_cycle tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_idle_no_bundles() {
    let dir = TestDir::new("loopr-intg-cycle1");
    let stores = test_stores(&dir);
    let agent = test_integrator(&dir, stores, test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::Idle);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_recovers_open_tick() {
    let dir = TestDir::new("loopr-intg-cycle2");
    let stores = test_stores(&dir);

    let tick = Tick::new(1);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::Recovered { count: 1 });

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_recovers_stuck_tick() {
    let dir = TestDir::new("loopr-intg-cycle3");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Sealing);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores, test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::Recovered { count: 1 });
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_publishes_tick() {
    let dir = TestDir::new("loopr-intg-cycle4");
    let stores = test_stores(&dir);

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::Published { .. }),
        "expected Published, got {:?}",
        result
    );

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].status(), BundleStatus::Merged);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_validation_failure() {
    let dir = TestDir::new("loopr-intg-cycle5");
    let stores = test_stores(&dir);

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), failing_config());
    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::ValidationFailed { .. }),
        "expected ValidationFailed, got {:?}",
        result
    );

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].status(), BundleStatus::Rejected);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_stale_bundle_rejected() {
    let dir = TestDir::new("loopr-intg-cycle6");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let mut bundle = Bundle::new(
        "wi-1".into(),
        Some("wrong-tick-id".into()),
        "feature/x".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].status(), BundleStatus::Rejected);

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks.len(), 1);
    assert_eq!(ticks[&tick_id].status(), TickStatus::Published);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_mixed_stale_and_valid() {
    let dir = TestDir::new("loopr-intg-cycle7");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    let published_tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let mut valid_bundle = Bundle::new(
        "wi-1".into(),
        Some(published_tick_id.clone()),
        "feature/valid".into(),
        vec!["claims".into()],
    );
    valid_bundle.force_status(BundleStatus::Accepted);
    let valid_id = valid_bundle.id.clone();
    stores
        .bundles
        .write()
        .unwrap()
        .insert(valid_bundle.id.clone(), valid_bundle);

    let mut stale_bundle = Bundle::new(
        "wi-2".into(),
        Some("old-tick-id".into()),
        "feature/stale".into(),
        vec!["claims".into()],
    );
    stale_bundle.force_status(BundleStatus::Accepted);
    let stale_id = stale_bundle.id.clone();
    stores
        .bundles
        .write()
        .unwrap()
        .insert(stale_bundle.id.clone(), stale_bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::Published { .. }),
        "expected Published, got {:?}",
        result
    );

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&valid_id].status(), BundleStatus::Merged);
    assert_eq!(bundles[&stale_id].status(), BundleStatus::Rejected);
}

fn test_stores_with_config(dir: &std::path::Path, config: Config) -> Arc<Stores> {
    let store = Store::open(dir).unwrap();
    let mut stores = Stores::new();
    stores.store = Some(Arc::new(StdMutex::new(store)));
    stores.config = config;
    Arc::new(stores)
}

// --- has_tick_in_progress: all non-terminal states ---

#[tokio::test(flavor = "multi_thread")]
async fn test_has_tick_in_progress_all_states() {
    let dir = TestDir::new("loopr-intg-tipall");

    let stores = test_stores(&dir);
    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Sealing);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
    let agent = test_integrator(&dir, stores, test_config());
    assert!(agent.has_tick_in_progress().unwrap());

    let stores = test_stores(&dir);
    let mut tick = Tick::new(2);
    tick.force_status(TickStatus::Validating);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
    let agent = test_integrator(&dir, stores, test_config());
    assert!(agent.has_tick_in_progress().unwrap());

    let stores = test_stores(&dir);
    let mut tick = Tick::new(3);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
    let agent = test_integrator(&dir, stores, test_config());
    assert!(!agent.has_tick_in_progress().unwrap());

    let stores = test_stores(&dir);
    let mut tick = Tick::new(4);
    tick.force_status(TickStatus::Failed);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);
    let agent = test_integrator(&dir, stores, test_config());
    assert!(!agent.has_tick_in_progress().unwrap());
}

// --- Stale policy tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_stale_policy_replan_at_safe_point() {
    let dir = TestDir::new("loopr-intg-replan");

    let mut config = Config {
        project: ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..ProjectConfig::default()
        },
        ..Config::default()
    };
    config.strategy.stale_policy = crate::config::StalePolicy::ReplanAtSafePoint;
    let stores = test_stores_with_config(&dir, config);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let mut bundle = Bundle::new(
        "wi-1".into(),
        Some("wrong-id".into()),
        "feature/x".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let (agent, event_tx) = test_integrator_with_stores_config(&dir, stores.clone(), test_config());
    let mut event_rx = event_tx.subscribe();
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].status(), BundleStatus::Rejected);

    let mut found_replan = false;
    while let Ok(event) = event_rx.try_recv() {
        if event.event == "bundle.stale_replan_needed" {
            found_replan = true;
            break;
        }
    }
    assert!(found_replan, "expected bundle.stale_replan_needed event");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stale_rejection_resets_work_to_ready() {
    let dir = TestDir::new("loopr-intg-stale-reset");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    // Create a Work in InReview status (with acceptance_criteria for Ready precondition)
    let mut wi = Work::new("ph-1".into(), "Task A".into());
    wi.force_status(WorkStatus::InReview);
    wi.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec!["tests pass".into()]);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    // Create a stale bundle referencing the work
    let mut bundle = Bundle::new(
        wi_id.clone(),
        Some("wrong-tick-id".into()),
        "feature/x".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

    // Work should be reset to Ready
    let works = stores.works.read().unwrap();
    assert_eq!(
        works[&wi_id].status(),
        WorkStatus::Ready,
        "Work should be reset to Ready after stale bundle rejection"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stale_rejection_creates_learning() {
    let dir = TestDir::new("loopr-intg-stale-learn");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let mut wi = Work::new("ph-1".into(), "Task B".into());
    wi.force_status(WorkStatus::InReview);
    wi.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec!["tests pass".into()]);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut bundle = Bundle::new(
        wi_id.clone(),
        Some("wrong-tick-id".into()),
        "feature/y".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let _ = agent.run_cycle().unwrap();

    // A learning should be created about the rejection
    let learnings = stores.learnings.read().unwrap();
    assert!(
        learnings
            .values()
            .any(|l| l.content.contains("Bundle rejected") && l.content.contains("stale")),
        "expected a learning about bundle rejection, found: {:?}",
        learnings.values().map(|l| &l.content).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stale_rejection_handles_terminal_work() {
    let dir = TestDir::new("loopr-intg-stale-term");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    // Create a Work that's already Done (terminal)
    let mut wi = Work::new("ph-1".into(), "Task C".into());
    wi.force_status(WorkStatus::Done);
    let wi_id = wi.id.clone();
    stores.works.write().unwrap().insert(wi.id.clone(), wi);

    let mut bundle = Bundle::new(
        wi_id.clone(),
        Some("wrong-tick-id".into()),
        "feature/z".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    // Should not panic even if work transition fails
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

    // Work stays in Done (terminal state, transition should fail gracefully)
    let works = stores.works.read().unwrap();
    assert_eq!(works[&wi_id].status(), WorkStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_stale_policy_auto_replay_and_verify() {
    let dir = TestDir::new("loopr-intg-replay");

    let mut config = Config {
        project: ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..ProjectConfig::default()
        },
        ..Config::default()
    };
    config.strategy.stale_policy = crate::config::StalePolicy::AutoReplayAndVerify;
    let stores = test_stores_with_config(&dir, config);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Published);
    let published_tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let mut bundle = Bundle::new(
        "wi-1".into(),
        Some("wrong-id".into()),
        "feature/x".into(),
        vec!["claims".into()],
    );
    bundle.force_status(BundleStatus::Accepted);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert_eq!(result, IntegratorCycleResult::StaleRejected { count: 1 });

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&bundle_id].status(), BundleStatus::Rejected);
    drop(bundles);

    let mut valid_bundle = Bundle::new(
        "wi-2".into(),
        Some(published_tick_id.clone()),
        "feature/valid".into(),
        vec!["claims".into()],
    );
    valid_bundle.force_status(BundleStatus::Accepted);
    let valid_id = valid_bundle.id.clone();
    stores
        .bundles
        .write()
        .unwrap()
        .insert(valid_bundle.id.clone(), valid_bundle);

    stores
        .bundles
        .write()
        .unwrap()
        .get_mut(&bundle_id)
        .unwrap()
        .force_status(BundleStatus::Accepted);

    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::Published { .. }),
        "expected Published, got {:?}",
        result
    );

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&valid_id].status(), BundleStatus::Merged);
}

// --- recover_stuck_ticks learning creation ---

#[tokio::test(flavor = "multi_thread")]
async fn test_recover_stuck_ticks_learning_creation() {
    let dir = TestDir::new("loopr-intg-recovlearn");
    let stores = test_stores(&dir);

    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Validating);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let recovered = agent.recover_stuck_ticks().unwrap();
    assert_eq!(recovered, 1);

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);

    let learnings = stores.learnings.read().unwrap();
    assert!(
        learnings
            .values()
            .any(|l| l.content.contains(&tick_id) && l.content.contains("stuck")),
        "expected a learning about stuck tick recovery, found: {:?}",
        learnings.values().map(|l| &l.content).collect::<Vec<_>>()
    );
}

// --- Tick creation and sealing error handling ---

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_tick_creation_error_handling() {
    let dir = TestDir::new("loopr-intg-tcreate");
    let stores = test_stores(&dir);
    let config = IntegratorConfig {
        validation_commands: vec!["echo stderr_msg >&2; false".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    };

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores, config);
    let result = agent.run_cycle().unwrap();
    match result {
        IntegratorCycleResult::ValidationFailed { log, .. } => {
            assert!(log.contains("stderr_msg"), "log should contain stderr: {}", log);
            assert!(log.contains("FAILED"), "log should contain FAILED: {}", log);
        }
        other => panic!("expected ValidationFailed, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_bundle_sealing_error_handling() {
    let dir = TestDir::new("loopr-intg-bseal");
    let stores = test_stores(&dir);

    let mut b1 = Bundle::new("wi-1".into(), None, "feature/a".into(), vec!["claims".into()]);
    b1.force_status(BundleStatus::Accepted);
    let b1_id = b1.id.clone();
    stores.bundles.write().unwrap().insert(b1.id.clone(), b1);

    let agent = test_integrator(&dir, stores.clone(), test_config());
    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::Published { .. }),
        "expected Published, got {:?}",
        result
    );

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(bundles[&b1_id].status(), BundleStatus::Merged);
}

// --- Validation with multiple commands ---

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_validation_multi_command_sequence() {
    let dir = TestDir::new("loopr-intg-multi");
    let stores = test_stores(&dir);

    let config = IntegratorConfig {
        validation_commands: vec!["echo step1".to_string(), "echo step2".to_string(), "false".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    };

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), config);
    let result = agent.run_cycle().unwrap();
    match result {
        IntegratorCycleResult::ValidationFailed { log, .. } => {
            assert!(log.contains("step1"), "should have step1 output");
            assert!(log.contains("step2"), "should have step2 output");
            assert!(log.contains("PASSED"), "first commands should PASS");
            assert!(log.contains("FAILED"), "third command should FAIL");
        }
        other => panic!("expected ValidationFailed, got {:?}", other),
    }

    let config_pass = IntegratorConfig {
        validation_commands: vec![
            "echo check1".to_string(),
            "echo check2".to_string(),
            "echo check3".to_string(),
        ],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    };

    let mut bundle2 = Bundle::new("wi-2".into(), None, "feature/y".into(), vec!["claims".into()]);
    bundle2.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle2.id.clone(), bundle2);

    // Need a new agent with the pass config
    let agent = test_integrator(&dir, stores, config_pass);
    let result = agent.run_cycle().unwrap();
    assert!(
        matches!(result, IntegratorCycleResult::Published { .. }),
        "expected Published, got {:?}",
        result
    );
}

// --- Tick publish creates learning on failure ---

#[tokio::test(flavor = "multi_thread")]
async fn test_cycle_tick_publish_learning_creation() {
    let dir = TestDir::new("loopr-intg-publearn");
    let stores = test_stores(&dir);

    let mut bundle = Bundle::new("wi-1".into(), None, "feature/x".into(), vec!["claims".into()]);
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let agent = test_integrator(&dir, stores.clone(), failing_config());
    let result = agent.run_cycle().unwrap();
    let tick_id = match &result {
        IntegratorCycleResult::ValidationFailed { tick_id, .. } => tick_id.clone(),
        other => panic!("expected ValidationFailed, got {:?}", other),
    };

    let learnings = stores.learnings.read().unwrap();
    assert!(
        learnings
            .values()
            .any(|l| l.content.contains(&tick_id) && l.content.contains("validation failed")),
        "expected a learning about validation failure for tick {}, found: {:?}",
        tick_id,
        learnings.values().map(|l| &l.content).collect::<Vec<_>>()
    );
}

// --- Agent::run() async loop tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_run_integrator_cancellation() {
    let dir = TestDir::new("loopr-intg-cancel");
    let stores = test_stores(&dir);
    let config = IntegratorConfig {
        validation_commands: vec!["true".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: None,
        ..Default::default()
    };

    let mut agent = test_integrator(&dir, stores.clone(), config);

    // Pre-cancel the agent's own session so run() exits immediately
    {
        let mut sessions = stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&agent.ctx.session.id) {
            let _ = s.transition_to(AgentStatus::Running);
            let _ = s.transition_to(AgentStatus::Cancelled);
        }
    }

    let result = agent.run().await;
    assert!(result.is_ok(), "cancelled integrator should return Ok: {:?}", result);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_run_integrator_timeout() {
    let dir = TestDir::new("loopr-intg-timeout");
    let stores = test_stores(&dir);
    let config = IntegratorConfig {
        validation_commands: vec!["true".to_string()],
        interval_secs: 1,
        enabled: true,
        session_timeout_secs: Some(1),
        ..Default::default()
    };

    let mut agent = test_integrator(&dir, stores.clone(), config);
    let sid = agent.ctx.session.id.clone();

    let stores_clone = stores.clone();
    let cancel_handle = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut sessions = stores_clone.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&sid) {
            let _ = s.transition_to(AgentStatus::Running);
            let _ = s.transition_to(AgentStatus::Cancelled);
        }
    });

    let result = agent.run().await;
    assert!(
        result.is_ok(),
        "integrator should exit cleanly on cancellation: {:?}",
        result
    );
    cancel_handle.await.unwrap();
}

// --- run_validation_commands tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_commands_pass() {
    let (passed, log) = run_validation_commands(&["true".to_string()]);
    assert!(passed);
    assert!(log.contains("PASSED"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_commands_fail() {
    let (passed, log) = run_validation_commands(&["false".to_string()]);
    assert!(!passed);
    assert!(log.contains("FAILED"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_commands_empty() {
    let (passed, log) = run_validation_commands(&[]);
    assert!(passed);
    assert!(log.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_commands_multiple_pass() {
    let (passed, _) = run_validation_commands(&["true".to_string(), "true".to_string()]);
    assert!(passed);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_validation_commands_first_fails() {
    let (passed, log) = run_validation_commands(&["false".to_string(), "true".to_string()]);
    assert!(!passed);
    assert!(log.contains("FAILED"));
}

// --- Fix #3: merge_bundle_branches cleanup tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_merge_bundle_branches_success() {
    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    let dir = TestDir::new("loopr-intg-merge-ok");

    // Initialize git repo with initial commit
    git(&dir, &["init"]);
    git(&dir, &["config", "user.email", "test@test.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("main.txt"), "main").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "initial"]);

    // Record the default branch name
    let out = git(&dir, &["branch", "--show-current"]);
    let default_branch = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // Create a feature branch with a commit
    git(&dir, &["checkout", "-b", "feature-1"]);
    std::fs::write(dir.join("feature.txt"), "feature").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "feature"]);
    git(&dir, &["checkout", &default_branch]);

    let result = merge_bundle_branches(&dir, &["feature-1".to_string()]);
    assert!(result.is_ok(), "merge should succeed: {:?}", result);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_merge_bundle_branches_failure_cleans_up() {
    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }

    let dir = TestDir::new("loopr-intg-merge-abort");

    // Initialize git repo
    git(&dir, &["init"]);
    git(&dir, &["config", "user.email", "test@test.com"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.join("conflict.txt"), "main-content").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "initial"]);

    // Record the default branch name (main or master)
    let out = git(&dir, &["branch", "--show-current"]);
    let default_branch = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // Create a feature branch with conflicting content
    git(&dir, &["checkout", "-b", "conflict-branch"]);
    std::fs::write(dir.join("conflict.txt"), "branch-content").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "branch change"]);

    // Go back to the default branch and make a conflicting change
    git(&dir, &["checkout", &default_branch]);
    std::fs::write(dir.join("conflict.txt"), "main-different-content").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "main diverge"]);

    // Merge should fail due to conflict
    let result = merge_bundle_branches(&dir, &["conflict-branch".to_string()]);
    assert!(result.is_err(), "merge should fail with conflict");
    let err = result.unwrap_err().to_string();
    assert!(err.contains("aborted"), "error should mention aborted: {}", err);

    // Verify repo is NOT in a half-merged state (no .git/MERGE_HEAD)
    assert!(
        !dir.join(".git/MERGE_HEAD").exists(),
        "MERGE_HEAD should not exist after cleanup"
    );
}

// --- effective_validation_commands tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_effective_validation_commands_global_only() {
    let dir = TestDir::new("loopr-int-evc-global");
    let stores = test_stores(&dir);
    let global = vec!["echo global".to_string()];
    let result = effective_validation_commands(&global, &[], &stores);
    assert_eq!(result, vec!["echo global"]);
}

// Phase-level validation_commands removed in domain-model-cleanup Phase 3.
// effective_validation_commands now returns only global commands regardless of phase.
#[tokio::test(flavor = "multi_thread")]
async fn test_effective_validation_commands_with_phase_returns_global_only() {
    let dir = TestDir::new("loopr-int-evc-phase");
    let stores = test_stores(&dir);
    let global = vec!["echo global".to_string()];
    let result = effective_validation_commands(&global, &[], &stores);
    assert_eq!(result, vec!["echo global"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_effective_validation_commands_global_unchanged() {
    let dir = TestDir::new("loopr-int-evc-dedup");
    let stores = test_stores(&dir);
    let global = vec!["echo global".to_string()];
    let result = effective_validation_commands(&global, &[], &stores);
    assert_eq!(result, vec!["echo global"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_effective_validation_commands_empty_phase() {
    use crate::domain::phase::Phase;

    let dir = TestDir::new("loopr-int-evc-empty");
    let stores = test_stores(&dir);

    let phase = Phase::new("spec-1".into(), "P1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    let work = Work::new(phase_id, "W1".into());
    let work_id = work.id.clone();
    stores.works.write().unwrap().insert(work.id.clone(), work);

    let bundle = Bundle::new(work_id, None, "feature/test".into(), vec![]);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let global = vec!["echo global".to_string()];
    let result = effective_validation_commands(&global, &[bundle_id], &stores);
    assert_eq!(result, vec!["echo global"]);
}

// --- audit_git_state tests ---

/// Initialize a bare git repo with a single commit so git operations work in tests.
fn init_test_git_repo(dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    // Create an initial commit so HEAD is valid
    std::fs::write(dir.join("readme.md"), "test").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .output()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_audit_git_state_skips_non_git_dir() {
    let dir = TestDir::new("loopr-intg-audit-nogit");
    let stores = test_stores(&dir);
    // Put a Published tick with no SHA into stores — would be catastrophic if git exists
    let mut tick = Tick::new(1);
    tick.force_status(crate::domain::tick::TickStatus::Published);
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let intg = test_integrator(&dir, stores.clone(), test_config());
    // Should not panic or set degraded (no .git directory)
    intg.audit_git_state();
    assert!(!stores.degraded.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_audit_tick_shas_sets_degraded_on_missing_sha() {
    let dir = TestDir::new("loopr-intg-audit-sha");
    init_test_git_repo(&dir);
    let stores = test_stores(&dir);

    // Published tick with no integration_sha
    let mut tick = Tick::new(1);
    tick.force_status(crate::domain::tick::TickStatus::Published);
    // integration_sha is None by default
    stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

    let intg = test_integrator(&dir, stores.clone(), test_config());
    intg.audit_git_state();
    assert!(stores.degraded.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_audit_branches_rejects_bundle_with_missing_branch() {
    let dir = TestDir::new("loopr-intg-audit-branch");
    init_test_git_repo(&dir);
    let stores = test_stores(&dir);

    // Non-terminal bundle whose work_id branch does NOT exist in git
    let work = Work::new("phase-1".into(), "Test Work".into());
    let work_id = work.id.clone();
    stores.works.write().unwrap().insert(work_id.clone(), work);

    let bundle = Bundle::new(work_id.clone(), None, format!("agent/{}", work_id), vec![]);
    let bundle_id = bundle.id.clone();
    stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

    let intg = test_integrator(&dir, stores.clone(), test_config());
    intg.audit_git_state();

    // Bundle should be rejected since the branch doesn't exist
    let bundles = stores.bundles.read().unwrap();
    assert_eq!(
        bundles[&bundle_id].status(),
        BundleStatus::Rejected,
        "bundle with missing branch should be Rejected"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_run_cycle_returns_idle_when_degraded() {
    let dir = TestDir::new("loopr-intg-degraded");
    init_test_git_repo(&dir);
    let stores = test_stores(&dir);

    // Set degraded flag directly
    stores.degraded.store(true, std::sync::atomic::Ordering::Relaxed);

    // Add an Accepted bundle to make cycle want to work
    let work = Work::new("phase-1".into(), "W".into());
    let work_id = work.id.clone();
    stores.works.write().unwrap().insert(work_id.clone(), work);
    let mut bundle = Bundle::new(work_id.clone(), None, format!("agent/{}", work_id), vec![]);
    bundle.force_status(BundleStatus::Accepted);
    stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

    let intg = test_integrator(&dir, stores.clone(), test_config());
    // run_cycle() should return Idle because degraded = true
    // (audit_git_state will re-check and might clear/reset, but degraded is already set)
    // Since git repo has no Ticks with bad SHAs and no bundles with bad branches after branch check,
    // degraded stays true from the manual set
    let result = intg.run_cycle().unwrap();
    assert_eq!(
        result,
        IntegratorCycleResult::Idle,
        "run_cycle should return Idle when degraded"
    );
}

// --- classify_conflict tests ---

#[test]
fn test_classify_conflict_no_overlap_returns_none() {
    let dir = TestDir::new("loopr-intg-cc1");
    let stores = test_stores(&dir);

    let wk_a = Work::new("phase-1".into(), "Work A".into());
    let wk_b = Work::new("phase-1".into(), "Work B".into());

    let mut bundle_a = Bundle::new(wk_a.id.clone(), None, "branch-a".into(), vec![]);
    bundle_a.paths = vec!["src/a.rs".into()];
    let mut bundle_b = Bundle::new(wk_b.id.clone(), None, "branch-b".into(), vec![]);
    bundle_b.paths = vec!["src/b.rs".into()];
    bundle_a.force_status(crate::domain::bundle::BundleStatus::Reviewed);
    bundle_b.force_status(crate::domain::bundle::BundleStatus::Reviewed);

    stores.works.write().unwrap().insert(wk_a.id.clone(), wk_a);
    stores.works.write().unwrap().insert(wk_b.id.clone(), wk_b);
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_a.id.clone(), bundle_a.clone());
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_b.id.clone(), bundle_b.clone());

    let result = classify_conflict(&stores, &[bundle_a.id.clone(), bundle_b.id.clone()]);
    assert!(result.is_none(), "non-overlapping paths should return None");
}

#[test]
fn test_classify_conflict_with_overlap_returns_some() {
    let dir = TestDir::new("loopr-intg-cc2");
    let stores = test_stores(&dir);

    let wk_a = Work::new("phase-1".into(), "Work A".into());
    let wk_b = Work::new("phase-1".into(), "Work B".into());

    let mut bundle_a = Bundle::new(wk_a.id.clone(), None, "branch-a".into(), vec![]);
    bundle_a.paths = vec!["src/database.py".into(), "src/models.py".into()];
    let mut bundle_b = Bundle::new(wk_b.id.clone(), None, "branch-b".into(), vec![]);
    bundle_b.paths = vec!["src/database.py".into(), "tests/test_db.py".into()]; // database.py is shared
    bundle_a.force_status(crate::domain::bundle::BundleStatus::Reviewed);
    bundle_b.force_status(crate::domain::bundle::BundleStatus::Reviewed);

    let wk_a_id = wk_a.id.clone();
    let wk_b_id = wk_b.id.clone();
    stores.works.write().unwrap().insert(wk_a.id.clone(), wk_a);
    stores.works.write().unwrap().insert(wk_b.id.clone(), wk_b);
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_a.id.clone(), bundle_a.clone());
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_b.id.clone(), bundle_b.clone());

    let result = classify_conflict(&stores, &[bundle_a.id.clone(), bundle_b.id.clone()]);
    assert!(result.is_some(), "overlapping paths should return Some");
    let (files, works) = result.unwrap();
    assert!(files.contains("src/database.py"));
    assert!(works.contains(&wk_a_id));
    assert!(works.contains(&wk_b_id));
}

#[test]
fn test_classify_conflict_empty_bundle_list_returns_none() {
    let dir = TestDir::new("loopr-intg-cc3");
    let stores = test_stores(&dir);
    let result = classify_conflict(&stores, &[]);
    assert!(result.is_none(), "empty bundle list should return None");
}

#[test]
fn test_classify_conflict_empty_paths_returns_none() {
    let dir = TestDir::new("loopr-intg-cc4");
    let stores = test_stores(&dir);

    let wk_a = Work::new("phase-1".into(), "Work A".into());
    let wk_b = Work::new("phase-1".into(), "Work B".into());

    // Both bundles have empty paths - should not trigger conflict
    let bundle_a = Bundle::new(wk_a.id.clone(), None, "branch-a".into(), vec![]);
    let bundle_b = Bundle::new(wk_b.id.clone(), None, "branch-b".into(), vec![]);

    stores.works.write().unwrap().insert(wk_a.id.clone(), wk_a);
    stores.works.write().unwrap().insert(wk_b.id.clone(), wk_b);
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_a.id.clone(), bundle_a.clone());
    stores
        .bundles
        .write()
        .unwrap()
        .insert(bundle_b.id.clone(), bundle_b.clone());

    let result = classify_conflict(&stores, &[bundle_a.id.clone(), bundle_b.id.clone()]);
    assert!(result.is_none(), "empty paths should return None");
}

// --- resolve_plan_id tests ---

#[test]
fn test_resolve_plan_id_traverses_hierarchy() {
    let dir = TestDir::new("loopr-intg-rpid1");
    let stores = test_stores(&dir);

    let plan = crate::domain::plan::Plan::new("Test Plan".into(), "criteria".into());
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan.id.clone(), plan);

    let spec = crate::domain::spec::Spec::new(plan_id.clone(), "Test Spec".into());
    let spec_id = spec.id.clone();
    stores.specs.write().unwrap().insert(spec.id.clone(), spec);

    let phase = crate::domain::phase::Phase::new(spec_id, "Test Phase".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    let work = Work::new(phase_id, "Test Work".into());
    let work_id = work.id.clone();
    stores.works.write().unwrap().insert(work.id.clone(), work);

    let result = resolve_plan_id(&stores, &work_id);
    assert_eq!(result, Some(plan_id));
}

#[test]
fn test_resolve_plan_id_returns_none_for_orphan() {
    let dir = TestDir::new("loopr-intg-rpid2");
    let stores = test_stores(&dir);

    let work = Work::new("nonexistent-phase".into(), "Orphan Work".into());
    let work_id = work.id.clone();
    stores.works.write().unwrap().insert(work.id.clone(), work);

    let result = resolve_plan_id(&stores, &work_id);
    assert!(result.is_none(), "orphaned work should return None");
}

#[test]
fn test_integration_branch_name() {
    assert_eq!(integration_branch_name("pl-abc12"), "integration/pl-abc12");
}

// --- combine_conflicting_works tests ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_combine_conflicting_works_creates_combined_work() {
    let dir = TestDir::new("loopr-intg-combine1");
    let stores = test_stores(&dir);
    let intg = test_integrator(&dir, stores.clone(), test_config());

    // Set up hierarchy: phase with two conflicting works.
    let phase = crate::domain::phase::Phase::new("spec-1".into(), "Phase 1".into());
    let phase_id = phase.id.clone();
    stores.phases.write().unwrap().insert(phase.id.clone(), phase);

    let mut wk_a = Work::new(phase_id.clone(), "get_db_path + init_db".into());
    wk_a.acceptance_criteria = vec!["AC-A1".to_string(), "AC-A2".to_string()].into_iter().collect();
    wk_a.force_status(WorkStatus::InReview);
    let wk_a_id = wk_a.id.clone();

    let mut wk_b = Work::new(phase_id.clone(), "create_bookmark + list_bookmarks".into());
    wk_b.acceptance_criteria = vec!["AC-B1".to_string()].into_iter().collect();
    wk_b.force_status(WorkStatus::InReview);
    let wk_b_id = wk_b.id.clone();

    // Third work depends on wk_a (should get rewired).
    let mut wk_c = Work::new(phase_id.clone(), "test_suite".into());
    wk_c.dependencies = vec![wk_a_id.clone()];
    wk_c.force_status(WorkStatus::Ready);
    let wk_c_id = wk_c.id.clone();

    if let Some(ref store) = stores.store {
        let mut s = store.lock().unwrap();
        s.create(wk_a.clone()).unwrap();
        s.create(wk_b.clone()).unwrap();
        s.create(wk_c.clone()).unwrap();
    }
    stores.works.write().unwrap().insert(wk_a_id.clone(), wk_a);
    stores.works.write().unwrap().insert(wk_b_id.clone(), wk_b);
    stores.works.write().unwrap().insert(wk_c_id.clone(), wk_c);

    // Create bundles for the conflicting works.
    let mut bd_a = Bundle::new(wk_a_id.clone(), None, "branch-a".into(), vec![]);
    bd_a.paths = vec!["src/database.py".into()];
    bd_a.force_status(crate::domain::bundle::BundleStatus::Rejected);
    let bd_a_id = bd_a.id.clone();
    let mut bd_b = Bundle::new(wk_b_id.clone(), None, "branch-b".into(), vec![]);
    bd_b.paths = vec!["src/database.py".into()];
    bd_b.force_status(crate::domain::bundle::BundleStatus::Rejected);
    let bd_b_id = bd_b.id.clone();
    stores.bundles.write().unwrap().insert(bd_a.id.clone(), bd_a);
    stores.bundles.write().unwrap().insert(bd_b.id.clone(), bd_b);

    let conflicting_work_ids: HashSet<String> = [wk_a_id.clone(), wk_b_id.clone()].into();
    let conflicting_files: HashSet<String> = ["src/database.py".to_string()].into();

    intg.combine_conflicting_works(&conflicting_work_ids, &conflicting_files, &[bd_a_id, bd_b_id]);

    // Verify: original works are Abandoned.
    let works = stores.works.read().unwrap();
    assert_eq!(works[&wk_a_id].status(), WorkStatus::Abandoned);
    assert_eq!(works[&wk_b_id].status(), WorkStatus::Abandoned);

    // Verify: a new combined work was created in the same phase with merged AC.
    let combined = works
        .values()
        .find(|w| w.parent_id == phase_id && w.id != wk_a_id && w.id != wk_b_id && w.id != wk_c_id);
    assert!(combined.is_some(), "combined work should be created");
    let combined = combined.unwrap();
    assert!(combined.title.contains('+'), "combined title should join originals");
    assert!(combined.acceptance_criteria.0.contains(&"AC-A1".to_string()));
    assert!(combined.acceptance_criteria.0.contains(&"AC-B1".to_string()));
    assert_eq!(combined.status(), WorkStatus::Ready, "combined work starts Ready");

    // Verify: wk_c's dependency on wk_a was rewired to the combined work.
    let wk_c_updated = &works[&wk_c_id];
    assert!(!wk_c_updated.dependencies.contains(&wk_a_id), "old dep should be gone");
    assert!(
        wk_c_updated.dependencies.contains(&combined.id),
        "new dep should point to combined work"
    );
}
