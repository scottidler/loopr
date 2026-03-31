use std::sync::Arc;

use log::debug;
use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::AgentType;
use crate::config::IntegratorConfig;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use super::context::Stores;

/// Convert a handler body that returns Result<DaemonResponse> into a DaemonResponse,
/// mapping any Err into an RPC internal error response.
macro_rules! try_handler {
    ($req_id:expr, $body:expr) => {{
        #[allow(clippy::redundant_closure_call)]
        let __result = (|| -> eyre::Result<DaemonResponse> { $body })();
        match __result {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::err($req_id, RpcError::internal(&e.to_string())),
        }
    }};
}

/// Returns the configured max_pool for a given agent type.
fn max_pool_for(agent_type: AgentType, config: &crate::config::Config) -> u32 {
    match agent_type {
        AgentType::Implementer => config.agents.implementer.max_pool,
        AgentType::Reviewer => config.agents.reviewer.max_pool,
        AgentType::Coordinator => config.agents.coordinator.role.max_pool,
        AgentType::Researcher => config.agents.researcher.max_pool,
        AgentType::Integrator => 1,
        AgentType::Chat => 1, // Single chat session for now
    }
}

mod agent;
mod bundle;
mod chat;
mod common;
mod coordinator;
mod integrator;
mod learning;
mod lock;
mod phase;
mod plan;
mod spec;
mod system;
mod tick;
mod work;
mod worktree;

use agent::*;
use bundle::*;
use chat::*;
use coordinator::*;
use integrator::*;
use learning::*;
use lock::*;
use phase::*;
use plan::*;
use spec::*;
use system::*;
use tick::*;
use work::*;
use worktree::*;

/// Dispatch an IPC request to the appropriate handler.
/// This is the central routing function for all daemon request handling.
pub fn dispatch(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    debug!("dispatch(method={})", req.method);
    let method = req.method.clone();
    let params = req.params.clone();
    let resp = match req.method.as_str() {
        "system.handshake" => handle_handshake(stores, req),
        "system.init" => handle_system_init(stores, req),
        "system.status" => handle_status(stores, req),
        "system.shutdown" => handle_shutdown(event_tx, req),
        "plan.create" => handle_plan_create(stores, event_tx, req),
        "plan.get" => handle_plan_get(stores, req),
        "plan.list" => handle_plan_list(stores, req),
        "plan.transition" => handle_plan_transition(stores, event_tx, req),
        "plan.update" => handle_plan_update(stores, event_tx, req),
        "spec.create" => handle_spec_create(stores, event_tx, req),
        "spec.get" => handle_spec_get(stores, req),
        "spec.list" => handle_spec_list(stores, req),
        "spec.transition" => handle_spec_transition(stores, event_tx, req),
        "spec.update" => handle_spec_update(stores, event_tx, req),
        "phase.create" => handle_phase_create(stores, event_tx, req),
        "phase.get" => handle_phase_get(stores, req),
        "phase.list" => handle_phase_list(stores, req),
        "phase.transition" => handle_phase_transition(stores, event_tx, req),
        "phase.update" => handle_phase_update(stores, event_tx, req),
        "work.create" => handle_work_create(stores, event_tx, req),
        "work.get" => handle_work_get(stores, req),
        "work.list" => handle_work_list(stores, req),
        "work.transition" => handle_work_transition(stores, event_tx, req),
        "work.update" => handle_work_update(stores, event_tx, req),
        "bundle.create" => handle_bundle_create(stores, event_tx, req),
        "bundle.get" => handle_bundle_get(stores, req),
        "bundle.list" => handle_bundle_list(stores, req),
        "bundle.transition" => handle_bundle_transition(stores, event_tx, req),
        "bundle.update" => handle_bundle_update(stores, event_tx, req),
        "tick.create" => handle_tick_create(stores, event_tx, req),
        "tick.get" => handle_tick_get(stores, req),
        "tick.list" => handle_tick_list(stores, req),
        "tick.transition" => handle_tick_transition(stores, event_tx, req),
        "tick.update" => handle_tick_update(stores, event_tx, req),
        "learning.create" => handle_learning_create(stores, event_tx, req),
        "learning.get" => handle_learning_get(stores, req),
        "learning.list" => handle_learning_list(stores, req),
        "learning.update" => handle_learning_update(stores, event_tx, req),
        "learning.reinforce" => handle_learning_reinforce(stores, event_tx, req),
        "learning.contradict" => handle_learning_contradict(stores, event_tx, req),
        "learning.promote" => handle_learning_promote(stores, event_tx, req),
        "learning.demote" => handle_learning_demote(stores, event_tx, req),
        "lock.create" => handle_lock_create(stores, event_tx, req),
        "lock.get" => handle_lock_get(stores, req),
        "lock.list" => handle_lock_list(stores, req),
        "lock.release" => handle_lock_release(stores, event_tx, req),
        "lock.expire" => handle_lock_expire(stores, event_tx, req),
        "worktree.create" => handle_worktree_create(stores, event_tx, worktree_mgr, req),
        "worktree.list" => handle_worktree_list(worktree_mgr, req),
        "worktree.cleanup" => handle_worktree_cleanup(stores, event_tx, worktree_mgr, req),
        "worktree.refresh" => handle_worktree_refresh(worktree_mgr, req),
        "integrator.validate" => handle_integrator_validate(stores, event_tx, integrator_config, req),
        "integrator.publish" => handle_integrator_publish(stores, event_tx, integrator_config, req),
        "validator.validate" => handle_validator_validate(stores, req),
        "validator.report" => handle_validator_report(stores, req),
        "validator.reports" => handle_validator_reports(stores, req),
        "coverage.evaluate" => handle_coverage_evaluate(stores, req),
        "tool.list" => handle_tool_list(stores, req),
        "coordinator.set_goal" => handle_coordinator_set_goal(stores, event_tx, req),
        "coordinator.clear_goal" => handle_coordinator_clear_goal(stores, event_tx, req),
        "coordinator.get_goal" => handle_coordinator_get_goal(stores, req),
        "coordinator.get_state" => handle_coordinator_get_state(stores, req),
        "coordinator.reset_state" => handle_coordinator_reset_state(stores, event_tx, req),
        "coordinator.interview_respond" => handle_coordinator_interview_respond(stores, event_tx, req),
        "coordinator.accept_plan" => {
            handle_coordinator_accept_plan(stores, event_tx, worktree_mgr, integrator_config, req)
        }
        "coordinator.interview_question" => handle_coordinator_interview_question(stores, event_tx, req),
        "chat.submit" => handle_chat_submit(stores, event_tx, req),
        "chat.attach" => handle_chat_attach(stores, req),
        "chat.history" => handle_chat_history(stores, req),
        "agent.start" => handle_agent_start(stores, event_tx, worktree_mgr, req),
        "agent.stop" => handle_agent_stop(stores, event_tx, req),
        "agent.pause" => handle_agent_pause(stores, event_tx, req),
        "agent.resume" => handle_agent_resume(stores, event_tx, req),
        "agent.status" => handle_agent_status(stores, req),
        "agent.list" => handle_agent_list(stores, req),
        "agent.output" => handle_agent_output(stores, req),
        _ => DaemonResponse::err(req.id, RpcError::method_not_found(&req.method)),
    };

    // Gap #29: Post-dispatch auto-start hook (only on successful transitions)
    if !resp.is_error() {
        auto_start_agents(stores, event_tx, worktree_mgr, integrator_config, &method, &params);
    }

    resp
}

/// Auto-start agents based on transition outcomes (Gap #29).
fn auto_start_agents(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    method: &str,
    params: &serde_json::Value,
) {
    if method == "work.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target == "InProgress"
        && stores.config.agents.auto_start_implementer
        && !stores.config.agents.pull_based_workers  // Workers handle their own spawning
        && let Some(wi_id) = params.get("id").and_then(|v| v.as_str())
    {
        let start_req = DaemonRequest::new(
            0,
            "agent.start",
            json!({
                "agent_type": "implementer", "work_id": wi_id,
            }),
        );
        let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
    }
    if method == "bundle.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target == "Triaged"
        && stores.config.agents.auto_start_reviewer
        && let Some(bid) = params.get("id").and_then(|v| v.as_str())
        && bid.starts_with("bd-")
    {
        let start_req = DaemonRequest::new(
            0,
            "agent.start",
            json!({
                "agent_type": "reviewer", "bundle_id": bid,
            }),
        );
        let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use super::*;

    use crate::agents::{AgentSession, AgentStatus, AgentType};
    use crate::config::IntegratorConfig;
    use crate::domain::bundle::Bundle;
    use crate::domain::learning::Learning;
    use crate::domain::lock::Lock;
    use crate::domain::phase::{Phase, PhaseStatus};
    use crate::domain::plan::{HierarchyStatus, Plan, PlanStatus};
    use crate::domain::spec::{Spec, SpecStatus};
    use crate::domain::tick::{Tick, TickStatus};
    use crate::domain::validation::{ValidationReport, ValidationVerdict};
    use crate::domain::work::{Work, WorkStatus};
    use crate::test_util::TestDir;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_stores_with_taskstore() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        // Initialize a git repo so install_git_hooks works
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<Spec>().unwrap();
        store.rebuild_indexes::<Phase>().unwrap();
        store.rebuild_indexes::<Work>().unwrap();
        store.rebuild_indexes::<Bundle>().unwrap();
        store.rebuild_indexes::<Tick>().unwrap();
        store.rebuild_indexes::<Learning>().unwrap();
        store.rebuild_indexes::<Lock>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        (dir, Arc::new(stores))
    }

    /// Creates stores with TaskStore AND a validator (DocValidator placeholder via Arc).
    /// This activates the validation gate for Draft → Active transitions.
    fn test_stores_with_validator_strictness(strictness: crate::config::ValidatorStrictness) -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        let validator_config = crate::config::ValidatorConfig {
            enabled: true,
            api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
            ..crate::config::ValidatorConfig::default()
        };
        stores.validator = Some(Arc::new(crate::validator::DocValidator::new(validator_config)));
        stores.config.strategy.validator_strictness = strictness;
        (dir, Arc::new(stores))
    }

    fn test_stores_with_validator() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<Spec>().unwrap();
        store.rebuild_indexes::<Phase>().unwrap();
        store.rebuild_indexes::<Work>().unwrap();
        store.rebuild_indexes::<Bundle>().unwrap();
        store.rebuild_indexes::<Tick>().unwrap();
        store.rebuild_indexes::<Learning>().unwrap();
        store.rebuild_indexes::<Lock>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        // Create a DocValidator to enable the validation gate.
        // Tests don't call the LLM — they only check that the gate logic works.
        let validator_config = crate::config::ValidatorConfig {
            enabled: true,
            api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
            ..crate::config::ValidatorConfig::default()
        };
        stores.validator = Some(Arc::new(crate::validator::DocValidator::new(validator_config)));
        (dir, Arc::new(stores))
    }

    fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(16);
        tx
    }

    fn test_worktree_mgr() -> WorktreeManager {
        WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        )
    }

    fn test_integrator_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        }
    }

    // --- dispatch tests ---

    #[test]
    fn test_dispatch_unknown_method() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "unknown.method", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unknown.method"));
    }

    #[test]
    fn test_dispatch_handshake() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.handshake", json!({"client_version": "0.1.0"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["protocol"], "ndjson/1");
    }

    #[test]
    fn test_dispatch_status_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["version"].is_string());
        assert!(result["pid"].is_number());
        assert_eq!(result["counts"]["plans"], 0);
        assert_eq!(result["counts"]["works"], 0);
        // Without TaskStore, reports disabled
        assert_eq!(result["taskstore"]["enabled"], false);
    }

    #[test]
    fn test_dispatch_status_with_records() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Insert a plan
        let plan = Plan::new("Test".into(), "".into(), "".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);
        // Insert a work item
        let wi = Work::new("p-1".into(), "WI".into(), "".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["counts"]["plans"], 1);
        assert_eq!(result["counts"]["works"], 1);
        assert_eq!(result["counts"]["specs"], 0);
    }

    #[test]
    fn test_dispatch_status_with_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan via TaskStore directly
        let plan = Plan::new("TS Plan".into(), "".into(), "".into());
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(plan.clone())
            .unwrap();

        // Create a spec via TaskStore directly
        let spec = Spec::new(plan.id.clone(), "TS Spec".into(), "".into());
        stores.store.as_ref().unwrap().lock().unwrap().create(spec).unwrap();

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();

        // TaskStore should be enabled and show counts
        assert_eq!(result["taskstore"]["enabled"], true);
        assert_eq!(result["taskstore"]["counts"]["plans"], 1);
        assert_eq!(result["taskstore"]["counts"]["specs"], 1);
        assert_eq!(result["taskstore"]["counts"]["phases"], 0);
        assert_eq!(result["taskstore"]["counts"]["works"], 0);

        // HashMap counts should be 0 (we only wrote to TaskStore)
        assert_eq!(result["counts"]["plans"], 0);
    }

    #[test]
    fn test_dispatch_shutdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.shutdown", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "shutting_down");
        // Verify event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "system.shutdown");
    }

    // --- plan.create tests ---

    #[test]
    fn test_plan_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.create",
            json!({
                "title": "Test Plan",
                "description": "A test",
                "acceptance_criteria": "It works"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Plan");
        assert_eq!(result["status"], "draft");
        // Should be stored
        assert_eq!(stores.plans.read().unwrap().len(), 1);
    }

    #[test]
    fn test_plan_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.create", json!({"description": "no title"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_plan_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let req = DaemonRequest::new(1, "plan.create", json!({"title": "Plan"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "plan");
    }

    #[test]
    fn test_plan_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.create",
            json!({"title": "Persisted Plan", "description": "desc"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plan_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Plan> = store.get(&plan_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Plan");
    }

    #[test]
    fn test_plan_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create first Draft Plan — succeeds
        let req1 = DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Plan — rejected
        let req2 = DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005); // precondition_failed
    }

    // --- plan.get tests ---

    #[test]
    fn test_plan_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a plan first
        let create_req = DaemonRequest::new(1, "plan.create", json!({"title": "My Plan"}));
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Get it
        let get_req = DaemonRequest::new(2, "plan.get", json!({"id": plan_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Plan");
    }

    #[test]
    fn test_plan_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_get_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.get", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("id"));
    }

    #[test]
    fn test_plan_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            1,
            "plan.create",
            json!({"title": "TaskStore Plan", "description": "persistent"}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.plans.write().unwrap().remove(&plan_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(2, "plan.get", json!({"id": plan_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Plan");
    }

    // --- plan.list tests ---

    #[test]
    fn test_plan_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert!(plans.is_array());
        assert_eq!(plans.as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_plan_list_with_plans() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create first plan and abandon it so a second Draft can be created
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
        );
        let plan_a_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "plan.transition",
                json!({"id": plan_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"})),
        );

        let req = DaemonRequest::new(3, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert_eq!(plans.as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_plan_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create first plan and abandon it so a second Draft can be created
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan A"})),
        );
        let plan_a_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "plan.transition",
                json!({"id": plan_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "plan.create", json!({"title": "Plan B"})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.plans.write().unwrap().clear();

        // List should still return both plans via TaskStore
        let req = DaemonRequest::new(3, "plan.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let plans = resp.result.unwrap();
        assert_eq!(plans.as_array().unwrap().len(), 2);
    }

    // --- plan.transition tests ---

    #[test]
    fn test_plan_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        // Create plan
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let _ = rx.try_recv(); // consume create event
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Check event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_plan_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try to skip Draft → Complete (invalid: must go through Active)
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition plans
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_plan_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "plan.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_plan_transition_missing_params() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Missing id
        let req = DaemonRequest::new(1, "plan.transition", json!({"target_status": "active"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());

        // Missing target_status
        let req = DaemonRequest::new(2, "plan.transition", json!({"id": "x"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_plan_transition_default_role_coordinator() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // No role specified — defaults to Coordinator, which is valid for hierarchy transitions
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create plan (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Transition Plan"})),
        );
        assert!(!create_resp.is_error());
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            2,
            "plan.transition",
            json!({
                "id": plan_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Plan> = store.get(&plan_id).unwrap();
        assert!(retrieved.is_some());
        let plan = retrieved.unwrap();
        assert_eq!(plan.status, PlanStatus::Active);
    }

    // --- spec.create tests ---

    /// Helper: create a plan and return its id
    fn create_test_plan(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_spec_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({
                "plan_id": plan_id,
                "title": "Test Spec",
                "description": "A spec"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Spec");
        assert_eq!(result["plan_id"], plan_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(stores.specs.read().unwrap().len(), 1);
    }

    #[test]
    fn test_spec_create_missing_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("plan_id"));
    }

    #[test]
    fn test_spec_create_plan_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.create", json!({"plan_id": "nonexistent", "title": "Spec"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "description": "no title"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_spec_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let _ = rx.try_recv(); // consume plan create event

        let req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "spec");
    }

    #[test]
    fn test_spec_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Persisted Spec", "description": "desc"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Spec");
    }

    // --- parent status validation tests ---

    #[test]
    fn test_spec_create_rejects_complete_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Transition plan: Draft → Active → Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "complete", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec Under Complete"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete plan"));
    }

    #[test]
    fn test_spec_create_rejects_abandoned_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Transition plan: Draft → Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec Under Abandoned"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned plan"));
    }

    #[test]
    fn test_spec_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        // Create first Draft Spec — succeeds
        let req1 = DaemonRequest::new(1, "spec.create", json!({"plan_id": plan_id, "title": "Spec A"}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Spec under same Plan — rejected
        let req2 = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec B"}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    #[test]
    fn test_phase_create_rejects_duplicate_draft() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Create first Draft Phase — succeeds
        let req1 = DaemonRequest::new(
            1,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase A", "order": 1}),
        );
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());

        // Create second Draft Phase under same Spec — rejected
        let req2 = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase B", "order": 2}),
        );
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(resp2.is_error());
        assert_eq!(resp2.error.unwrap().code, -32005);
    }

    #[test]
    fn test_phase_create_rejects_complete_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Transition spec: Draft → Active → Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "spec.transition",
                json!({"id": spec_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "spec.transition",
                json!({"id": spec_id, "target_status": "complete", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase Under Complete", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete spec"));
    }

    #[test]
    fn test_phase_create_rejects_abandoned_spec() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Transition spec: Draft → Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "spec.transition",
                json!({"id": spec_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase Under Abandoned", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned spec"));
    }

    #[test]
    fn test_work_create_rejects_complete_phase() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft → Active → Complete
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "complete", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "work.create",
            json!({"phase_id": phase_id, "title": "Work Under Complete", "resource_tags": ["src/"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("complete phase"));
    }

    #[test]
    fn test_work_create_rejects_abandoned_phase() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Transition phase: Draft → Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "phase.transition",
                json!({"id": phase_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "work.create",
            json!({"phase_id": phase_id, "title": "Work Under Abandoned", "resource_tags": ["src/"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("abandoned phase"));
    }

    #[test]
    fn test_bundle_create_rejects_done_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Directly set work status to Done via the HashMap (bypasses transition preconditions)
        {
            let mut works = stores.works.write().unwrap();
            let work = works.get_mut(&wi_id).unwrap();
            work.status = WorkStatus::Done;
        }

        let req = DaemonRequest::new(
            2,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/late"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Done work"));
    }

    #[test]
    fn test_bundle_create_rejects_abandoned_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Transition work: Ready → Abandoned
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "work.transition",
                json!({"id": wi_id, "target_status": "Abandoned", "role": "coordinator"}),
            ),
        );

        let req = DaemonRequest::new(
            2,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/abandoned"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Abandoned work"));
    }

    // --- spec.get tests ---

    #[test]
    fn test_spec_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "My Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.get", json!({"id": spec_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My Spec");
    }

    #[test]
    fn test_spec_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Create a spec (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "TaskStore Spec"}));
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.specs.write().unwrap().remove(&spec_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(3, "spec.get", json!({"id": spec_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Spec");
    }

    // --- spec.list tests ---

    #[test]
    fn test_spec_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_spec_list_filtered_by_plan_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id_1 = create_test_plan(&stores, &tx, &wm);

        // Activate first plan so we can create a second Draft Plan
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                9,
                "plan.transition",
                json!({"id": plan_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "plan.create", json!({"title": "Plan 2"})),
        );
        let plan_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create specs under different plans
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id_1, "title": "Spec A"})),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"plan_id": plan_id_2, "title": "Spec B"})),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "spec.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by plan_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(5, "spec.list", json!({"plan_id": plan_id_1})),
        );
        let specs = filtered_resp.result.unwrap();
        let arr = specs.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Spec A");
    }

    #[test]
    fn test_spec_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan first
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan X"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create first spec, abandon it, then create second
        let spec_a_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec A"})),
        );
        let spec_a_id = spec_a_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "spec.transition",
                json!({"id": spec_a_id, "target_status": "abandoned", "role": "coordinator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "spec.create", json!({"plan_id": plan_id, "title": "Spec B"})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.specs.write().unwrap().clear();

        // List should still return both specs via TaskStore
        let req = DaemonRequest::new(4, "spec.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let specs = resp.result.unwrap();
        assert_eq!(specs.as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(5, "spec.list", json!({"plan_id": plan_id}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_specs = filtered_resp.result.unwrap();
        assert_eq!(filtered_specs.as_array().unwrap().len(), 2);
    }

    // --- spec.transition tests ---

    #[test]
    fn test_spec_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let plan_id = create_test_plan(&stores, &tx, &wm);
        let _ = rx.try_recv(); // consume plan create event

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let _ = rx.try_recv(); // consume spec create event
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "spec");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_spec_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Spec"})),
        );
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_spec_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "spec.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_spec_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        // Create spec (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.create",
                json!({"plan_id": plan_id, "title": "Transition Spec"}),
            ),
        );
        assert!(!create_resp.is_error());
        let spec_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            3,
            "spec.transition",
            json!({
                "id": spec_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Spec> = store.get(&spec_id).unwrap();
        assert!(retrieved.is_some());
        let spec = retrieved.unwrap();
        assert_eq!(spec.status, SpecStatus::Active);
    }

    // --- phase handlers ---

    /// Helper: create a plan + spec and return (plan_id, spec_id)
    fn create_test_spec(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let plan_id = create_test_plan(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
        let spec_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id)
    }

    #[test]
    fn test_phase_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({
                "spec_id": spec_id,
                "title": "Test Phase",
                "description": "A phase",
                "order": 1
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Test Phase");
        assert_eq!(result["spec_id"], spec_id);
        assert_eq!(result["status"], "draft");
        assert_eq!(result["order"], 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
    }

    #[test]
    fn test_phase_create_missing_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("spec_id"));
    }

    #[test]
    fn test_phase_create_spec_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.create", json!({"spec_id": "nonexistent", "title": "Phase"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "description": "no title"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_phase_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "phase");
    }

    #[test]
    fn test_phase_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Persisted Phase", "description": "desc", "order": 1}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted Phase");
    }

    #[test]
    fn test_phase_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "My Phase", "order": 3}),
            ),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(21, "phase.get", json!({"id": phase_id})),
        );
        assert!(!get_resp.is_error());
        let result = get_resp.result.unwrap();
        assert_eq!(result["title"], "My Phase");
        assert_eq!(result["order"], 3);
    }

    #[test]
    fn test_phase_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Create a phase (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            20,
            "phase.create",
            json!({"spec_id": spec_id, "title": "TaskStore Phase", "order": 1}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.phases.write().unwrap().remove(&phase_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(21, "phase.get", json!({"id": phase_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore Phase");
    }

    #[test]
    fn test_phase_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "phase.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_phase_list_filtered_by_spec_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

        // Activate first spec so we can create a second Draft Spec (and phases under both)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "spec.transition",
                json!({"id": spec_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second spec under the same plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"plan_id": plan_id, "title": "Spec 2"})),
        );
        let spec_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create phases under different specs
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by spec_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "phase.list", json!({"spec_id": spec_id_1})),
        );
        let phases = filtered_resp.result.unwrap();
        let arr = phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[test]
    fn test_phase_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (plan_id, spec_id_1) = create_test_spec(&stores, &tx, &wm);

        // Activate first spec so we can create a second Draft Spec
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                10,
                "spec.transition",
                json!({"id": spec_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second spec under the same plan
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "spec.create", json!({"plan_id": plan_id, "title": "Spec 2"})),
        );
        let spec_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create phases under different specs (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id_1, "title": "Phase A", "order": 1}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id_2, "title": "Phase B", "order": 1}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.phases.write().unwrap().clear();

        // List all should still return both phases via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(30, "phase.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(31, "phase.list", json!({"spec_id": spec_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_phases = filtered_resp.result.unwrap();
        let arr = filtered_phases.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "Phase A");
    }

    #[test]
    fn test_phase_transition_draft_to_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);
        // Drain plan+spec create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let _ = rx.try_recv(); // consume phase create event
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "phase");
        assert_eq!(event.data["from"], "draft");
        assert_eq!(event.data["to"], "active");
    }

    #[test]
    fn test_phase_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "complete",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(20, "phase.create", json!({"spec_id": spec_id, "title": "Phase"})),
        );
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            21,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_phase_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "phase.transition",
            json!({
                "id": "nonexistent",
                "target_status": "active"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_phase_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, spec_id) = create_test_spec(&stores, &tx, &wm);

        // Create phase (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Transition Phase"}),
            ),
        );
        assert!(!create_resp.is_error());
        let phase_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition Draft → Active
        let req = DaemonRequest::new(
            3,
            "phase.transition",
            json!({
                "id": phase_id,
                "target_status": "active",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Phase> = store.get(&phase_id).unwrap();
        assert!(retrieved.is_some());
        let phase = retrieved.unwrap();
        assert_eq!(phase.status, PhaseStatus::Active);
    }

    // --- work handlers ---

    /// Helper: create a plan + spec + phase and return (plan_id, spec_id, phase_id)
    fn create_test_phase(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String, String) {
        let (plan_id, spec_id) = create_test_spec(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                20,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Parent Phase", "order": 1}),
            ),
        );
        let phase_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (plan_id, spec_id, phase_id)
    }

    #[test]
    fn test_work_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement auth",
                "description": "Add JWT signing"
            , "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Implement auth");
        assert_eq!(result["phase_id"], phase_id);
        // Auto-promoted to Ready because acceptance_criteria were provided
        assert_eq!(result["status"], "Ready");
        assert_eq!(stores.works.read().unwrap().len(), 1);
    }

    #[test]
    fn test_work_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "Persisted WI", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Work> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Persisted WI");
    }

    #[test]
    fn test_work_create_missing_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.create",
            json!({"title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("phase_id"));
    }

    #[test]
    fn test_work_create_phase_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.create",
            json!({"phase_id": "nonexistent", "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_create_missing_title() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "description": "no title", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("title"));
    }

    #[test]
    fn test_work_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "work");
    }

    #[test]
    fn test_work_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "My WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(31, "work.get", json!({"id": wi_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "My WI");
    }

    #[test]
    fn test_work_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create a work item (writes to both TaskStore and HashMap)
        let create_req = DaemonRequest::new(
            30,
            "work.create",
            json!({"phase_id": phase_id, "title": "TaskStore WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let create_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), create_req);
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.works.write().unwrap().remove(&wi_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(31, "work.get", json!({"id": wi_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["title"], "TaskStore WI");
    }

    #[test]
    fn test_work_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "work.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_work_list_filtered_by_phase_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

        // Activate first phase so we can create a second Draft Phase
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                15,
                "phase.transition",
                json!({"id": phase_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id_1, "title": "WI A", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": phase_id_2, "title": "WI B", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by phase_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(41, "work.list", json!({"phase_id": phase_id_1})),
        );
        let items = filtered_resp.result.unwrap();
        let arr = items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id_1) = create_test_phase(&stores, &tx, &wm);

        // Activate first phase so we can create a second Draft Phase
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                15,
                "phase.transition",
                json!({"id": phase_id_1, "target_status": "active", "role": "coordinator"}),
            ),
        );

        // Create a second phase under the same spec
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                21,
                "phase.create",
                json!({"spec_id": _spec_id, "title": "Phase 2", "order": 2}),
            ),
        );
        let phase_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create work items under different phases (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id_1, "title": "WI A", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": phase_id_2, "title": "WI B", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.works.write().unwrap().clear();

        // List all should still return both work items via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(40, "work.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(41, "work.list", json!({"phase_id": phase_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["title"], "WI A");
    }

    #[test]
    fn test_work_transition_draft_to_ready() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);
        // Drain plan+spec+phase create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        // With acceptance_criteria, WI is auto-promoted to Ready on creation
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let _ = rx.try_recv(); // consume work create event
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Already Ready — transition to InProgress (with assignee, required by precondition)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "agent-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "InProgress");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "work");
        assert_eq!(event.data["from"], "Ready");
        assert_eq!(event.data["to"], "InProgress");
    }

    #[test]
    fn test_work_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Ready → Done (invalid: must go through InProgress, InReview, Integrated)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "Done",
                "role": "coordinator"
            , "assignee": "agent-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Ready → InProgress (only Coordinator can)
        let req = DaemonRequest::new(
            31,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_work_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "work.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Ready"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_work_transition_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create work item (also persisted to TaskStore)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "work.create",
                json!({"phase_id": phase_id, "title": "Transition WI", "description": "Test", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        assert!(!create_resp.is_error());
        let wi_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Already Ready via auto-promotion (acceptance_criteria present) — transition to InProgress
        let req = DaemonRequest::new(
            3,
            "work.transition",
            json!({
                "id": wi_id,
                "target_status": "InProgress",
                "role": "coordinator",
                "assignee": "agent-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "InProgress");

        // Verify TaskStore has the updated status
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Work> = store.get(&wi_id).unwrap();
        assert!(retrieved.is_some());
        let wi = retrieved.unwrap();
        assert_eq!(wi.status, WorkStatus::InProgress);
    }

    // --- bundle handlers ---

    /// Helper: create plan + spec + phase + work and return (phase_id, work_id)
    fn create_test_work(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_plan_id, _spec_id, phase_id) = create_test_phase(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                30,
                "work.create",
                json!({"phase_id": phase_id, "title": "Parent WI", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (phase_id, wi_id)
    }

    /// Helper: create plan + spec + phase + work + bundle and return (work_id, bundle_id)
    fn create_test_bundle(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> (String, String) {
        let (_phase_id, wi_id) = create_test_work(stores, tx, wm);
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/test", "base_tick_id": null, "claims": "Initial claims"}),
            ),
        );
        assert!(!resp.is_error(), "bundle.create failed: {:?}", resp.error);
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        (wi_id, bundle_id)
    }

    /// Helper: create a tick and return its id
    fn create_test_tick(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        assert!(!resp.is_error(), "tick.create failed: {:?}", resp.error);
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_bundle_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/persist",
                "base_tick_id": "tick-001",
                "claims": "Persisted bundle"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Bundle> = store.get(&bundle_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().claims, vec!["Persisted bundle".to_string()]);
    }

    #[test]
    fn test_bundle_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "tick-001",
                "claims": "Add JWT signing"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["work_id"], wi_id);
        assert_eq!(result["branch_name"], "feature/auth");
        assert_eq!(result["base_tick_id"], "tick-001");
        assert_eq!(result["claims"], serde_json::json!(["Add JWT signing"]));
        assert_eq!(result["status"], "Proposed");
        assert_eq!(stores.bundles.read().unwrap().len(), 1);
    }

    #[test]
    fn test_bundle_create_no_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["base_tick_id"].is_null());
    }

    #[test]
    fn test_bundle_create_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.create", json!({"branch_name": "feature/x"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_bundle_create_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "bundle.create",
            json!({"work_id": "nonexistent", "branch_name": "feature/x"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_create_missing_branch_name() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let req = DaemonRequest::new(40, "bundle.create", json!({"work_id": wi_id, "claims": "stuff"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("branch_name"));
    }

    #[test]
    fn test_bundle_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain plan+spec+phase+work create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feature/x"}),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "bundle");
    }

    #[test]
    fn test_bundle_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/auth"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/auth");
    }

    #[test]
    fn test_bundle_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_bundle_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        // Create a bundle (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/ts-read"}),
            ),
        );
        assert!(!create_resp.is_error());
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.bundles.write().unwrap().remove(&bundle_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(41, "bundle.get", json!({"id": bundle_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["branch_name"], "feature/ts-read");
    }

    #[test]
    fn test_bundle_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "bundle.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_bundle_list_filtered_by_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": _phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundles under different work items
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                41,
                "bundle.create",
                json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "bundle.list", json!(null)),
        );
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by wi_id_1 — should have 1
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1})),
        );
        let bundles = filtered_resp.result.unwrap();
        let arr = bundles.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id_1) = create_test_work(&stores, &tx, &wm);

        // Create a second work item under the same phase
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                31,
                "work.create",
                json!({"phase_id": _phase_id, "title": "WI 2", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        );
        let wi_id_2 = resp2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundles under different work items (writes to both TaskStore and HashMap)
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id_1, "branch_name": "feature/a"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                41,
                "bundle.create",
                json!({"work_id": wi_id_2, "branch_name": "feature/b"}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.bundles.write().unwrap().clear();

        // List all should still return both bundles via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "bundle.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list also works from TaskStore
        let filtered_req = DaemonRequest::new(51, "bundle.list", json!({"work_id": wi_id_1}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["branch_name"], "feature/a");
    }

    #[test]
    fn test_bundle_transition_proposed_to_triaged() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain plan+spec+phase+work create events
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();
        let _ = rx.try_recv();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let _ = rx.try_recv(); // consume bundle create event
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Triaged",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Triaged");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "bundle");
        assert_eq!(event.data["from"], "Proposed");
        assert_eq!(event.data["to"], "Triaged");
    }

    #[test]
    fn test_bundle_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Proposed → Accepted (invalid: must go through Triaged → Reviewed)
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Accepted",
                "role": "coordinator"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                40,
                "bundle.create",
                json!({"work_id": wi_id, "branch_name": "feature/x"}),
            ),
        );
        let bundle_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Implementer cannot transition Proposed → Triaged
        let req = DaemonRequest::new(
            41,
            "bundle.transition",
            json!({
                "id": bundle_id,
                "target_status": "Triaged",
                "role": "implementer"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_bundle_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "bundle.transition",
            json!({
                "id": "nonexistent",
                "target_status": "Triaged"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    // --- Staleness guard tests ---

    /// Helper: insert a Published Tick into the store and return its ID.
    fn insert_published_tick(stores: &Arc<Stores>, number: u32) -> String {
        use crate::domain::tick::Tick;
        let mut tick = Tick::new(number);
        tick.status = TickStatus::Published;
        tick.integration_sha = Some(format!("sha-{number}"));
        let id = tick.id.clone();
        stores.ticks.write().unwrap().insert(id.clone(), tick);
        id
    }

    #[test]
    fn test_bundle_create_staleness_guard_rejects_no_base_tick_when_published_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
        assert!(err.message.contains("staleness guard"));
    }

    #[test]
    fn test_bundle_create_staleness_guard_rejects_stale_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _old_tick_id = insert_published_tick(&stores, 1);
        let latest_tick_id = insert_published_tick(&stores, 2);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "old-stale-tick-id"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32002);
        assert!(err.message.contains("staleness guard"));
        assert!(err.message.contains(&latest_tick_id));
    }

    #[test]
    fn test_bundle_create_staleness_guard_accepts_matching_base_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": tick_id,
                "claims": "Add auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "Expected success but got: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["base_tick_id"], tick_id);
        assert_eq!(result["status"], "Proposed");
    }

    #[test]
    fn test_bundle_create_staleness_guard_uses_highest_tick_number() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        let _tick1_id = insert_published_tick(&stores, 1);
        let tick2_id = insert_published_tick(&stores, 2);

        // Using tick1's ID should be rejected (tick2 is latest)
        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": _tick1_id,
                "claims": "Add auth"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains(&tick2_id));
    }

    #[test]
    fn test_bundle_create_staleness_guard_broadcasts_stale_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // Drain create events
        while rx.try_recv().is_ok() {}

        let _tick_id = insert_published_tick(&stores, 1);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/auth",
                "base_tick_id": "stale-id"
            }),
        );
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "bundle.rejected_stale");
        assert_eq!(event.data["bundle_work_id"], wi_id.as_str());
        assert_eq!(event.data["base_tick_id"], "stale-id");
    }

    #[test]
    fn test_bundle_create_bootstrap_no_published_tick_no_base() {
        // Bootstrap case: no published tick, no base_tick_id → OK
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let req = DaemonRequest::new(
            40,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "branch_name": "feature/init"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
    }

    // --- Tick handler tests ---

    #[test]
    fn test_tick_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 7}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Tick> = store.get(&tick_id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().number, 7);
    }

    #[test]
    fn test_tick_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["number"], 1);
        assert_eq!(result["status"], "Open");
        assert!(result["integration_sha"].is_null());
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 0);
        assert_eq!(stores.ticks.read().unwrap().len(), 1);
    }

    #[test]
    fn test_tick_create_missing_number() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.create", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("number"));
    }

    #[test]
    fn test_tick_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        let req = DaemonRequest::new(50, "tick.create", json!({"number": 1}));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "tick");
    }

    #[test]
    fn test_tick_create_singleton_guard_blocks_second() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick (Open)
        let resp1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        assert!(!resp1.is_error());

        // Second create should fail — non-terminal Tick exists
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(51, "tick.create", json!({"number": 2})),
        );
        assert!(resp2.is_error());
        assert!(
            resp2
                .error
                .unwrap()
                .message
                .contains("non-terminal Tick already exists")
        );
    }

    #[test]
    fn test_tick_create_singleton_guard_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create and publish first tick
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                51,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );

        // Now creation should succeed
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(54, "tick.create", json!({"number": 2})),
        );
        assert!(!resp2.is_error());
    }

    #[test]
    fn test_tick_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 42})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let get_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "tick.get", json!({"id": tick_id})),
        );
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 42);
    }

    #[test]
    fn test_tick_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.get", json!({"id": "nonexistent"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_tick_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick (writes to both TaskStore and HashMap)
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 99})),
        );
        assert!(!create_resp.is_error());
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Remove from HashMap to prove get reads from TaskStore
        stores.ticks.write().unwrap().remove(&tick_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "tick.get", json!({"id": tick_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["number"], 99);
    }

    #[test]
    fn test_tick_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tick.list", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_tick_list_filtered_by_status() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick
        let create1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick1_id = create1.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 1 through to Published so we can create another
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": tick1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );

        // Now create second tick (singleton guard allows it since tick 1 is terminal)
        let create2 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
        );
        let tick2_id = create2.result.unwrap()["id"].as_str().unwrap().to_string();

        // Transition tick 2 to Sealing
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                56,
                "tick.transition",
                json!({"id": tick2_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );

        // List all — should have 2
        let all_resp = dispatch(&stores, &tx, &wm, &ic, DaemonRequest::new(60, "tick.list", json!(null)));
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // List filtered by Published — should have 1 (tick 1)
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(61, "tick.list", json!({"status": "Published"})),
        );
        let ticks = filtered_resp.result.unwrap();
        let arr = ticks.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["number"], 1);
    }

    #[test]
    fn test_tick_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create first tick, transition to Published, then create second
        let c1 = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let t1_id = c1.result.unwrap()["id"].as_str().unwrap().to_string();
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                52,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                53,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                54,
                "tick.transition",
                json!({"id": t1_id, "target_status": "Published", "role": "integrator"}),
            ),
        );
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(55, "tick.create", json!({"number": 2})),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.ticks.write().unwrap().clear();

        // List all should still return both ticks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(60, "tick.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by status also works from TaskStore
        // Tick 1 is Published, tick 2 is Open
        let filtered_req = DaemonRequest::new(61, "tick.list", json!({"status": "Open"}));
        let filtered_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), filtered_req);
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        let arr = filtered_items.as_array().unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[test]
    fn test_tick_transition_open_to_sealing() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let _ = rx.try_recv(); // consume create event
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "transition.completed");
        assert_eq!(event.data["collection"], "tick");
        assert_eq!(event.data["from"], "Open");
        assert_eq!(event.data["to"], "Sealing");
    }

    #[test]
    fn test_tick_transition_invalid_skip_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Open → Published (invalid: must go through Sealing → Validating)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_tick_transition_wrong_role() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Coordinator cannot transition tick (Integrator-only)
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "coordinator"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32000);
    }

    #[test]
    fn test_tick_transition_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tick.transition",
            json!({"id": "nonexistent", "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_tick_transition_default_role_is_integrator() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(50, "tick.create", json!({"number": 1})),
        );
        let tick_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Omit role — should default to Integrator and succeed
        let req = DaemonRequest::new(
            51,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Sealing");
    }

    // --- learning.create tests ---

    fn create_learning(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        id: u64,
    ) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                id,
                "learning.create",
                json!({
                    "source_id": "wi-123",
                    "scope": "work",
                    "content": "Always run tests"
                }),
            ),
        );
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_learning_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "learning.create",
            json!({
                "source_id": "wi-123",
                "scope": "work",
                "content": "Always run tests"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(retrieved.is_some());
        let learning = retrieved.unwrap();
        assert_eq!(learning.source_id, "wi-123");
        assert_eq!(learning.content, "Always run tests");
    }

    #[test]
    fn test_learning_create_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({
                    "source_id": "wi-123",
                    "scope": "work",
                    "content": "Always run tests before committing"
                }),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["source_id"], "wi-123");
        assert_eq!(result["scope"], "work");
        assert_eq!(result["content"], "Always run tests before committing");
        assert_eq!(result["reinforcements"], 0);
        assert!(!result["promoted"].as_bool().unwrap());
        assert_eq!(stores.learnings.read().unwrap().len(), 1);
    }

    #[test]
    fn test_learning_create_missing_source_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"scope": "global", "content": "insight"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("source_id"));
    }

    #[test]
    fn test_learning_create_missing_content() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.create", json!({"source_id": "wi-1", "scope": "global"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("content"));
    }

    #[test]
    fn test_learning_create_invalid_scope() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "invalid", "content": "test"}),
            ),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("scope"));
    }

    #[test]
    fn test_learning_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "learning");
    }

    // --- learning.get tests ---

    #[test]
    fn test_learning_get_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.get", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["source_id"], "wi-123");
    }

    #[test]
    fn test_learning_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.get", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32001);
    }

    #[test]
    fn test_learning_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a learning (writes to both TaskStore and HashMap)
        let learning_id = create_learning(&stores, &tx, &wm, 50);

        // Remove from HashMap to prove get reads from TaskStore
        stores.learnings.write().unwrap().remove(&learning_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "learning.get", json!({"id": learning_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["source_id"], "wi-123");
    }

    // --- learning.list tests ---

    #[test]
    fn test_learning_list_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.list", json!(null)),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_learning_list_with_scope_filter() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a work-scoped learning
        create_learning(&stores, &tx, &wm, 1);
        // Create a global-scoped learning
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.create",
                json!({"source_id": "global", "scope": "global", "content": "global insight"}),
            ),
        );

        // Filter by global scope
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.list", json!({"scope": "global"})),
        );
        assert!(!resp.is_error());
        let list = resp.result.unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["scope"], "global");
    }

    #[test]
    fn test_learning_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a work-scoped learning (writes to both TaskStore and HashMap)
        create_learning(&stores, &tx, &wm, 1);
        // Create a global-scoped learning
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.create",
                json!({"source_id": "global-src", "scope": "global", "content": "global insight"}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.learnings.write().unwrap().clear();

        // List all should still return both learnings via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "learning.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test filtered list by scope works from TaskStore
        let filtered_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "learning.list", json!({"scope": "global"})),
        );
        assert!(!filtered_resp.is_error());
        let filtered_items = filtered_resp.result.unwrap();
        assert_eq!(filtered_items.as_array().unwrap().len(), 1);
        assert_eq!(filtered_items[0]["scope"], "global");
    }

    // --- learning.reinforce tests ---

    #[test]
    fn test_learning_reinforce() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);

        // Reinforce again
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.reinforce", json!({"id": learning_id})),
        );
        assert_eq!(resp2.result.unwrap()["reinforcements"], 2);
    }

    #[test]
    fn test_learning_reinforce_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.reinforce", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    // --- learning.contradict tests ---

    #[test]
    fn test_learning_contradict() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["contradictions"], 1);
    }

    // --- learning.promote / demote tests ---

    #[test]
    fn test_learning_promote_and_demote() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        // Promote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap()["promoted"].as_bool().unwrap());

        // Demote
        let resp2 = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp2.is_error());
        assert!(!resp2.result.unwrap()["promoted"].as_bool().unwrap());
    }

    #[test]
    fn test_learning_promote_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let learning_id = create_learning(&stores, &tx, &wm, 1);
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "learning");
    }

    #[test]
    fn test_learning_reinforce_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().reinforcements, 1);
    }

    #[test]
    fn test_learning_contradict_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert_eq!(learning.unwrap().contradictions, 1);
    }

    #[test]
    fn test_learning_promote_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(learning.unwrap().promoted);
    }

    #[test]
    fn test_learning_demote_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        // Promote first so we can demote
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let learning: Option<Learning> = store.get(&learning_id).unwrap();
        assert!(learning.is_some());
        assert!(!learning.unwrap().promoted);
    }

    // --- Lock handler tests ---

    fn create_lock(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager, id: u64) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                id,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_lock_create_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Verify it was persisted to TaskStore
        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let retrieved: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(retrieved.is_some());
        let lock = retrieved.unwrap();
        assert_eq!(lock.resource, "src/main.rs");
        assert_eq!(lock.holder_id, "wi-1");
        assert_eq!(lock.granted_by, "coord-1");
    }

    #[test]
    fn test_lock_create() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["resource"], "src/main.rs");
        assert_eq!(result["holder_id"], "wi-1");
        assert_eq!(result["granted_by"], "coord-1");
        assert_eq!(result["status"], "active");
    }

    #[test]
    fn test_lock_create_missing_resource() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.create", json!({"holder_id": "wi-1", "granted_by": "coord-1"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_create_missing_holder_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "file.rs", "granted_by": "coord-1"}),
            ),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_get() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.get", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[test]
    fn test_lock_get_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "lock.get", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_get_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a lock (writes to both TaskStore and HashMap)
        let lock_id = create_lock(&stores, &tx, &wm, 50);

        // Remove from HashMap to prove get reads from TaskStore
        stores.locks.write().unwrap().remove(&lock_id);

        // Get should still succeed via TaskStore
        let get_req = DaemonRequest::new(51, "lock.get", json!({"id": lock_id}));
        let get_resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), get_req);
        assert!(!get_resp.is_error());
        assert_eq!(get_resp.result.unwrap()["resource"], "src/main.rs");
    }

    #[test]
    fn test_lock_list() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        create_lock(&stores, &tx, &wm, 1);
        create_lock(&stores, &tx, &wm, 2);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.list", json!({})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_lock_list_filter_active_only() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        create_lock(&stores, &tx, &wm, 2);

        // Release the first lock
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        );

        // List active only
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "lock.list", json!({"active_only": true})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_lock_list_reads_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two locks (writes to both TaskStore and HashMap)
        create_lock(&stores, &tx, &wm, 1);
        // Create a second lock with different resource
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        );

        // Clear HashMap to prove list reads from TaskStore
        stores.locks.write().unwrap().clear();

        // List all should still return both locks via TaskStore
        let all_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(10, "lock.list", json!(null)),
        );
        assert!(!all_resp.is_error());
        assert_eq!(all_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test active_only filter works from TaskStore (both are Active)
        let active_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(11, "lock.list", json!({"active_only": true})),
        );
        assert!(!active_resp.is_error());
        assert_eq!(active_resp.result.unwrap().as_array().unwrap().len(), 2);

        // Test resource filter works from TaskStore
        let resource_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(12, "lock.list", json!({"resource": "src/lib.rs"})),
        );
        assert!(!resource_resp.is_error());
        let resource_items = resource_resp.result.unwrap();
        assert_eq!(resource_items.as_array().unwrap().len(), 1);
        assert_eq!(resource_items[0]["resource"], "src/lib.rs");
    }

    #[test]
    fn test_lock_release() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "released");
    }

    #[test]
    fn test_lock_release_already_released() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        // Try releasing again
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.release", json!({"id": lock_id})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "expired");
    }

    #[test]
    fn test_lock_expire_already_expired() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.expire", json!({"id": lock_id})),
        );
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "lock.expire", json!({"id": lock_id})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_lock_release_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.release", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status.to_string(), "Released");
    }

    #[test]
    fn test_lock_expire_persists_to_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            50,
            "lock.create",
            json!({
                "resource": "src/main.rs",
                "holder_id": "wi-1",
                "granted_by": "coord-1"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        let lock_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(51, "lock.expire", json!({"id": lock_id})),
        );
        assert!(!resp.is_error());

        let store = stores.store.as_ref().unwrap().lock().unwrap();
        let lock: Option<Lock> = store.get(&lock_id).unwrap();
        assert!(lock.is_some());
        assert_eq!(lock.unwrap().status.to_string(), "Expired");
    }

    #[test]
    fn test_lock_create_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        create_lock(&stores, &tx, &wm, 1);
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.created");
        assert_eq!(event.data["collection"], "lock");
    }

    #[test]
    fn test_lock_release_broadcasts_event() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let mut rx = tx.subscribe();
        let lock_id = create_lock(&stores, &tx, &wm, 1);
        let _ = rx.try_recv(); // consume create event

        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "lock.release", json!({"id": lock_id})),
        );
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "record.updated");
        assert_eq!(event.data["collection"], "lock");
    }

    // --- Worktree handler tests ---

    #[test]
    fn test_worktree_create_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_worktree_create_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.create", json!({"work_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_create_validates_work_exists() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Create a full hierarchy so work exists
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);
        // This will fail at the git level (nonexistent repo path) but should
        // pass the work validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "worktree.create", json!({"work_id": wi_id})),
        );
        // The error should be from git, not from "not found"
        assert!(resp.is_error());
        let msg = &resp.error.as_ref().unwrap().message;
        assert!(
            !msg.contains("not found"),
            "error should be from git, not from validation: {}",
            msg
        );
    }

    #[test]
    fn test_worktree_list_returns_response() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // list on nonexistent repo will error, but it routes correctly
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.list", json!(null)),
        );
        // Will be an error since the repo doesn't exist, but the method routes
        assert!(resp.is_error());
    }

    #[test]
    fn test_worktree_cleanup_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_worktree_cleanup_work_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.cleanup", json!({"work_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("not found"));
    }

    #[test]
    fn test_worktree_refresh_missing_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("work_id"));
    }

    #[test]
    fn test_worktree_refresh_nonexistent_worktree() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "worktree.refresh", json!({"work_id": "nonexistent"})),
        );
        // Will error since worktree path doesn't exist
        assert!(resp.is_error());
    }

    #[test]
    fn test_worktree_dispatch_routes_all_methods() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Verify all 4 worktree methods are routed (not method_not_found)
        for method in &[
            "worktree.create",
            "worktree.list",
            "worktree.cleanup",
            "worktree.refresh",
        ] {
            let resp = dispatch(
                &stores,
                &tx,
                &wm,
                &test_integrator_config(),
                DaemonRequest::new(1, *method, json!({})),
            );
            // Even if they error, they should NOT be method_not_found (-32601)
            if resp.is_error() {
                assert_ne!(
                    resp.error.as_ref().unwrap().code,
                    -32601,
                    "method {} should be routed",
                    method
                );
            }
        }
    }

    // --- integrator.validate tests ---

    fn create_sealing_tick(stores: &Arc<Stores>, tx: &broadcast::Sender<DaemonEvent>, wm: &WorktreeManager) -> String {
        // Create a tick
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
        // Transition Open → Sealing
        let tr = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "tick.transition",
                json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
            ),
        );
        assert!(!tr.is_error(), "transition failed: {:?}", tr.error);
        tick_id
    }

    #[test]
    fn test_integrator_validate_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Use "echo ok" as validation command (always succeeds)
        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
        assert!(!result["validation_log"].as_str().unwrap().is_empty());
        // integration_sha should be set (we're in a git repo)
        assert!(result["integration_sha"].is_string());
    }

    #[test]
    fn test_integrator_validate_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Use a failing command
        let ic = IntegratorConfig {
            validation_commands: vec!["false".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        assert!(result["validation_log"].as_str().unwrap().contains("FAILED"));
        assert!(result["integration_sha"].is_null());
    }

    #[test]
    fn test_integrator_validate_wrong_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick in Open state (not Sealing)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "integrator.validate", json!({"tick_id": tick_id})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Sealing"));
    }

    #[test]
    fn test_integrator_validate_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({"tick_id": "nonexistent"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not found"));
    }

    #[test]
    fn test_integrator_validate_missing_tick_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("tick_id"));
    }

    #[test]
    fn test_integrator_validate_events() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);
        // Drain setup events
        while rx.try_recv().is_ok() {}

        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        // Should get transition.completed (Sealing→Validating), validation.started,
        // validation.completed, and tick.published events
        let event1 = rx.try_recv().unwrap();
        assert_eq!(event1.event, "transition.completed");
        let event2 = rx.try_recv().unwrap();
        assert_eq!(event2.event, "validation.started");
        let event3 = rx.try_recv().unwrap();
        assert_eq!(event3.event, "validation.completed");
        assert_eq!(event3.data["success"], true);
        let event4 = rx.try_recv().unwrap();
        assert_eq!(event4.event, "tick.published");
    }

    // --- integrator.publish tests ---

    #[test]
    fn test_integrator_publish_from_open() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a tick in Open state
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // publish chains Open → Sealing → Validating → Published
        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
    }

    #[test]
    fn test_integrator_publish_from_sealing() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        let ic = IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Published");
    }

    #[test]
    fn test_integrator_publish_wrong_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        // Manually transition to Validating to create wrong state
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "tick.transition",
                json!({"id": tick_id, "target_status": "Validating", "role": "integrator"}),
            ),
        );

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Open or Sealing"));
    }

    #[test]
    fn test_integrator_publish_validation_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        );
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let ic = IntegratorConfig {
            validation_commands: vec!["false".to_string()],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "integrator.publish", json!({"tick_id": tick_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Failed");
    }

    #[test]
    fn test_integrator_dispatch_routes() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        for method in &["integrator.validate", "integrator.publish"] {
            let resp = dispatch(
                &stores,
                &tx,
                &wm,
                &test_integrator_config(),
                DaemonRequest::new(1, *method, json!({})),
            );
            if resp.is_error() {
                assert_ne!(
                    resp.error.as_ref().unwrap().code,
                    -32601,
                    "method {} should be routed",
                    method
                );
            }
        }
    }

    #[test]
    fn test_integrator_validate_multi_command_stops_on_first_failure() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm);

        let ic = IntegratorConfig {
            validation_commands: vec![
                "echo first".to_string(),
                "false".to_string(),
                "echo should-not-run".to_string(),
            ],
            ..Default::default()
        };
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(3, "integrator.validate", json!({"tick_id": tick_id})),
        );
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        let log = result["validation_log"].as_str().unwrap();
        assert!(log.contains("first"));
        assert!(log.contains("FAILED"));
        assert!(!log.contains("should-not-run"));
    }

    // --- system.init tests ---

    #[test]
    fn test_dispatch_system_init() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "system.init failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let collections = result["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 10);
        assert!(collections.contains(&json!("plans")));
        assert!(collections.contains(&json!("specs")));
        assert!(collections.contains(&json!("phases")));
        assert!(collections.contains(&json!("works")));
        assert!(collections.contains(&json!("bundles")));
        assert!(collections.contains(&json!("ticks")));
        assert!(collections.contains(&json!("learnings")));
        assert!(collections.contains(&json!("locks")));
        assert!(collections.contains(&json!("coordinator_goals")));
        assert!(collections.contains(&json!("agent_sessions")));
        // git_hooks_installed is best-effort — may be false in test environments
        // due to taskstore's configure_merge_driver not using current_dir
        assert!(result.get("git_hooks_installed").is_some());
    }

    #[test]
    fn test_dispatch_system_init_without_store() {
        let stores = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert!(err.message.contains("TaskStore not initialized"));
    }

    #[test]
    fn test_dispatch_system_init_idempotent() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Call init twice — should succeed both times
        let req1 = DaemonRequest::new(1, "system.init", json!({}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());
        let req2 = DaemonRequest::new(2, "system.init", json!({}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp2.is_error());
    }

    // --- Validation gate tests (Phase 4) ---

    #[test]
    fn test_plan_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Try Draft → Active without any validation report — should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.as_ref().unwrap().code, -32003);
        assert!(resp.error.unwrap().message.contains("validator.validate"));
    }

    #[test]
    fn test_plan_transition_allowed_with_pass_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a passing validation report into TaskStore
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "All criteria met".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft → Active should succeed
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_allowed_with_warn_report() {
        let (_dir, stores) =
            test_stores_with_validator_strictness(crate::config::ValidatorStrictness::AllowAmbiguityWithFlags);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a Warn validation report
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Warn,
            vec![],
            "Passes with warnings".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft → Active should succeed (Warn allows transition)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_blocked_with_fail_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a Fail validation report
        let report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Fail,
            vec![],
            "Missing criteria".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft → Active should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_plan_transition_skip_validation_override() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active with skip_validation=true — should succeed even without report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_plan_transition_no_gate_when_validator_disabled() {
        // test_stores_with_taskstore has no validator → gate should not apply
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "No Gate Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active should succeed without any validation report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_spec_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create spec
        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Gate Test Spec"})),
        );
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active without report — blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "spec.transition",
                json!({
                    "id": spec_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_spec_transition_allowed_with_pass_report() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Gate Test Spec"})),
        );
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert passing report
        let report = ValidationReport::new(
            "specs".to_string(),
            spec_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "ok".to_string(),
            "test-model".to_string(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Draft → Active should succeed
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "spec.transition",
                json!({
                    "id": spec_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_phase_transition_blocked_no_report_when_validator_enabled() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create parent plan → spec → phase
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let phase_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "phase.create",
                json!({
                    "spec_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        );
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active without report — blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                4,
                "phase.transition",
                json!({
                    "id": phase_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(resp.is_error());
        assert_eq!(resp.error.unwrap().code, -32003);
    }

    #[test]
    fn test_phase_transition_skip_validation_override() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Parent Plan"})),
        );
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "spec.create", json!({"plan_id": plan_id, "title": "Parent Spec"})),
        );
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let phase_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "phase.create",
                json!({
                    "spec_id": spec_id, "title": "Gate Test Phase", "order": 1
                }),
            ),
        );
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Draft → Active with skip_validation — should succeed
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                4,
                "phase.transition",
                json!({
                    "id": phase_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    #[test]
    fn test_non_draft_to_active_transition_no_gate() {
        // Active → Complete should NOT trigger the validation gate
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // First, skip validation to get to Active
        dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator",
                    "skip_validation": true
                }),
            ),
        );

        // Active → Complete should work without any validation report
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "complete",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "complete");
    }

    #[test]
    fn test_latest_report_wins_for_validation_gate() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let create_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Gate Test Plan"})),
        );
        let plan_id = create_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Insert a Fail report first (older)
        let fail_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Fail,
            vec![],
            "Failed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(fail_report)
            .unwrap();

        // Sleep briefly to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Insert a Pass report (newer — should win)
        let pass_report = ValidationReport::new(
            "plans".to_string(),
            plan_id.clone(),
            crate::domain::validation::ValidationVerdict::Pass,
            vec![],
            "Passed".to_string(),
            "test-model".to_string(),
        );
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(pass_report)
            .unwrap();

        // Draft → Active should succeed (latest report is Pass)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.transition",
                json!({
                    "id": plan_id,
                    "target_status": "active",
                    "role": "coordinator"
                }),
            ),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "active");
    }

    // --- Coordinator goal tests ---

    #[test]
    fn test_coordinator_set_goal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Build auth" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "set_goal failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["goal"], "Build auth");
        assert_eq!(result["active"], true);
        assert!(!result["id"].as_str().unwrap().is_empty());
        // Verify in stores
        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 1);
        let goal = goals.values().next().unwrap();
        assert_eq!(goal.goal, "Build auth");
        assert!(goal.active);
    }

    #[test]
    fn test_coordinator_set_goal_replaces_previous() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Set first goal
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "First goal" }));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        assert!(!resp1.is_error());
        let first_id = resp1.result.unwrap()["id"].as_str().unwrap().to_string();

        // Set second goal — deactivates first
        let req2 = DaemonRequest::new(2, "coordinator.set_goal", json!({ "goal": "Second goal" }));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp2.is_error());

        let goals = stores.coordinator_goals.read().unwrap();
        assert_eq!(goals.len(), 2);
        // First goal deactivated
        assert!(!goals[&first_id].active);
        // Second goal active
        let active_count = goals.values().filter(|g| g.active).count();
        assert_eq!(active_count, 1);
    }

    #[test]
    fn test_coordinator_set_goal_empty_rejected() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_coordinator_set_goal_missing_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.set_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_coordinator_clear_goal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Set a goal first
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Test goal" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);

        // Clear it
        let req2 = DaemonRequest::new(2, "coordinator.clear_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["cleared"], 1);

        // All goals deactivated
        let goals = stores.coordinator_goals.read().unwrap();
        assert!(goals.values().all(|g| !g.active));
    }

    #[test]
    fn test_coordinator_clear_goal_when_none() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.clear_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["cleared"], 0);
    }

    #[test]
    fn test_coordinator_get_goal_returns_active() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Set a goal first
        let req1 = DaemonRequest::new(1, "coordinator.set_goal", json!({ "goal": "Build auth" }));
        dispatch(&stores, &tx, &wm, &test_integrator_config(), req1);
        // Get it
        let req2 = DaemonRequest::new(2, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2);
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["goal"], "Build auth");
        assert_eq!(result["active"], true);
    }

    #[test]
    fn test_coordinator_get_goal_when_none() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "coordinator.get_goal", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["active"], false);
    }

    // --- Pool size enforcement tests ---

    #[test]
    fn test_max_pool_for_helper() {
        let config = crate::config::Config::default();
        assert_eq!(max_pool_for(AgentType::Implementer, &config), 6);
        assert_eq!(max_pool_for(AgentType::Reviewer, &config), 2);
        assert_eq!(max_pool_for(AgentType::Coordinator, &config), 1);
        assert_eq!(max_pool_for(AgentType::Researcher, &config), 4);
        assert_eq!(max_pool_for(AgentType::Integrator, &config), 1);
    }

    #[test]
    fn test_agent_start_max_pool_enforcement() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Add a non-terminal Coordinator session to the store (simulating already running)
        let session = crate::agents::AgentSession::new(AgentType::Coordinator, "model".to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Attempt to start another Coordinator — should be rejected (max_pool = 1)
        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error(), "expected max_pool rejection");
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32004);
        assert!(err.message.contains("max_pool exceeded"));
    }

    #[tokio::test]
    async fn test_agent_start_pool_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Add a terminal (Completed) Coordinator session — should NOT count
        let mut session = crate::agents::AgentSession::new(AgentType::Coordinator, "model".to_string());
        session.status = crate::agents::AgentStatus::Completed;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Should be allowed — no active Coordinator sessions
        let req = DaemonRequest::new(1, "agent.start", json!({ "agent_type": "coordinator" }));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        // This will succeed at session creation but the spawned task may fail (no runtime).
        // We just check it wasn't rejected by max_pool.
        assert!(!resp.is_error(), "expected success, got: {:?}", resp.error);
    }

    // === Coverage tests: learning CRUD ===

    #[test]
    fn test_handle_learning_update() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let learning_id = create_learning(&stores, &tx, &wm, 1);

        // Update content, applicable_roles, and resource_tags
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "learning.update",
                json!({
                    "id": learning_id,
                    "content": "Updated content",
                    "applicable_roles": ["Implementer", "Reviewer"],
                    "resource_tags": ["src/main.rs", "src/lib.rs"]
                }),
            ),
        );
        assert!(!resp.is_error(), "learning.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["content"], "Updated content");
        assert_eq!(result["resource_tags"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_handle_learning_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"id": "nonexistent", "content": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_learning_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "learning.update", json!({"content": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_learning_reinforce() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create learning via dispatch (persists to TaskStore)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Reinforce it — exercises the TaskStore persist path
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.reinforce", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["reinforcements"], 1);
    }

    #[test]
    fn test_handle_learning_contradict() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.contradict", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["contradictions"], 1);
    }

    #[test]
    fn test_handle_learning_promote() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], true);
    }

    #[test]
    fn test_handle_learning_demote() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "learning.create",
                json!({"source_id": "wi-1", "scope": "global", "content": "test"}),
            ),
        );
        assert!(!resp.is_error());
        let learning_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Promote first
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "learning.promote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());

        // Then demote
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "learning.demote", json!({"id": learning_id})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["promoted"], false);
    }

    // === Coverage tests: lock creation ===

    #[test]
    fn test_lock_create_with_ttl_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/main.rs", "holder_id": "wi-1", "granted_by": "coord-1", "ttl_secs": 300}),
            ),
        );
        assert!(!resp.is_error(), "lock.create with ttl failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert!(result["expires_at"].is_number(), "should have expires_at from ttl_secs");
    }

    #[test]
    fn test_lock_create_auto_expire() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Default max_lock_ttl_minutes is 60, so auto-expire should set expires_at
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/lib.rs", "holder_id": "wi-2", "granted_by": "coord-1"}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        // Without explicit ttl_secs, auto-expire from max_lock_ttl_minutes should set expires_at
        assert!(result["expires_at"].is_number(), "should have auto-expire expires_at");
    }

    #[test]
    fn test_lock_create_renewable_flag() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "lock.create",
                json!({"resource": "src/mod.rs", "holder_id": "wi-3", "granted_by": "coord-1", "renewable": true}),
            ),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["renewable"], true);
    }

    // === Coverage tests: agent lifecycle ===

    #[test]
    fn test_handle_agent_pause() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a running agent session
        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": sid})),
        );
        assert!(!resp.is_error(), "agent.pause failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "paused");
    }

    #[test]
    fn test_handle_agent_pause_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_agent_pause_terminal_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let _ = session.transition_to(crate::agents::AgentStatus::Completed);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.pause", json!({"session_id": sid})),
        );
        assert!(resp.is_error(), "should reject pause on terminal agent");
    }

    #[test]
    fn test_handle_agent_resume() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a paused agent session
        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "model".to_string());
        let _ = session.transition_to(crate::agents::AgentStatus::Running);
        let _ = session.transition_to(crate::agents::AgentStatus::Paused);
        let sid = session.id.clone();
        stores.agent_sessions.write().unwrap().insert(sid.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.resume", json!({"session_id": sid})),
        );
        assert!(!resp.is_error(), "agent.resume failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["status"], "running");
    }

    #[test]
    fn test_handle_agent_resume_missing_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.resume", json!({"session_id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_agent_output() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // No events for session — should return empty array
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({"session_id": "sess-1"})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);

        // Add some events and query with since=0
        {
            let event = crate::agents::AgentEvent::LlmOutput {
                session_id: "sess-1".to_string(),
                chunk: "hello world".to_string(),
                is_final: false,
            };
            let mut events = stores.agent_events.write().unwrap();
            events.entry("sess-1".to_string()).or_default().push_back(event);
        }

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "agent.output", json!({"session_id": "sess-1", "since": 0})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_handle_agent_output_missing_session_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.output", json!({})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: validator handlers ===

    #[test]
    fn test_handle_validator_validate() {
        // validator.validate requires prompts::init() and an LLM API key.
        // We test the parameter validation paths instead.
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Missing collection — exercises param validation
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "plan-1"})),
        );
        assert!(resp.is_error());

        // Missing id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        );
        assert!(resp.is_error());

        // Plan not found — exercises the plan lookup path
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "validator.validate",
                json!({"collection": "plans", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Unsupported collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "validator.validate", json!({"collection": "widgets", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported"));
    }

    #[test]
    fn test_handle_validator_validate_no_validator() {
        let stores = test_stores(); // no validator
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "plans", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not enabled"));
    }

    #[test]
    fn test_handle_validator_validate_missing_params() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Missing collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "x"})),
        );
        assert!(resp.is_error());

        // Missing id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_validate_unknown_collection() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "unknown", "id": "x"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported collection"));
    }

    #[test]
    fn test_handle_validator_validate_not_found() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Plan not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "validator.validate",
                json!({"collection": "plans", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Spec not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "validator.validate",
                json!({"collection": "specs", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());

        // Phase not found
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                3,
                "validator.validate",
                json!({"collection": "phases", "id": "nonexistent"}),
            ),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_report() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a validation report directly in TaskStore
        let report = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Pass,
            vec![],
            "All good".into(),
            "test-model".into(),
        );
        let report_id = report.id.clone();
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": report_id})),
        );
        assert!(!resp.is_error(), "validator.report failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["verdict"], "pass");
    }

    #[test]
    fn test_handle_validator_report_not_found() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "nonexistent"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_validator_report_no_taskstore() {
        let stores = test_stores(); // no TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "any"})),
        );
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("TaskStore"));
    }

    #[test]
    fn test_handle_validator_reports() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create two reports for different targets
        let report1 = ValidationReport::new(
            "plans".into(),
            "plan-1".into(),
            ValidationVerdict::Pass,
            vec![],
            "ok".into(),
            "test-model".into(),
        );
        let report2 = ValidationReport::new(
            "plans".into(),
            "plan-2".into(),
            ValidationVerdict::Fail,
            vec![],
            "bad".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report1).unwrap();
        stores.store.as_ref().unwrap().lock().unwrap().create(report2).unwrap();

        // List all reports
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);

        // Filter by target_id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.reports", json!({"target_id": "plan-1"})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);

        // Filter by collection
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "validator.reports", json!({"target_collection": "plans"})),
        );
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);
    }

    #[test]
    fn test_handle_validator_reports_no_taskstore() {
        let stores = test_stores(); // no TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        );
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    // === Coverage tests: tool.list ===

    #[test]
    fn test_handle_tool_list() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tool.list", json!({})),
        );
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["tools"].is_array());
    }

    // === Coverage tests: validation gate strictness ===

    #[test]
    fn test_validation_gate_hard_fail_on_warn() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Warn report
        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Warn,
            vec![],
            "Ambiguous criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Try to transition Draft → Active — should be blocked
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "HardFailOnAnyAmbiguity should block Draft→Active on Warn report"
        );
    }

    #[test]
    fn test_validation_gate_suggest_only_on_fail() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::SuggestOnly);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan
        let plan = Plan::new("Gate Test".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Fail report — SuggestOnly should NOT block
        let report = ValidationReport::new(
            "plans".into(),
            plan_id.clone(),
            ValidationVerdict::Fail,
            vec![],
            "Failed criteria".into(),
            "test-model".into(),
        );
        stores.store.as_ref().unwrap().lock().unwrap().create(report).unwrap();

        // Try to transition Draft → Active — should succeed under SuggestOnly
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            !resp.is_error(),
            "SuggestOnly should NOT block Draft→Active even on Fail report: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_validation_gate_no_report_enabled() {
        use crate::config::ValidatorStrictness;

        let (_dir, stores) = test_stores_with_validator_strictness(ValidatorStrictness::HardFailOnAnyAmbiguity);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan with NO validation reports
        let plan = Plan::new("No Reports".into(), "desc".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Try to transition Draft → Active — should be blocked (no report exists)
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "plan.transition",
                json!({"id": plan_id, "target_status": "active", "role": "coordinator"}),
            ),
        );
        assert!(
            resp.is_error(),
            "should block Draft→Active when no validation report exists"
        );
    }

    #[test]
    fn test_agent_session_model_from_config() {
        use crate::config::Config;
        // Verify the config-based model lookup matches what each agent type should get
        let config = Config::default();
        let cases: Vec<(AgentType, String)> = vec![
            (AgentType::Coordinator, config.agents.coordinator.role.model.clone()),
            (AgentType::Implementer, config.agents.implementer.model.clone()),
            (AgentType::Reviewer, config.agents.reviewer.model.clone()),
            (AgentType::Researcher, config.agents.researcher.model.clone()),
            (AgentType::Integrator, "deterministic".to_string()),
        ];
        for (agent_type, expected_model) in cases {
            let model = match agent_type {
                AgentType::Coordinator => config.agents.coordinator.role.model.clone(),
                AgentType::Implementer => config.agents.implementer.model.clone(),
                AgentType::Reviewer => config.agents.reviewer.model.clone(),
                AgentType::Researcher => config.agents.researcher.model.clone(),
                AgentType::Integrator => "deterministic".to_string(),
                AgentType::Chat => config.agents.implementer.model.clone(),
            };
            assert_eq!(model, expected_model, "model mismatch for {:?}", agent_type);
        }
        // Coordinator should specifically be Opus
        assert_eq!(config.agents.coordinator.role.model, "claude-opus-4-6");
    }

    // --- Implementer dedup tests ---

    #[test]
    fn test_implementer_dedup_rejects_second_on_same_work() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a non-terminal Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Try to start another implementer for the same work_id
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        assert!(resp.is_error(), "should reject duplicate implementer");
        let err_msg = resp.error.unwrap().message;
        assert!(
            err_msg.contains("non-terminal Implementer session already exists"),
            "unexpected error: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_implementer_dedup_allows_after_terminal() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a terminal (Completed) Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        session.transition_to(AgentStatus::Completed).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Start a new implementer for the same work_id — should pass dedup (but may fail on spawn)
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-1"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        // Should NOT be a precondition_failed error (it may error for other reasons like no LLM key)
        if resp.is_error() {
            let err_msg = &resp.error.as_ref().unwrap().message;
            assert!(
                !err_msg.contains("non-terminal Implementer session already exists"),
                "should not reject after terminal session: {}",
                err_msg
            );
        }
    }

    #[tokio::test]
    async fn test_implementer_dedup_allows_different_work_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Insert a non-terminal Implementer session for work_id "wi-1"
        let mut session = AgentSession::new(AgentType::Implementer, "test-model".into());
        session.work_id = Some("wi-1".to_string());
        session.transition_to(AgentStatus::Running).unwrap();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Start an implementer for a DIFFERENT work_id — should pass dedup
        let req = DaemonRequest::new(
            1,
            "agent.start",
            json!({"agent_type": "implementer", "work_id": "wi-2"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);

        // Should NOT be a precondition_failed error
        if resp.is_error() {
            let err_msg = &resp.error.as_ref().unwrap().message;
            assert!(
                !err_msg.contains("non-terminal Implementer session already exists"),
                "should not reject different work_id: {}",
                err_msg
            );
        }
    }

    // === Coverage tests: plan.update ===

    #[test]
    fn test_handle_plan_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let plan_id = create_test_plan(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "plan.update",
                json!({
                    "id": plan_id,
                    "title": "Updated Plan",
                    "description": "New desc",
                    "acceptance_criteria": "New criteria"
                }),
            ),
        );
        assert!(!resp.is_error(), "plan.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Plan");
        assert_eq!(result["description"], "New desc");
        assert_eq!(result["acceptance_criteria"], "New criteria");
    }

    #[test]
    fn test_handle_plan_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_plan_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: spec.update ===

    #[test]
    fn test_handle_spec_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, spec_id) = create_test_spec(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "spec.update",
                json!({
                    "id": spec_id,
                    "title": "Updated Spec",
                    "description": "New desc"
                }),
            ),
        );
        assert!(!resp.is_error(), "spec.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Spec");
    }

    #[test]
    fn test_handle_spec_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_spec_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "spec.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: phase.update ===

    #[test]
    fn test_handle_phase_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_plan_id, _spec_id, phase_id) = create_test_phase(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "phase.update",
                json!({
                    "id": phase_id,
                    "title": "Updated Phase",
                    "description": "New desc",
                    "order": 5
                }),
            ),
        );
        assert!(!resp.is_error(), "phase.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Phase");
        assert_eq!(result["order"], 5);
    }

    #[test]
    fn test_handle_phase_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_phase_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "phase.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: work.update ===

    #[test]
    fn test_handle_work_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_phase_id, wi_id) = create_test_work(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "work.update",
                json!({
                    "id": wi_id,
                    "title": "Updated Work",
                    "description": "New desc",
                    "assignee": "agent-1",
                    "resource_tags": ["src/lib.rs"],
                    "acceptance_criteria": ["tests pass"],
                    "dependencies": ["dep-1"]
                }),
            ),
        );
        assert!(!resp.is_error(), "work.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["title"], "Updated Work");
        assert_eq!(result["description"], "New desc");
        assert_eq!(result["assignee"], "agent-1");
        assert_eq!(result["resource_tags"].as_array().unwrap().len(), 1);
        assert_eq!(result["acceptance_criteria"].as_array().unwrap().len(), 1);
        assert_eq!(result["dependencies"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_handle_work_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.update", json!({"id": "nonexistent", "title": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_work_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.update", json!({"title": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: bundle.update ===

    #[test]
    fn test_handle_bundle_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({
                    "id": bundle_id,
                    "description": "Updated desc",
                    "verification": "tests pass",
                    "locks_used": ["lock-1"],
                    "base_tick_id": "tick-002"
                }),
            ),
        );
        assert!(!resp.is_error(), "bundle.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["description"], "Updated desc");
        assert_eq!(result["verification"], "tests pass");
        assert_eq!(result["locks_used"].as_array().unwrap().len(), 1);
        assert_eq!(result["base_tick_id"], "tick-002");
    }

    #[test]
    fn test_handle_bundle_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "bundle.update", json!({"id": "nonexistent", "description": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_bundle_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "bundle.update", json!({"description": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_bundle_update_size_policy_rejects_too_many_files() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        // Default max_files_touched is 8, so 9 paths should be rejected
        let too_many_paths: Vec<String> = (0..9).map(|i| format!("file_{}.rs", i)).collect();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({
                    "id": bundle_id,
                    "touched_paths": too_many_paths
                }),
            ),
        );
        assert!(resp.is_error(), "expected size policy rejection but got success");
    }

    #[test]
    fn test_handle_bundle_update_claims_string_backward_compat() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({"id": bundle_id, "claims": "single claim string"}),
            ),
        );
        assert!(!resp.is_error(), "bundle.update claims string failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let claims = result["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0], "single claim string");
    }

    #[test]
    fn test_handle_bundle_update_claims_array() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_wi_id, bundle_id) = create_test_bundle(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "bundle.update",
                json!({"id": bundle_id, "claims": ["claim 1", "claim 2"]}),
            ),
        );
        assert!(!resp.is_error(), "bundle.update claims array failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let claims = result["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0], "claim 1");
        assert_eq!(claims[1], "claim 2");
    }

    // === Coverage tests: tick.update ===

    #[test]
    fn test_handle_tick_update_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_test_tick(&stores, &tx, &wm);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                2,
                "tick.update",
                json!({
                    "id": tick_id,
                    "validation_log": "All tests passed",
                    "bundle_ids": ["b-1", "b-2"],
                    "attempted_bundle_ids": ["b-1", "b-2", "b-3"]
                }),
            ),
        );
        assert!(!resp.is_error(), "tick.update failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["validation_log"], "All tests passed");
        assert_eq!(result["bundle_ids"].as_array().unwrap().len(), 2);
        assert_eq!(result["attempted_bundle_ids"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_handle_tick_update_not_found() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"id": "nonexistent", "validation_log": "x"})),
        );
        assert!(resp.is_error());
    }

    #[test]
    fn test_handle_tick_update_missing_id() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.update", json!({"validation_log": "x"})),
        );
        assert!(resp.is_error());
    }

    // === Coverage tests: agent.status paths ===

    #[test]
    fn test_agent_status_from_taskstore() {
        let (_dir, stores) = test_stores_with_taskstore();
        // Rebuild AgentSession indexes for TaskStore
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .rebuild_indexes::<AgentSession>()
            .unwrap();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create an agent session and insert it directly into TaskStore (NOT the HashMap)
        let session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores.store.as_ref().unwrap().lock().unwrap().create(session).unwrap();

        // Query via agent.status — should find it in TaskStore
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.status", json!({"session_id": session_id})),
        );
        assert!(!resp.is_error(), "agent.status failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }

    #[test]
    fn test_agent_status_fallback_to_hashmap() {
        let stores = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "agent.status", json!({"session_id": session_id})),
        );
        assert!(!resp.is_error(), "agent.status fallback failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["id"], session_id);
    }

    // --- coordinator.accept_plan tests ---

    #[test]
    fn test_accept_plan_with_plan_id() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Create a plan first
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "plan.create", json!({"title": "Test Plan", "description": "desc"})),
        );
        assert!(!resp.is_error());
        let plan_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        // Accept with plan_id
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "coordinator.accept_plan", json!({"plan_id": plan_id})),
        );
        assert!(!resp.is_error(), "accept_plan failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], true);
        assert_eq!(result["plan_id"], plan_id);
        // New: response includes goal_id
        assert!(result["goal_id"].as_str().is_some());

        // Verify plan is now Active
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans[&plan_id].status, HierarchyStatus::Active);

        // Verify CoordinatorGoal was created
        let goals = stores.read_coordinator_goals().unwrap();
        assert!(goals.values().any(|g| g.active));
    }

    #[test]
    fn test_accept_plan_with_text() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "coordinator.accept_plan",
                json!({"plan": "My Plan Title\nGoal: Build auth"}),
            ),
        );
        assert!(!resp.is_error(), "accept_plan with text failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["accepted"], true);
        let plan_id = result["plan_id"].as_str().unwrap();
        let goal_id = result["goal_id"].as_str().unwrap();

        // Verify plan was created and activated
        let plans = stores.read_plans().unwrap();
        let plan = &plans[plan_id];
        assert_eq!(plan.title, "My Plan Title");
        assert_eq!(plan.description, "My Plan Title\nGoal: Build auth");
        assert_eq!(plan.status, HierarchyStatus::Active);

        // Verify CoordinatorGoal was created with plan title
        let goals = stores.read_coordinator_goals().unwrap();
        let goal = &goals[goal_id];
        assert!(goal.active);
        assert_eq!(goal.goal, "My Plan Title");

        // Verify CoordinatorState was pre-created with plan_approved=true
        let states = stores.read_coordinator_states().unwrap();
        let state = states
            .values()
            .find(|s| s.goal_id == goal_id)
            .expect("CoordinatorState should exist for the new goal");
        assert!(state.plan_approved, "plan_approved must be true after accept_plan");
        assert_eq!(
            state.fsm_state,
            crate::domain::coordinator_state::CoordinatorFsmState::Planning,
            "FSM should start in Planning after accept_plan"
        );
    }

    #[test]
    fn test_accept_plan_neither_param() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "coordinator.accept_plan", json!({})),
        );
        assert!(resp.is_error());
        assert!(
            resp.error
                .as_ref()
                .unwrap()
                .message
                .contains("plan_id or plan text is required")
        );
    }

    #[test]
    fn test_accept_plan_empty_text() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "coordinator.accept_plan", json!({"plan": "   "})),
        );
        assert!(resp.is_error());
        assert!(resp.error.as_ref().unwrap().message.contains("plan text is empty"));
    }

    #[test]
    fn test_accept_plan_title_extraction() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Title from first non-empty line
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(
                1,
                "coordinator.accept_plan",
                json!({"plan": "\n\n  Auth Module  \nDetails here"}),
            ),
        );
        assert!(!resp.is_error());
        let plan_id = resp.result.as_ref().unwrap()["plan_id"].as_str().unwrap();
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans[plan_id].title, "Auth Module");
    }

    // ── Phase 3: Cycle Detection tests ──────────────────────────────────

    /// Helper: create a Work in the store and return its ID.
    fn create_work(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        phase_id: &str,
        title: &str,
        deps: &[&str],
    ) -> String {
        let mut params = json!({
            "phase_id": phase_id,
            "title": title,
            "description": "test",
            "resource_tags": ["src/"],
            "acceptance_criteria": ["pass"],
        });
        if !deps.is_empty() {
            params["dependencies"] = json!(deps);
        }
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "work.create", params),
        );
        assert!(!resp.is_error(), "work.create failed: {:?}", resp.error);
        resp.result.unwrap()["id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_detect_cycle_self_reference() {
        let mut works = HashMap::new();
        let w = Work::new("ph-1".into(), "A".into(), "desc".into());
        let id = w.id.clone();
        works.insert(id.clone(), w);
        // A depends on itself
        assert!(detect_dependency_cycle(&works, &id, std::slice::from_ref(&id)));
    }

    #[test]
    fn test_detect_cycle_direct() {
        let mut works = HashMap::new();
        let mut a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        // A depends on B
        a.dependencies = vec![b_id.clone()];
        works.insert(a_id.clone(), a);
        works.insert(b_id.clone(), b);
        // Creating C with deps [A] is fine
        assert!(!detect_dependency_cycle(&works, "wk-new", std::slice::from_ref(&a_id)));
        // But if B tries to depend on A, it's a cycle
        assert!(detect_dependency_cycle(&works, &b_id, std::slice::from_ref(&a_id)));
    }

    #[test]
    fn test_detect_cycle_transitive() {
        let mut works = HashMap::new();
        let mut a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let c = Work::new("ph-1".into(), "C".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();
        // A -> B -> C
        a.dependencies = vec![b_id.clone()];
        b.dependencies = vec![c_id.clone()];
        works.insert(a_id.clone(), a);
        works.insert(b_id.clone(), b);
        works.insert(c_id.clone(), c);
        // C trying to depend on A creates transitive cycle
        assert!(detect_dependency_cycle(&works, &c_id, std::slice::from_ref(&a_id)));
    }

    #[test]
    fn test_detect_cycle_valid_chain() {
        let mut works = HashMap::new();
        let a = Work::new("ph-1".into(), "A".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        b.dependencies = vec![a_id.clone()];
        works.insert(a_id, a);
        works.insert(b_id.clone(), b);
        // C depends on B (linear chain A <- B <- C) - no cycle
        assert!(!detect_dependency_cycle(&works, "wk-c", &[b_id]));
    }

    #[test]
    fn test_detect_cycle_diamond_accepted() {
        let mut works = HashMap::new();
        let d = Work::new("ph-1".into(), "D".into(), "desc".into());
        let mut b = Work::new("ph-1".into(), "B".into(), "desc".into());
        let mut c = Work::new("ph-1".into(), "C".into(), "desc".into());
        let d_id = d.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();
        // B -> D, C -> D (diamond: A -> B -> D, A -> C -> D)
        b.dependencies = vec![d_id.clone()];
        c.dependencies = vec![d_id.clone()];
        works.insert(d_id, d);
        works.insert(b_id.clone(), b);
        works.insert(c_id.clone(), c);
        // A depends on both B and C - diamond, no cycle
        assert!(!detect_dependency_cycle(&works, "wk-a", &[b_id, c_id]));
    }

    #[test]
    fn test_self_referencing_dependency_rejected_via_handler() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        // Create A first (no deps)
        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);

        // Try to update A to depend on itself
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "work.update", json!({"id": a_id, "dependencies": [a_id]})),
        );
        assert!(resp.is_error(), "expected error for self-referencing dependency");
        assert!(
            resp.error.as_ref().unwrap().message.contains("Circular dependency"),
            "expected cycle error, got: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_direct_cycle_rejected_at_update() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);
        let b_id = create_work(&stores, &tx, &wm, &phase_id, "B", &[&a_id]);

        // Update A to depend on B - creates cycle A -> B -> A
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "work.update", json!({"id": a_id, "dependencies": [b_id]})),
        );
        assert!(resp.is_error(), "expected error for direct cycle");
        assert!(
            resp.error.as_ref().unwrap().message.contains("Circular dependency"),
            "expected cycle error, got: {:?}",
            resp.error
        );
    }

    #[test]
    fn test_valid_chain_accepted_via_handler() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let (_, _, phase_id) = create_test_phase(&stores, &tx, &wm);

        let a_id = create_work(&stores, &tx, &wm, &phase_id, "A", &[]);
        let b_id = create_work(&stores, &tx, &wm, &phase_id, "B", &[&a_id]);
        let c_id = create_work(&stores, &tx, &wm, &phase_id, "C", &[&b_id]);

        // Verify all were created successfully (linear chain)
        assert!(!a_id.is_empty());
        assert!(!b_id.is_empty());
        assert!(!c_id.is_empty());
    }
}
