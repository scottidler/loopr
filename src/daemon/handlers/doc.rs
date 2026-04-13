//! IPC handlers for Doc-based plan entry paths.
//!
//! Two entry paths both funnel into a shared pipeline:
//!
//! - `doc.accept` (chat funnel): caller supplies plan markdown as a string.
//! - `doc.inject` (E2E test path): caller supplies a path to an existing plan .md file.
//!
//! Both paths:
//!   1. Optionally run the Decomposer (skip via `skip_decompose=true` for tests)
//!   2. Create a CoordinatorGoal and CoordinatorState
//!   3. Start the Coordinator agent
//!
//! The manifest entry path (`coordinator.seed_manifest`) is in coordinator.rs and
//! skips the Decomposer, injecting a pre-decomposed hierarchy directly.

use std::sync::Arc;

use eyre::eyre;
use tracing::{info, instrument, warn};

use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{IntegratorConfig, InterviewMode, ValidatorConfig};
use crate::daemon::context::Stores;
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};
use crate::domain::criteria::AcceptanceCriteria;
use crate::domain::markdown::write_doc_markdown_body;
use crate::domain::plan::{Plan, PlanStatus};
use crate::fsm::runtime::FsmInterpreter;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::validator::client::LlmClient;
use crate::worktree::manager::WorktreeManager;

/// Handle `doc.accept`: accept plan markdown as a string and run the full entry pipeline.
///
/// Required params:
/// - `markdown` (string): the plan document content
///
/// Optional params:
/// - `skip_decompose` (bool, default false): skip the Decomposer (useful in tests)
#[instrument(skip_all, fields(skip_decompose = ?req.params.get("skip_decompose"), markdown_len = req.params.get("markdown").and_then(|v| v.as_str()).map(|s| s.len())))]
pub(super) async fn handle_doc_accept(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    fsm: &FsmInterpreter,
    req: DaemonRequest,
) -> DaemonResponse {
    try_async_handler!(req.id, {
        let markdown = match req.params.get("markdown").and_then(|v| v.as_str()) {
            Some(m) if !m.trim().is_empty() => m.to_string(),
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("markdown is required and must be non-empty"),
                ));
            }
        };

        let skip_decompose = req
            .params
            .get("skip_decompose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        accept_plan_markdown(
            stores,
            event_tx,
            worktree_mgr,
            integrator_config,
            fsm,
            req.id,
            markdown,
            skip_decompose,
        )
        .await
    })
}

/// Handle `doc.inject`: read plan markdown from a file path and run the full entry pipeline.
///
/// Required params:
/// - `path` (string): absolute path to a plan .md file on disk
///
/// Optional params:
/// - `skip_decompose` (bool, default false): skip the Decomposer (useful in tests)
#[instrument(skip_all, fields(path = ?req.params.get("path")))]
pub(super) async fn handle_doc_inject(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    fsm: &FsmInterpreter,
    req: DaemonRequest,
) -> DaemonResponse {
    try_async_handler!(req.id, {
        let path_str = match req.params.get("path").and_then(|v| v.as_str()) {
            Some(p) => p.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("path to plan .md file is required"),
                ));
            }
        };

        let plan_path = std::path::PathBuf::from(&path_str);
        if !plan_path.exists() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params(&format!("plan file not found: {}", path_str)),
            ));
        }

        let markdown =
            std::fs::read_to_string(&plan_path).map_err(|e| eyre!("Failed to read plan file {}: {}", path_str, e))?;

        if markdown.trim().is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("plan file is empty"),
            ));
        }

        let skip_decompose = req
            .params
            .get("skip_decompose")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        accept_plan_markdown(
            stores,
            event_tx,
            worktree_mgr,
            integrator_config,
            fsm,
            req.id,
            markdown,
            skip_decompose,
        )
        .await
    })
}

/// Shared entry pipeline: receive plan markdown, optionally decompose, start Coordinator.
///
/// When `skip_decompose=false`: creates the goal in `Decomposing` state, spawns decomposition
/// in a background task, starts the coordinator, and returns immediately. `child_count` is
/// `null` in the response - it will be available via the `decomposition.completed` event.
///
/// When `skip_decompose=true`: behaves synchronously (used in tests).
pub(super) async fn accept_plan_markdown(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    fsm: &FsmInterpreter,
    req_id: u64,
    markdown: String,
    skip_decompose: bool,
) -> eyre::Result<DaemonResponse> {
    let title = extract_plan_title(&markdown);
    info!("doc entry: plan title = {:?}", title);

    // Deactivate any existing active CoordinatorGoals
    {
        let mut goals = stores.write_coordinator_goals()?;
        for existing in goals.values_mut() {
            if existing.active {
                existing.deactivate();
                if let Some(store_arc) = &stores.store {
                    let _ = store_arc
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .update(existing.clone());
                }
            }
        }
    }

    // Create a new CoordinatorGoal
    let goal = CoordinatorGoal::new(title.clone());
    let goal_id = goal.id.clone();

    if let Some(store_arc) = &stores.store
        && let Err(e) = store_arc
            .lock()
            .map_err(|_| eyre!("taskstore lock poisoned"))?
            .create(goal.clone())
    {
        return Ok(DaemonResponse::err(req_id, RpcError::internal(&e.to_string())));
    }
    stores.write_coordinator_goals()?.insert(goal_id.clone(), goal);
    let _ = event_tx.send(DaemonEvent::record_created("coordinator_goal", &goal_id));

    let child_count_for_response;
    if skip_decompose {
        info!("doc entry: skip_decompose=true, skipping Decomposer");
        child_count_for_response = Some(0usize);

        // Pre-create CoordinatorState starting at Planning (synchronous path, no decomposition)
        let coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Skip);
        let cs_id = coord_state.id.clone();
        if let Some(store_arc) = &stores.store {
            let _ = store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(coord_state.clone());
        }
        stores.write_coordinator_states()?.insert(cs_id, coord_state);
    } else {
        child_count_for_response = None;

        // Pre-create CoordinatorState in Decomposing: the coordinator must not advance to
        // Planning until decomposition completes. The engine's plan-decomposable trigger
        // will fire on the next tick and spawn a DecomposerAgent. The agent emits
        // decomposition.completed when done, which transitions the coordinator state.
        let mut coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Skip);
        coord_state.fsm_state = CoordinatorFsmState::Decomposing;
        let cs_id = coord_state.id.clone();
        if let Some(store_arc) = &stores.store {
            let _ = store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(coord_state.clone());
        }
        stores.write_coordinator_states()?.insert(cs_id, coord_state);

        // Create the Plan record directly from markdown (per Architect guidance:
        // instantiate natively, don't route through plan.create IPC).
        let plan_ac = AcceptanceCriteria(crate::daemon::handlers::decomposer::extract_acceptance_criteria_pub(
            &markdown,
        ));
        let mut plan = Plan::new(title.clone(), plan_ac);
        plan.force_status(PlanStatus::Active);
        let plan_id = plan.id.clone();

        // Classify tier and set on plan
        let brief = classify_brief(stores, &markdown).await;
        if brief {
            plan.tier = crate::domain::plan::Tier::Brief;
        }

        // Persist plan to TaskStore + in-memory
        if let Some(store_arc) = &stores.store {
            store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(plan.clone())
                .map_err(|e| eyre!("failed to persist plan: {}", e))?;
        }
        stores.write_plans()?.insert(plan_id.clone(), plan.clone());

        // Write plan markdown to docs/loopr/<plan_id>.md
        if let Err(e) =
            write_doc_markdown_body(std::path::Path::new(&stores.config.project.repo_path), &plan, &markdown)
        {
            warn!("docs/loopr write failed for plan {}: {}", plan_id, e);
        }

        let _ = event_tx.send(DaemonEvent::record_created("plan", &plan_id));
        info!(
            "doc entry: created Active plan {} (tier={:?}), engine will decompose",
            plan_id, plan.tier
        );
    }

    // Start the Coordinator agent
    let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
    let start_resp = Box::pin(super::dispatch(
        stores,
        event_tx,
        worktree_mgr,
        integrator_config,
        fsm,
        start_req,
    ))
    .await;
    let coordinator_already_running = start_resp.is_error();
    if coordinator_already_running {
        info!("doc entry: coordinator already running, relying on existing session");
    }
    let coordinator_session_id = start_resp
        .result
        .as_ref()
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let _ = event_tx.send(DaemonEvent::new("doc.plan_accepted", json!({ "goal_id": goal_id })));

    Ok(DaemonResponse::ok(
        req_id,
        json!({
            "child_count": child_count_for_response,
            "goal_id": goal_id,
            "coordinator_session_id": coordinator_session_id,
            "coordinator_already_running": coordinator_already_running,
        }),
    ))
}

/// Extract the plan title from the first `# ` heading line in the markdown.
/// Falls back to "Untitled Plan" if no heading is found.
pub(super) fn extract_plan_title(markdown: &str) -> String {
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("# ") {
            let title = stripped.trim();
            if !title.is_empty() {
                return title.to_string();
            }
        }
    }
    "Untitled Plan".to_string()
}

/// Classify whether a plan is Brief (no contracts, skip Spec/Phase) or Full.
///
/// Uses the tier-gate config when enabled. Defaults to Full (returns false) on any error.
/// Returns true for Brief, false for Full.
async fn classify_brief(stores: &Arc<Stores>, plan_content: &str) -> bool {
    let tg = &stores.config.tier_gate;
    if !tg.enabled {
        return false;
    }

    let tier_config = ValidatorConfig {
        llm: tg.llm.clone(),
        enabled: true,
        provider: tg.provider.clone(),
        prompts: crate::config::ValidatorPrompts::default(),
    };
    let client = LlmClient::with_reqwest(tier_config);
    let prompt_template = crate::prompts::store().tier_gate.clone();
    if prompt_template.is_empty() {
        return false;
    }
    let prompt = format!("{}\n{}", prompt_template, plan_content);
    match client.call(&prompt).await {
        Ok(response) => response.trim().to_lowercase() == "brief",
        Err(e) => {
            tracing::warn!("Tier classification failed, defaulting to Full: {}", e);
            false
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_fsm, test_integrator_config, test_stores, test_worktree_mgr,
    };
    use crate::test_util::TestDir;
    use serde_json::json;

    // --- extract_plan_title ---

    #[test]
    fn test_extract_plan_title_heading() {
        let md = "# My Plan Title\n\nSome content here.";
        assert_eq!(extract_plan_title(md), "My Plan Title");
    }

    #[test]
    fn test_extract_plan_title_no_heading() {
        let md = "No heading here, just content.";
        assert_eq!(extract_plan_title(md), "Untitled Plan");
    }

    #[test]
    fn test_extract_plan_title_skips_h2() {
        let md = "## Not a title\n# Real Title\n";
        assert_eq!(extract_plan_title(md), "Real Title");
    }

    #[test]
    fn test_extract_plan_title_empty() {
        assert_eq!(extract_plan_title(""), "Untitled Plan");
    }

    // --- doc.accept handler ---

    #[tokio::test]
    async fn test_doc_accept_missing_markdown() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({}));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[tokio::test]
    async fn test_doc_accept_empty_markdown() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": "   " }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[tokio::test]
    async fn test_doc_accept_skip_decompose_creates_goal() {
        let dir = TestDir::new("doc-accept-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let markdown = "# Auth Refactor Plan\n\n## Summary\n\nRefactor the auth module.";
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": markdown, "skip_decompose": true }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;

        assert!(!resp.is_error(), "Expected success, got: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["child_count"], 0);
        assert!(result["goal_id"].as_str().is_some(), "goal_id must be present");

        // No Doc records - Doc intermediary is gone
        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 0, "no Doc records should be created");

        // Goal should exist with correct title
        let goals = stores.read_coordinator_goals().unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals.values().next().unwrap().goal, "Auth Refactor Plan");
    }

    #[tokio::test]
    async fn test_doc_accept_creates_goal_and_state() {
        let dir = TestDir::new("doc-accept-goal-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            1,
            "doc.accept",
            json!({ "markdown": "# Test Plan\n\n## Summary\n\nDo a thing.", "skip_decompose": true }),
        );
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error());

        // CoordinatorGoal created
        let goals = stores.read_coordinator_goals().unwrap();
        assert_eq!(goals.len(), 1);
        assert!(goals.values().next().unwrap().active);

        // CoordinatorState created and starts in Planning (skip_decompose=true path)
        let states = stores.read_coordinator_states().unwrap();
        assert_eq!(states.len(), 1);
        let state = states.values().next().unwrap();
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);
    }

    #[tokio::test]
    async fn test_doc_accept_skip_decompose_child_count_zero() {
        // skip_decompose=true must return child_count=0 (not null) so callers can assert on it.
        let dir = TestDir::new("doc-accept-cc-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            1,
            "doc.accept",
            json!({ "markdown": "# Plan\n\n## Summary\n\nThing.", "skip_decompose": true }),
        );
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["child_count"], 0);
    }

    #[tokio::test]
    async fn test_doc_accept_no_skip_sets_decomposing_state() {
        // When skip_decompose is NOT set, CoordinatorState must start in Decomposing.
        // (Actual LLM decomposition does not run in this test because no API key is configured.)
        let dir = TestDir::new("doc-accept-decomposing-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            1,
            "doc.accept",
            // skip_decompose not set → background task spawned, state starts Decomposing
            json!({ "markdown": "# Plan\n\n## Summary\n\nThing." }),
        );
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        // The handler returns immediately even though decomposition is running in background.
        assert!(!resp.is_error(), "Expected success, got: {:?}", resp.error);
        let result = resp.result.unwrap();
        // child_count is null for the async path
        assert!(
            result["child_count"].is_null(),
            "child_count should be null for async path"
        );

        // CoordinatorState must be in Decomposing
        let states = stores.read_coordinator_states().unwrap();
        assert_eq!(states.len(), 1);
        let state = states.values().next().unwrap();
        assert_eq!(
            state.fsm_state,
            CoordinatorFsmState::Decomposing,
            "async path must start in Decomposing state"
        );
    }

    // --- doc.inject handler ---

    #[tokio::test]
    async fn test_doc_inject_missing_path() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({}));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("path"));
    }

    #[tokio::test]
    async fn test_doc_inject_nonexistent_file() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({ "path": "/nonexistent/plan.md" }));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not found"));
    }

    #[tokio::test]
    async fn test_doc_inject_from_file_skip_decompose() {
        let dir = TestDir::new("doc-inject-test");
        let plan_path = dir.join("my-plan.md");
        let content = "# Feature Implementation\n\n## Summary\n\nImplement the feature.";
        std::fs::write(&plan_path, content).unwrap();

        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            1,
            "doc.inject",
            json!({
                "path": plan_path.to_string_lossy(),
                "skip_decompose": true
            }),
        );
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error(), "Expected success, got: {:?}", resp.error);

        let result = resp.result.unwrap();
        assert_eq!(result["child_count"], 0);
        assert!(result["goal_id"].as_str().is_some(), "goal_id must be present");

        // No Doc records - Doc intermediary is gone
        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 0, "no Doc records should be created");

        // Goal should exist with correct title
        let goals = stores.read_coordinator_goals().unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals.values().next().unwrap().goal, "Feature Implementation");
    }

    // --- single coordinator invariant (Phase 2) ---

    #[tokio::test]
    async fn test_two_doc_accepts_start_only_one_coordinator() {
        // Two sequential doc.accept calls should result in exactly one non-terminal
        // coordinator session: the second call detects the running coordinator via the
        // atomic pool check and returns coordinator_already_running=true.
        let dir = TestDir::new("doc-accept-single-coord");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let md = "# Plan A\n\n## Summary\n\nFirst plan.";
        let req1 = DaemonRequest::new(1, "doc.accept", json!({ "markdown": md, "skip_decompose": true }));
        let fsm = crate::fsm::runtime::FsmInterpreter::embedded().unwrap();
        let resp1 = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &fsm, req1).await;
        assert!(!resp1.is_error(), "first accept should succeed");
        let r1 = resp1.result.unwrap();
        assert!(!r1["coordinator_already_running"].as_bool().unwrap_or(true));

        let md2 = "# Plan B\n\n## Summary\n\nSecond plan.";
        let req2 = DaemonRequest::new(2, "doc.accept", json!({ "markdown": md2, "skip_decompose": true }));
        let resp2 = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &fsm, req2).await;
        assert!(!resp2.is_error(), "second accept should also succeed");
        let r2 = resp2.result.unwrap();
        assert!(
            r2["coordinator_already_running"].as_bool().unwrap_or(false),
            "second call must see coordinator_already_running=true"
        );

        // Exactly one coordinator session in a non-terminal state
        let sessions = stores.read_agent_sessions().unwrap();
        let coordinator_count = sessions
            .values()
            .filter(|s| s.agent_type == crate::agents::AgentKind::Coordinator && !s.status().is_terminal())
            .count();
        assert_eq!(coordinator_count, 1, "exactly one coordinator must be running");
    }
}
