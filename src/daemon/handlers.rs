use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::trace;

use crate::agents::AgentKind;
use crate::config::IntegratorConfig;
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::method::DaemonMethod;
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
    let raw = match agent_type {
        AgentKind::Implementer => config.agents.implementer.max_pool,
        AgentKind::Reviewer => config.agents.reviewer.max_pool,
        AgentKind::Director => 1, // Only one Director at a time
        AgentKind::Researcher => config.agents.researcher.max_pool,
        AgentKind::Chat => 1,
        AgentKind::Decomposer => config.agents.researcher.max_pool,
    };
    if raw == crate::config::MAX_POOL_UNLIMITED {
        config.agents.worker_pool_size.resolve()
    } else {
        raw
    }
}

mod agent;
mod bundle;
mod chat;
mod common;
mod decomposer;
mod director;
mod doc;
pub mod integrator;
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
use director::*;
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
    fsm: &FsmInterpreter,
    req: DaemonRequest,
) -> DaemonResponse {
    trace!("dispatch(method={})", req.method);
    // v4 cutover: auto_start_agents hook removed. Engine strategies now handle:
    // - Implementer spawning: spawn-implementer-for-ready-work strategy
    // - Bundle triage: auto-triage-proposed-bundle strategy
    // - Reviewer spawning: spawn-reviewer-for-triaged-bundle strategy

    // Parse the wire-string method into our typed method vocabulary. Unknown strings
    // short-circuit to a JSON-RPC method-not-found error. Once parsed, the match below
    // is exhaustive - adding a variant to `DaemonMethod` without handling it here fails
    // compilation.
    let method: DaemonMethod = match req.method.parse() {
        Ok(m) => m,
        Err(_) => return DaemonResponse::err(req.id, RpcError::method_not_found(&req.method)),
    };

    match method {
        // system.*
        DaemonMethod::SystemHandshake => handle_handshake(stores, req),
        DaemonMethod::SystemInit => handle_system_init(stores, req),
        DaemonMethod::SystemStatus => handle_status(stores, req),
        DaemonMethod::SystemShutdown => handle_shutdown(event_tx, req),
        DaemonMethod::SystemRecover => handle_recover(stores, req),

        // plan.*
        DaemonMethod::PlanCreate => handle_plan_create(stores, event_tx, req),
        DaemonMethod::PlanGet => handle_plan_get(stores, req),
        DaemonMethod::PlanList => handle_plan_list(stores, req),
        DaemonMethod::PlanTransition => handle_plan_transition(stores, event_tx, fsm, req),
        DaemonMethod::PlanUpdate => handle_plan_update(stores, event_tx, req),

        // spec.*
        DaemonMethod::SpecCreate => handle_spec_create(stores, event_tx, req),
        DaemonMethod::SpecGet => handle_spec_get(stores, req),
        DaemonMethod::SpecList => handle_spec_list(stores, req),
        DaemonMethod::SpecTransition => handle_spec_transition(stores, event_tx, fsm, req),
        DaemonMethod::SpecUpdate => handle_spec_update(stores, event_tx, req),

        // phase.*
        DaemonMethod::PhaseCreate => handle_phase_create(stores, event_tx, req),
        DaemonMethod::PhaseGet => handle_phase_get(stores, req),
        DaemonMethod::PhaseList => handle_phase_list(stores, req),
        DaemonMethod::PhaseTransition => handle_phase_transition(stores, event_tx, fsm, req),
        DaemonMethod::PhaseUpdate => handle_phase_update(stores, event_tx, req),

        // work.*
        DaemonMethod::WorkCreate => handle_work_create(stores, event_tx, req),
        DaemonMethod::WorkGet => handle_work_get(stores, req),
        DaemonMethod::WorkList => handle_work_list(stores, req),
        DaemonMethod::WorkTransition => handle_work_transition(stores, event_tx, fsm, req),
        DaemonMethod::WorkUpdate => handle_work_update(stores, event_tx, req),

        // bundle.*
        DaemonMethod::BundleCreate => handle_bundle_create(stores, event_tx, req),
        DaemonMethod::BundleGet => handle_bundle_get(stores, req),
        DaemonMethod::BundleList => handle_bundle_list(stores, req),
        DaemonMethod::BundleTransition => handle_bundle_transition(stores, event_tx, fsm, req),
        DaemonMethod::BundleUpdate => handle_bundle_update(stores, event_tx, req),

        // tick.*
        DaemonMethod::TickCreate => handle_tick_create(stores, event_tx, req),
        DaemonMethod::TickGet => handle_tick_get(stores, req),
        DaemonMethod::TickList => handle_tick_list(stores, req),
        DaemonMethod::TickTransition => handle_tick_transition(stores, event_tx, fsm, req),
        DaemonMethod::TickUpdate => handle_tick_update(stores, event_tx, req),

        // learning.*
        DaemonMethod::LearningCreate => handle_learning_create(stores, event_tx, req),
        DaemonMethod::LearningGet => handle_learning_get(stores, req),
        DaemonMethod::LearningList => handle_learning_list(stores, req),
        DaemonMethod::LearningUpdate => handle_learning_update(stores, event_tx, req),
        DaemonMethod::LearningReinforce => handle_learning_reinforce(stores, event_tx, req),
        DaemonMethod::LearningContradict => handle_learning_contradict(stores, event_tx, req),
        DaemonMethod::LearningPromote => handle_learning_promote(stores, event_tx, req),
        DaemonMethod::LearningDemote => handle_learning_demote(stores, event_tx, req),

        // lock.*
        DaemonMethod::LockCreate => handle_lock_create(stores, event_tx, req),
        DaemonMethod::LockGet => handle_lock_get(stores, req),
        DaemonMethod::LockList => handle_lock_list(stores, req),
        DaemonMethod::LockRelease => handle_lock_release(stores, event_tx, req),
        DaemonMethod::LockExpire => handle_lock_expire(stores, event_tx, req),

        // worktree.*
        DaemonMethod::WorktreeCreate => handle_worktree_create(stores, event_tx, worktree_mgr, req),
        DaemonMethod::WorktreeList => handle_worktree_list(worktree_mgr, req),
        DaemonMethod::WorktreeCleanup => handle_worktree_cleanup(stores, event_tx, worktree_mgr, req),
        DaemonMethod::WorktreeRefresh => handle_worktree_refresh(worktree_mgr, req),

        // integrator.*
        DaemonMethod::IntegratorValidate => handle_integrator_validate(stores, event_tx, integrator_config, req),
        DaemonMethod::IntegratorPublish => handle_integrator_publish(stores, event_tx, integrator_config, req),

        // validator.*
        DaemonMethod::ValidatorValidate => handle_validator_validate(stores, req).await,
        DaemonMethod::ValidatorReport => handle_validator_report(stores, req),
        DaemonMethod::ValidatorReports => handle_validator_reports(stores, req),

        // coverage.*
        DaemonMethod::CoverageEvaluate => handle_coverage_evaluate(stores, req).await,

        // tool.* / tools.*
        DaemonMethod::ToolList => handle_tool_list(stores, req),
        DaemonMethod::ToolsRegister => handle_tools_register(stores, event_tx, req),

        // doc.*
        DaemonMethod::DocAccept => handle_doc_accept(stores, event_tx, worktree_mgr, integrator_config, fsm, req).await,
        DaemonMethod::DocInject => handle_doc_inject(stores, event_tx, worktree_mgr, integrator_config, fsm, req).await,

        // chat.*
        DaemonMethod::ChatSubmit => handle_chat_submit(stores, event_tx, req),
        DaemonMethod::ChatAttach => handle_chat_attach(stores, req),
        DaemonMethod::ChatHistory => handle_chat_history(stores, req),

        // agent.*
        DaemonMethod::AgentStart => handle_agent_start(stores, event_tx, worktree_mgr, req),
        DaemonMethod::AgentStop => handle_agent_stop(stores, event_tx, req),
        DaemonMethod::AgentPause => handle_agent_pause(stores, event_tx, req),
        DaemonMethod::AgentResume => handle_agent_resume(stores, event_tx, req),
        DaemonMethod::AgentStatus => handle_agent_status(stores, req),
        DaemonMethod::AgentList => handle_agent_list(stores, req),
        DaemonMethod::AgentOutput => handle_agent_output(stores, req),

        // director.*
        DaemonMethod::DirectorStartPlanIntake => handle_director_start_plan_intake(stores, event_tx, worktree_mgr, req),
        DaemonMethod::DirectorUserMessage => handle_director_user_message(stores, event_tx, req),

        // decomposer.*
        DaemonMethod::DecomposerDecompose => decomposer::handle_decomposer_decompose(stores, event_tx, req).await,
        DaemonMethod::DecomposerRatify => decomposer::handle_decomposer_ratify(stores, req).await,
        DaemonMethod::DecomposerAbandonChildren => {
            decomposer::handle_decomposer_abandon_children(stores, event_tx, req)
        }
        DaemonMethod::DecomposerReDecompose => decomposer::handle_decomposer_re_decompose(stores, event_tx, req).await,
        DaemonMethod::DecomposerHandleFailure => decomposer::handle_decomposer_failure(stores, req),
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

    pub(crate) fn test_stores() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test");
        let mut stores = Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        (dir, Arc::new(stores))
    }

    /// Like `test_stores` but initializes a real git repo with a HEAD commit.
    /// Only use this when the test actually requires `git rev-parse HEAD` to succeed
    /// (e.g. asserting that `integration_sha` is populated). All other tests should
    /// use `test_stores()` to avoid paying the ~0.5s per-test git subprocess cost.
    pub(crate) fn test_stores_with_git() -> (TestDir, Arc<Stores>) {
        let dir = TestDir::new("loopr-handler-test-git");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.email=test@test.com",
                "-c",
                "user.name=Test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .output()
            .expect("git initial commit failed");
        let mut stores = Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        (dir, Arc::new(stores))
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
            llm: crate::config::LlmConfig {
                api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
                ..crate::config::LlmConfig::default()
            },
            enabled: true,
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
            llm: crate::config::LlmConfig {
                api_key_env: "NONEXISTENT_TEST_KEY".to_string(),
                ..crate::config::LlmConfig::default()
            },
            enabled: true,
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

    pub(crate) fn test_fsm() -> FsmInterpreter {
        FsmInterpreter::embedded().unwrap()
    }

    /// Convenience wrapper around `dispatch()` that auto-provides an embedded FsmInterpreter.
    /// Avoids changing 400+ test call sites that call dispatch with the old 5-param signature.
    pub(crate) async fn test_dispatch(
        stores: &Arc<Stores>,
        event_tx: &broadcast::Sender<DaemonEvent>,
        worktree_mgr: &WorktreeManager,
        integrator_config: &IntegratorConfig,
        req: DaemonRequest,
    ) -> DaemonResponse {
        let fsm = test_fsm();
        dispatch(stores, event_tx, worktree_mgr, integrator_config, &fsm, req).await
    }

    // --- dispatch tests ---

    #[tokio::test]
    async fn test_dispatch_unknown_method() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "unknown.method", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unknown.method"));
    }

    #[tokio::test]
    async fn test_dispatch_handshake() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.handshake", json!({"client_version": "0.1.0"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["protocol"], "ndjson/1");
    }

    #[tokio::test]
    async fn test_dispatch_status_empty() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
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
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Insert a plan
        let plan = Plan::new("Test".into(), "".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);
        // Insert a work item
        let wi = Work::new("p-1".into(), "WI".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
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
        let plan = Plan::new("TS Plan".into(), "".into());
        stores
            .store
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .create(plan.clone())
            .unwrap();

        // Create a spec via TaskStore directly
        let spec = Spec::new(plan.id.clone(), "TS Spec".into());
        stores.store.as_ref().unwrap().lock().unwrap().create(spec).unwrap();

        let req = DaemonRequest::new(1, "system.status", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
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
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.shutdown", json!(null));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
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
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error(), "system.init failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        let collections = result["collections"].as_array().unwrap();
        assert_eq!(collections.len(), 10);
        assert!(collections.contains(&json!("plans")));
        assert!(collections.contains(&json!("specs")));
        assert!(collections.contains(&json!("phases")));
        assert!(collections.contains(&json!("works")));
        assert!(collections.contains(&json!("docs")));
        assert!(collections.contains(&json!("bundles")));
        assert!(collections.contains(&json!("ticks")));
        assert!(collections.contains(&json!("learnings")));
        assert!(collections.contains(&json!("locks")));
        assert!(collections.contains(&json!("agent_sessions")));
        // git_hooks_installed is best-effort - may be false in test environments
        // due to taskstore's configure_merge_driver not using current_dir
        assert!(result.get("git_hooks_installed").is_some());
    }

    #[tokio::test]
    async fn test_dispatch_system_init_without_store() {
        let (_dir, stores) = test_stores(); // No TaskStore
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "system.init", json!({}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
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
        let resp1 = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req1).await;
        assert!(!resp1.is_error());
        let req2 = DaemonRequest::new(2, "system.init", json!({}));
        let resp2 = dispatch(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req2).await;
        assert!(!resp2.is_error());
    }

    // --- max_pool_for test ---

    #[test]
    fn test_max_pool_for_helper() {
        let config = crate::config::Config::default();
        // Implementer default is MAX_POOL_UNLIMITED, resolved to worker_pool_size (>= 1).
        assert!(max_pool_for(AgentKind::Implementer, &config) >= 1);
        // Reviewer default is now MAX_POOL_UNLIMITED, resolved to worker_pool_size (>= 1).
        assert!(max_pool_for(AgentKind::Reviewer, &config) >= 1);
        assert_eq!(max_pool_for(AgentKind::Director, &config), 1);
        assert_eq!(max_pool_for(AgentKind::Researcher, &config), 4);
    }

    // --- Fix 7: Auto-triage tests ---

    /// Helper to build stores with auto_start_reviewer=true and a full hierarchy.
    async fn setup_stores_with_bundle_parent() -> (TestDir, Arc<Stores>, String) {
        let (dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();
        let fsm = test_fsm();

        // Plan
        let plan_resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            &fsm,
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
            &fsm,
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
            &fsm,
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
            &fsm,
            DaemonRequest::new(
                4,
                "work.create",
                json!({"parent_id": phase_id, "title": "Test Work", "files": ["src/"], "acceptance_criteria": ["tests pass"]}),
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
    async fn test_bundle_create_stays_proposed_engine_handles_triage() {
        // v4 cutover: auto_start_agents hook removed. Bundles stay Proposed after
        // creation. The engine's auto-triage-proposed-bundle strategy handles triage.
        let (dir, stores, work_id) = setup_stores_with_bundle_parent().await;
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let ic = test_integrator_config();
        let fsm = test_fsm();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &ic,
            &fsm,
            DaemonRequest::new(
                10,
                "bundle.create",
                json!({"work_id": work_id, "branch_name": "agent/test", "claims": ["test claim"]}),
            ),
        )
        .await;
        assert!(!resp.is_error(), "bundle.create should succeed: {:?}", resp.error);
        let bundle_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        // Bundle stays Proposed - engine strategy will triage it on next tick
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).expect("bundle should exist");
        assert_eq!(
            bundle.status(),
            crate::domain::bundle::BundleStatus::Proposed,
            "bundle should stay Proposed after creation (engine handles triage)"
        );
    }

    #[tokio::test]
    async fn test_bundle_create_proposed_with_full_hierarchy() {
        // Bundles start in Proposed state; engine strategies handle triage.
        let (dir, stores_with_ts) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let ic = test_integrator_config();
        let fsm = test_fsm();

        // Create hierarchy
        let plan_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            &fsm,
            DaemonRequest::new(1, "plan.create", json!({"title": "Plan"})),
        )
        .await;
        let plan_id = plan_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        let spec_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            &fsm,
            DaemonRequest::new(2, "spec.create", json!({"parent_id": plan_id, "title": "Spec"})),
        )
        .await;
        let spec_id = spec_resp.result.unwrap()["id"].as_str().unwrap().to_string();
        let phase_resp = dispatch(
            &stores_with_ts,
            &tx,
            &wm,
            &ic,
            &fsm,
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
            &fsm,
            DaemonRequest::new(
                4,
                "work.create",
                json!({"parent_id": phase_id, "title": "Work", "files": ["src/"], "acceptance_criteria": ["tests pass"]}),
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
            &fsm,
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
