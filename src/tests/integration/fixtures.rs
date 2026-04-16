#![allow(clippy::unwrap_used, dead_code, unused_imports)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::{AgentKind, AgentSession};
use crate::config::{Config, IntegratorConfig};
use crate::daemon::context::Stores;
use crate::daemon::handlers::dispatch;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
use crate::worktree::manager::WorktreeManager;

// Type aliases for inject_preformed_plan
pub(super) type WorkInput<'a> = (&'a str, &'a str, Vec<&'a str>);
pub(super) type PhaseInput<'a> = (&'a str, &'a str, u32, Vec<WorkInput<'a>>);
pub(super) type SpecInput<'a> = (&'a str, &'a str, Vec<PhaseInput<'a>>);
pub(super) type PhaseResult = (String, Vec<String>);
pub(super) type SpecResult = (String, Vec<PhaseResult>);

pub(super) struct PlanInput<'a> {
    pub title: &'a str,
    pub desc: &'a str,
    pub criteria: &'a str,
    pub specs: Vec<SpecInput<'a>>,
}

pub(super) fn test_stores() -> Arc<Stores> {
    Arc::new(Stores::new())
}

pub(super) fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
    let (tx, _) = broadcast::channel(64);
    tx
}

pub(super) fn test_worktree_mgr() -> WorktreeManager {
    WorktreeManager::new(
        PathBuf::from("/nonexistent/repo"),
        PathBuf::from("/nonexistent/worktrees"),
    )
}

/// Build a minimal AgentContext for integration tests calling execute_action.
pub(super) fn test_agent_context(
    stores: &Arc<Stores>,
    bridge: crate::agents::bridge::AgentIpcBridge,
    tx: broadcast::Sender<DaemonEvent>,
) -> crate::agents::AgentContext {
    let session = AgentSession::new(AgentKind::Director, "test-model".into());
    crate::agents::AgentContext {
        session,
        stores: stores.clone(),
        bridge,
        event_tx: tx,
        tool_runner: stores.read_tool_runner().unwrap(),
        tool_executor: stores.read_tool_executor().unwrap(),
        read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
    }
}

pub(super) fn test_integrator_config() -> IntegratorConfig {
    IntegratorConfig {
        validation_commands: vec!["echo ok".to_string()],
        ..Default::default()
    }
}

pub(super) fn test_fsm() -> FsmInterpreter {
    FsmInterpreter::embedded().unwrap()
}

/// Helper: dispatch a request and assert success, returning the result JSON.
pub(super) async fn dispatch_ok(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let fsm = test_fsm();
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, &fsm, req).await;
    assert!(!resp.is_error(), "{method} failed: {:?}", resp.error);
    resp.result.unwrap()
}

/// Helper: dispatch a request and assert error, returning the error code.
pub(super) async fn dispatch_err(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    method: &str,
    params: serde_json::Value,
) -> i32 {
    let fsm = test_fsm();
    let req = DaemonRequest::new(1, method, params);
    let resp = dispatch(stores, tx, wm, ic, &fsm, req).await;
    assert!(resp.is_error(), "{method} expected error but got success");
    resp.error.unwrap().code
}

/// Helper: create Plan->Spec->Phase hierarchy and return (plan_id, spec_id, phase_id).
/// NOTE: variable names plan_id, spec_id, phase_id are kept here because they
/// represent the *identity* of these records, not a parent reference.
pub(super) async fn create_test_hierarchy(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
) -> (String, String, String) {
    let plan = dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "plan.create",
        json!({"title": "Test Plan", "description": "desc", "acceptance_criteria": "pass"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;
    let spec = dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "spec.create",
        json!({"parent_id": plan_id, "title": "Test Spec", "description": "desc", "acceptance_criteria": "pass"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap().to_string();
    dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    )
    .await;
    let phase = dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "phase.create",
        json!({"parent_id": spec_id, "title": "Test Phase", "description": "desc", "acceptance_criteria": "pass"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap().to_string();
    dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "phase.transition",
        json!({"id": phase_id, "target_status": "active"}),
    )
    .await;
    (plan_id, spec_id, phase_id)
}

/// Seed a coordinator goal (no-op after CoordinatorGoal removal).
/// Returns an empty string. Retained for call-site compatibility.
pub(super) fn seed_goal(_stores: &Arc<Stores>, _goal_text: &str) -> String {
    String::new()
}

pub(super) fn test_stores_with_persistence(dir: &std::path::Path) -> Arc<Stores> {
    let config = Config {
        project: crate::config::ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..crate::config::ProjectConfig::default()
        },
        ..Config::default()
    };
    let store = taskstore::Store::open(dir).unwrap();
    let mut stores = Stores::new();
    stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
    stores.config = config;
    Arc::new(stores)
}

/// Helper: inject a pre-formed plan with specs, phases, and works, all transitioned to active.
/// Returns (plan_id, vec of (spec_id, vec of (phase_id, vec of work_ids))).
pub(super) async fn inject_preformed_plan(
    stores: &Arc<Stores>,
    tx: &broadcast::Sender<DaemonEvent>,
    wm: &WorktreeManager,
    ic: &IntegratorConfig,
    input: PlanInput<'_>,
) -> (String, Vec<SpecResult>) {
    let plan = dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "plan.create",
        json!({
            "title": input.title,
            "description": input.desc,
            "acceptance_criteria": input.criteria,
        }),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        stores,
        tx,
        wm,
        ic,
        "plan.transition",
        json!({
            "id": plan_id, "target_status": "active"
        }),
    )
    .await;

    let mut spec_results = Vec::new();
    for (spec_title, spec_desc, phases) in input.specs {
        let spec = dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "spec.create",
            json!({
                "parent_id": plan_id,
                "title": spec_title,
                "description": spec_desc,
                "acceptance_criteria": "all tests pass",
            }),
        )
        .await;
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "spec.transition",
            json!({
                "id": spec_id, "target_status": "active"
            }),
        )
        .await;

        let mut phase_results = Vec::new();
        for (phase_title, phase_desc, order, works) in &phases {
            let phase = dispatch_ok(
                stores,
                tx,
                wm,
                ic,
                "phase.create",
                json!({
                    "parent_id": spec_id,
                    "title": phase_title,
                    "description": phase_desc,
                    "order": order,
                }),
            )
            .await;
            let phase_id = phase["id"].as_str().unwrap().to_string();
            dispatch_ok(
                stores,
                tx,
                wm,
                ic,
                "phase.transition",
                json!({
                    "id": phase_id, "target_status": "active"
                }),
            )
            .await;

            let mut work_ids = Vec::new();
            for (work_title, work_desc, files) in works {
                let work = dispatch_ok(
                    stores,
                    tx,
                    wm,
                    ic,
                    "work.create",
                    json!({
                        "parent_id": phase_id,
                        "title": work_title,
                        "description": work_desc,
                        "files": files,
                        "acceptance_criteria": ["tests pass"],
                    }),
                )
                .await;
                work_ids.push(work["id"].as_str().unwrap().to_string());
            }
            phase_results.push((phase_id, work_ids));
        }
        spec_results.push((spec_id, phase_results));
    }
    (plan_id, spec_results)
}
