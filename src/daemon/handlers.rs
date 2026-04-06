use std::sync::Arc;

use log::trace;
use serde_json::json;
use tokio::sync::broadcast;

use crate::agents::AgentKind;
use crate::config::IntegratorConfig;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::worktree::manager::WorktreeManager;

use super::context::Stores;

/// Parse a string-valued param from the request using `FromStr`.
/// Returns the parsed value or an error DaemonResponse with a descriptive message.
pub(super) fn parse_required_param<T: std::str::FromStr<Err = String>>(
    req: &DaemonRequest,
    param: &str,
) -> Result<T, DaemonResponse> {
    match req.params.get(param).and_then(|v| v.as_str()) {
        Some(s) => s
            .parse::<T>()
            .map_err(|e| DaemonResponse::err(req.id, RpcError::invalid_params(&e))),
        None => Err(DaemonResponse::err(
            req.id,
            RpcError::invalid_params(&format!("{} is required", param)),
        )),
    }
}

/// Parse an optional string-valued param with a default value.
pub(super) fn parse_optional_param<T: std::str::FromStr<Err = String>>(
    req: &DaemonRequest,
    param: &str,
    default: T,
) -> Result<T, DaemonResponse> {
    match req.params.get(param) {
        Some(v) => match v.as_str() {
            Some(s) => s
                .parse::<T>()
                .map_err(|e| DaemonResponse::err(req.id, RpcError::invalid_params(&e))),
            None => Ok(default),
        },
        None => Ok(default),
    }
}

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

/// Async variant of try_handler! for handlers that contain .await calls.
macro_rules! try_async_handler {
    ($req_id:expr, $body:expr) => {{
        let __result: eyre::Result<DaemonResponse> = async { $body }.await;
        match __result {
            Ok(resp) => resp,
            Err(e) => DaemonResponse::err($req_id, RpcError::internal(&e.to_string())),
        }
    }};
}

/// Returns the configured max_pool for a given agent type.
fn max_pool_for(agent_type: AgentKind, config: &crate::config::Config) -> u32 {
    match agent_type {
        AgentKind::Implementer => config.agents.implementer.max_pool,
        AgentKind::Reviewer => config.agents.reviewer.max_pool,
        AgentKind::Coordinator => config.agents.coordinator.role.max_pool,
        AgentKind::Researcher => config.agents.researcher.max_pool,
        AgentKind::Integrator => 1,
        AgentKind::Chat => 1, // Single chat session for now
    }
}

mod agent;
mod bundle;
mod chat;
mod common;
mod coordinator;
mod doc;
mod integrator;
mod learning;
mod lock;
mod phase;
mod plan;
mod spec;
mod system;
mod tick;
mod tools;
mod work;
mod worktree;

use agent::*;
use bundle::*;
use chat::*;
use coordinator::*;
use doc::*;
use integrator::*;
use learning::*;
use lock::*;
use phase::*;
use plan::*;
use spec::*;
use system::*;
use tick::*;
use tools::*;
use work::*;
use worktree::*;

/// Dispatch an IPC request to the appropriate handler.
/// This is the central routing function for all daemon request handling.
pub async fn dispatch(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    trace!("dispatch(method={})", req.method);
    let method = req.method.clone();
    let params = req.params.clone();
    let resp = match req.method.as_str() {
        "system.handshake" => handle_handshake(stores, req),
        "system.init" => handle_system_init(stores, req),
        "system.status" => handle_status(stores, req),
        "system.shutdown" => handle_shutdown(event_tx, req),
        "system.recover" => handle_recover(stores, req),
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
        "validator.validate" => handle_validator_validate(stores, req).await,
        "validator.report" => handle_validator_report(stores, req),
        "validator.reports" => handle_validator_reports(stores, req),
        "coverage.evaluate" => handle_coverage_evaluate(stores, req).await,
        "tool.list" => handle_tool_list(stores, req),
        "tools.register" => handle_tools_register(stores, event_tx, req),
        "coordinator.get_goal" => handle_coordinator_get_goal(stores, req),
        "coordinator.get_state" => handle_coordinator_get_state(stores, req),
        "coordinator.reset_state" => handle_coordinator_reset_state(stores, event_tx, req),
        "coordinator.interview_respond" => handle_coordinator_interview_respond(stores, event_tx, req),
        "doc.accept" => handle_doc_accept(stores, event_tx, worktree_mgr, integrator_config, req).await,
        "doc.inject" => handle_doc_inject(stores, event_tx, worktree_mgr, integrator_config, req).await,
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
        auto_start_agents(
            stores,
            event_tx,
            worktree_mgr,
            integrator_config,
            &method,
            &params,
            &resp,
        )
        .await;
    }

    resp
}

/// Auto-start agents based on transition outcomes (Gap #29).
async fn auto_start_agents(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    method: &str,
    params: &serde_json::Value,
    resp: &crate::ipc::protocol::DaemonResponse,
) {
    if method == "work.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target.parse::<crate::domain::work::WorkStatus>().ok() == Some(crate::domain::work::WorkStatus::InProgress)
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
        let _ = Box::pin(dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req)).await;
    }

    // Fix 7: Auto-triage: when a bundle is created (starts in Proposed), immediately
    // dispatch Proposed -> Triaged deterministically without LLM involvement.
    // The existing Triaged hook below will then auto-spawn the reviewer.
    if method == "bundle.create"
        && stores.config.agents.auto_start_reviewer
        && let Some(bid) = resp.result.as_ref().and_then(|r| r.get("id")).and_then(|v| v.as_str())
        && bid.starts_with("bd-")
    {
        let triage_req = DaemonRequest::new(
            0,
            "bundle.transition",
            json!({
                "id": bid,
                "target_status": "Triaged",
                "role": "coordinator",
            }),
        );
        let _ = Box::pin(dispatch(stores, event_tx, worktree_mgr, integrator_config, triage_req)).await;
    }

    if method == "bundle.transition"
        && let Some(target) = params.get("target_status").and_then(|v| v.as_str())
        && target.parse::<crate::domain::bundle::BundleStatus>().ok()
            == Some(crate::domain::bundle::BundleStatus::Triaged)
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
        let _ = Box::pin(dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req)).await;
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use super::*;

    use crate::agents::AgentKind;
    use crate::config::IntegratorConfig;
    use crate::domain::bundle::Bundle;
    use crate::domain::doc::Doc;
    use crate::domain::learning::Learning;
    use crate::domain::lock::Lock;
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::tick::Tick;
    use crate::domain::validation::ValidationReport;
    use crate::domain::work::Work;
    use crate::test_util::TestDir;

    pub(crate) fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    pub(crate) fn test_stores_with_taskstore() -> (TestDir, Arc<Stores>) {
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
        store.rebuild_indexes::<Doc>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        (dir, Arc::new(stores))
    }

    /// Creates stores with TaskStore AND a validator (DocValidator placeholder via Arc).
    /// This activates the validation gate for Draft -> Active transitions.
    pub(crate) fn test_stores_with_validator_strictness(
        strictness: crate::config::ValidatorStrictness,
    ) -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        let mut store = taskstore::Store::open(&dir).unwrap();
        store.rebuild_indexes::<Plan>().unwrap();
        store.rebuild_indexes::<ValidationReport>().unwrap();
        store.rebuild_indexes::<Doc>().unwrap();
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

    pub(crate) fn test_stores_with_validator() -> (TestDir, Arc<Stores>) {
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
        store.rebuild_indexes::<Doc>().unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        // Create a DocValidator to enable the validation gate.
        // Tests don't call the LLM - they only check that the gate logic works.
        let validator_config = crate::config::ValidatorConfig {
            enabled: true,
            api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
            ..crate::config::ValidatorConfig::default()
        };
        stores.validator = Some(Arc::new(crate::validator::DocValidator::new(validator_config)));
        (dir, Arc::new(stores))
    }

    pub(crate) fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(16);
        tx
    }

    pub(crate) fn test_worktree_mgr() -> WorktreeManager {
        WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        )
    }

    pub(crate) fn test_integrator_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        }
    }

    // --- dispatch tests ---

    #[tokio::test]
    async fn test_dispatch_unknown_method() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "unknown.method", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unknown.method"));
    }

    #[tokio::test]
    async fn test_dispatch_handshake() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.handshake", json!({"client_version": "0.1.0"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["protocol"], "ndjson/1");
    }

    #[tokio::test]
    async fn test_dispatch_status_empty() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["version"].is_string());
        assert!(result["pid"].is_number());
        assert_eq!(result["counts"]["plans"], 0);
        assert_eq!(result["counts"]["works"], 0);
        // Without TaskStore, reports disabled
        assert_eq!(result["taskstore"]["enabled"], false);
    }

    #[tokio::test]
    async fn test_dispatch_status_with_records() {
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["counts"]["plans"], 1);
        assert_eq!(result["counts"]["works"], 1);
        assert_eq!(result["counts"]["specs"], 0);
    }

    #[tokio::test]
    async fn test_dispatch_status_with_taskstore() {
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
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

    #[tokio::test]
    async fn test_dispatch_shutdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.shutdown", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "shutting_down");
        // Verify event was broadcast
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "system.shutdown");
    }

    // --- system.init tests ---

    #[tokio::test]
    async fn test_dispatch_system_init() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(!resp.is_error(), "system.init failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let collections = result["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 11);
        assert!(collections.contains(&json!("plans")));
        assert!(collections.contains(&json!("specs")));
        assert!(collections.contains(&json!("phases")));
        assert!(collections.contains(&json!("works")));
        assert!(collections.contains(&json!("docs")));
        assert!(collections.contains(&json!("bundles")));
        assert!(collections.contains(&json!("ticks")));
        assert!(collections.contains(&json!("learnings")));
        assert!(collections.contains(&json!("locks")));
        assert!(collections.contains(&json!("coordinator_goals")));
        assert!(collections.contains(&json!("agent_sessions")));
        // git_hooks_installed is best-effort - may be false in test environments
        // due to taskstore's configure_merge_driver not using current_dir
        assert!(result.get("git_hooks_installed").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_system_init_without_store() {
        let stores = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        let err = resp.error.unwrap();
        assert!(err.message.contains("TaskStore not initialized"));
    }

    #[tokio::test]
    async fn test_dispatch_system_init_idempotent() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Call init twice - should succeed both times
        let req1 = DaemonRequest::new(1, "system.init", json!({}));
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req1).await;
        assert!(!resp1.is_error());
        let req2 = DaemonRequest::new(2, "system.init", json!({}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), req2).await;
        assert!(!resp2.is_error());
    }

    // --- max_pool_for test ---

    #[test]
    fn test_max_pool_for_helper() {
        let config = crate::config::Config::default();
        assert_eq!(max_pool_for(AgentKind::Implementer, &config), 6);
        assert_eq!(max_pool_for(AgentKind::Reviewer, &config), 2);
        assert_eq!(max_pool_for(AgentKind::Coordinator, &config), 1);
        assert_eq!(max_pool_for(AgentKind::Researcher, &config), 4);
        assert_eq!(max_pool_for(AgentKind::Integrator, &config), 1);
    }

    // --- Fix 7: Auto-triage tests ---

    /// Helper to build stores with auto_start_reviewer=true and a full hierarchy.
    async fn setup_stores_with_bundle_parent() -> (TestDir, Arc<Stores>, String) {
        let (dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Plan
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(1, "plan.create", json!({"title": "Test Plan"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Spec
        let spec_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Test Spec"})),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Phase
        let phase_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                3,
                "phase.create",
                json!({"parent_id": spec_id, "title": "Test Phase", "number": 1}),
            ),
        )
        .await;
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Work (with AC so it auto-promotes to Ready)
        let work_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                4,
                "work.create",
                json!({"parent_id": phase_id, "title": "Test Work", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        )
        .await;
        let work_id = work_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Enable auto_start_reviewer in stores config
        let mut config = stores.config.clone();
        config.agents.auto_start_reviewer = true;
        let mut new_stores = Stores::new();
        new_stores.config = config;
        new_stores.store = stores.store.clone();
        // Copy data from old stores
        *new_stores.plans.write().unwrap() = stores.plans.read().unwrap().clone();
        *new_stores.specs.write().unwrap() = stores.specs.read().unwrap().clone();
        *new_stores.phases.write().unwrap() = stores.phases.read().unwrap().clone();
        *new_stores.works.write().unwrap() = stores.works.read().unwrap().clone();
        let new_stores = Arc::new(new_stores);

        (dir, new_stores, work_id)
    }

    #[tokio::test]
    async fn test_bundle_create_auto_triages_when_auto_start_reviewer() {
        let (dir, stores, work_id) = setup_stores_with_bundle_parent().await;
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let ic = test_integrator_config();

        // Create a bundle - auto-triage should fire immediately
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                10,
                "bundle.create",
                json!({"work_id": work_id, "branch_name": "agent/test", "claims": ["test claim"]}),
            ),
        )
        .await;
        assert!(!resp.is_error(), "bundle.create should succeed: {:?}", resp.error);
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Bundle should now be Triaged (auto-triage fired in auto_start_agents)
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).expect("bundle should exist");
        assert_eq!(
            bundle.status(),
            crate::domain::bundle::BundleStatus::Triaged,
            "bundle should be auto-triaged to Triaged after creation"
        );
    }

    #[tokio::test]
    async fn test_bundle_create_no_auto_triage_when_disabled() {
        // auto_start_reviewer defaults to false, so no auto-triage should happen
        let (dir, stores_with_ts) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let ic = test_integrator_config();

        // Create hierarchy
        let plan_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        let spec_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"})),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        let phase_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                3,
                "phase.create",
                json!({"parent_id": spec_id, "title": "Phase", "number": 1}),
            ),
        )
        .await;
        let phase_id = phase_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        let work_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                4,
                "work.create",
                json!({"parent_id": phase_id, "title": "Work", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
            ),
        )
        .await;
        let work_id = work_resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Create bundle with auto_start_reviewer=false (default)
        let resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            DaemonRequest::new(
                10,
                "bundle.create",
                json!({"work_id": work_id, "branch_name": "agent/test", "claims": ["claim"]}),
            ),
        )
        .await;
        assert!(!resp.is_error());
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Bundle should remain Proposed (no auto-triage)
        let bundles = stores_with_ts.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).expect("bundle should exist");
        assert_eq!(
            bundle.status(),
            crate::domain::bundle::BundleStatus::Proposed,
            "bundle should remain Proposed when auto_start_reviewer is false"
        );
    }
}
