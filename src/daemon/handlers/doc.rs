//! IPC handlers for Doc-based plan entry paths.
//!
//! Two entry paths both funnel into a shared pipeline:
//!
//! - `doc.accept` (chat funnel): caller supplies plan markdown as a string.
//! - `doc.inject` (E2E test path): caller supplies a path to an existing plan .md file.
//!
//! Both paths:
//!   1. Optionally run the Decomposer (skip via `skip_decompose=true` for tests)
//!   2. Create a Plan record
//!   3. Emit plan-accepted event; the engine takes over orchestration

use std::sync::Arc;

use eyre::eyre;
use tracing::{info, instrument, warn};

use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{IntegratorConfig, ValidatorConfig};
use crate::daemon::context::Stores;
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
    _worktree_mgr: &WorktreeManager,
    _integrator_config: &IntegratorConfig,
    _fsm: &FsmInterpreter,
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

        accept_plan_markdown(stores, event_tx, req.id, markdown, skip_decompose).await
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
    _worktree_mgr: &WorktreeManager,
    _integrator_config: &IntegratorConfig,
    _fsm: &FsmInterpreter,
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

        accept_plan_markdown(stores, event_tx, req.id, markdown, skip_decompose).await
    })
}

/// Shared entry pipeline: receive plan markdown, optionally decompose, create Plan.
///
/// When `skip_decompose=false`: creates an Active Plan, spawns decomposition in a background
/// task via the engine's plan-decomposable trigger, and returns immediately. `child_count` is
/// `null` in the response - it will be available via the `decomposition.completed` event.
///
/// When `skip_decompose=true`: behaves synchronously (used in tests).
pub(super) async fn accept_plan_markdown(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req_id: u64,
    markdown: String,
    skip_decompose: bool,
) -> eyre::Result<DaemonResponse> {
    let title = extract_plan_title(&markdown);
    info!("doc entry: plan title = {:?}", title);

    let child_count_for_response;
    let plan_id;
    if skip_decompose {
        info!("doc entry: skip_decompose=true, skipping Decomposer");
        child_count_for_response = Some(0usize);

        // Create a minimal Plan for the skip_decompose path (tests)
        let plan_ac = AcceptanceCriteria(vec![]);
        let plan = Plan::new(title.clone(), plan_ac);
        plan_id = plan.id.clone();

        if let Some(store_arc) = &stores.store {
            store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(plan.clone())
                .map_err(|e| eyre!("failed to persist plan: {}", e))?;
        }
        stores.write_plans()?.insert(plan_id.clone(), plan);

        let _ = event_tx.send(DaemonEvent::record_created("plan", &plan_id));
    } else {
        child_count_for_response = None;

        // Create the Plan record directly from markdown (per Architect guidance:
        // instantiate natively, don't route through plan.create IPC).
        let plan_ac = AcceptanceCriteria(crate::daemon::handlers::decomposer::extract_acceptance_criteria_pub(
            &markdown,
        ));
        let mut plan = Plan::new(title.clone(), plan_ac);
        plan.force_status(PlanStatus::Active);
        plan_id = plan.id.clone();

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

    // Engine takes over from here: plan-is-active trigger fires decomposition,
    // reconciliation promotes hierarchy, agent-lifecycle strategies spawn implementers.
    // No coordinator agent needed - the engine IS the coordinator now.
    let _ = event_tx.send(DaemonEvent::new("doc.plan_accepted", json!({ "plan_id": &plan_id })));

    Ok(DaemonResponse::ok(
        req_id,
        json!({
            "child_count": child_count_for_response,
            "plan_id": &plan_id,
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
    async fn test_doc_accept_skip_decompose_creates_plan() {
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
        assert!(result["plan_id"].as_str().is_some(), "plan_id must be present");

        // No Doc records - Doc intermediary is gone
        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 0, "no Doc records should be created");

        // Plan should exist with correct title
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans.values().next().unwrap().title, "Auth Refactor Plan");
    }

    #[tokio::test]
    async fn test_doc_accept_creates_plan() {
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

        // Plan created
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans.len(), 1);
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
    async fn test_doc_accept_no_skip_creates_active_plan() {
        // When skip_decompose is NOT set, Plan must be Active so engine decomposes it.
        let dir = TestDir::new("doc-accept-decomposing-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let req = DaemonRequest::new(
            1,
            "doc.accept",
            // skip_decompose not set - engine will decompose
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

        // Plan must be Active for engine to pick it up
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans.len(), 1);
        let plan = plans.values().next().unwrap();
        assert_eq!(
            plan.status(),
            crate::domain::plan::PlanStatus::Active,
            "async path must create Active plan"
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
        assert!(result["plan_id"].as_str().is_some(), "plan_id must be present");

        // No Doc records - Doc intermediary is gone
        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 0, "no Doc records should be created");

        // Plan should exist with correct title
        let plans = stores.read_plans().unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans.values().next().unwrap().title, "Feature Implementation");
    }

    // --- engine-driven orchestration (v4 cutover) ---

    #[tokio::test]
    async fn test_doc_accept_no_coordinator_spawned() {
        // After v4 cutover, doc.accept does NOT spawn a coordinator agent.
        // The engine takes over orchestration via triggers and strategies.
        let dir = TestDir::new("doc-accept-no-coord");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let md = "# Plan A\n\n## Summary\n\nFirst plan.";
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": md, "skip_decompose": true }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), &test_fsm(), req).await;
        assert!(!resp.is_error(), "accept should succeed");
        let result = resp.result.unwrap();

        // Response should NOT contain coordinator fields
        assert!(result.get("coordinator_session_id").is_none());
        assert!(result.get("coordinator_already_running").is_none());

        // No coordinator sessions should exist
        let sessions = stores.read_agent_sessions().unwrap();
        let coordinator_count = sessions
            .values()
            .filter(|s| s.agent_type == crate::agents::AgentKind::Director)
            .count();
        assert_eq!(coordinator_count, 0, "no coordinator sessions should be created");
    }
}
