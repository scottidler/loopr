//! IPC handlers for Doc-based plan entry paths.
//!
//! Two entry paths both funnel into a shared pipeline:
//!
//! - `doc.accept` (chat funnel): caller supplies plan markdown as a string.
//! - `doc.inject` (E2E test path): caller supplies a path to an existing plan .md file.
//!
//! Both paths:
//!   1. Create a run directory under `<repo>/.loopr/runs/YYYYMMDD-HHMMSS/`
//!   2. Write the plan .md file into the run directory
//!   3. Create a `Doc` record (DocKind::Plan) and persist it
//!   4. Optionally run the Decomposer (skip via `skip_decompose=true` for tests)
//!   5. Persist child Docs produced by the Decomposer
//!   6. Create a CoordinatorGoal and CoordinatorState
//!   7. Start the Coordinator agent
//!
//! The manifest entry path (`coordinator.seed_manifest`) is in coordinator.rs and
//! skips the Decomposer, injecting a pre-decomposed hierarchy directly.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use eyre::eyre;
use log::{debug, info, warn};
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{IntegratorConfig, InterviewMode, ValidatorConfig};
use crate::daemon::context::Stores;
use crate::decomposer::{decompose_hierarchy, extract_acceptance_criteria};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::CoordinatorState;
use crate::domain::doc::{Doc, DocKind, create_run_dir, write_doc_file};
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{Plan, PlanStatus};
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::validator::client::{LlmClient, UreqClient};
use crate::worktree::manager::WorktreeManager;

use super::common::persist_doc;

/// Handle `doc.accept`: accept plan markdown as a string and run the full entry pipeline.
///
/// Required params:
/// - `markdown` (string): the plan document content
///
/// Optional params:
/// - `skip_decompose` (bool, default false): skip the Decomposer (useful in tests)
pub(super) fn handle_doc_accept(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!(
            "handle_doc_accept(markdown_len={:?})",
            req.params.get("markdown").and_then(|v| v.as_str()).map(|s| s.len())
        );

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
            req.id,
            markdown,
            skip_decompose,
        )
    })
}

/// Handle `doc.inject`: read plan markdown from a file path and run the full entry pipeline.
///
/// Required params:
/// - `path` (string): absolute path to a plan .md file on disk
///
/// Optional params:
/// - `skip_decompose` (bool, default false): skip the Decomposer (useful in tests)
pub(super) fn handle_doc_inject(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!(
            "handle_doc_inject(path={:?})",
            req.params.get("path").and_then(|v| v.as_str())
        );

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
            req.id,
            markdown,
            skip_decompose,
        )
    })
}

/// Shared entry pipeline: write plan.md, create Doc, optionally decompose, start Coordinator.
pub(super) fn accept_plan_markdown(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req_id: u64,
    markdown: String,
    skip_decompose: bool,
) -> eyre::Result<DaemonResponse> {
    let title = extract_plan_title(&markdown);
    info!("doc entry: plan title = {:?}", title);

    // Create run directory under <repo>/.loopr/runs/YYYYMMDD-HHMMSS/
    let project_root = stores.config.project.repo_path.clone();
    let run_dir = create_run_dir(&project_root).map_err(|e| eyre!("Failed to create run directory: {}", e))?;
    info!("doc entry: run_dir = {}", run_dir.display());

    // Write plan.md into the run directory
    let filename = write_doc_file(&run_dir, DocKind::Plan, &title, &markdown, &[])
        .map_err(|e| eyre!("Failed to write plan.md: {}", e))?;

    // Build the plan Doc record
    let mut plan_doc = Doc::new(DocKind::Plan, None, filename);
    plan_doc.acceptance_criteria = extract_acceptance_criteria(&markdown);
    let plan_doc_id = plan_doc.id.clone();

    // Persist the plan Doc
    persist_doc(stores, plan_doc.clone(), event_tx)?;

    let child_count;
    if skip_decompose {
        info!("doc entry: skip_decompose=true, skipping Decomposer");
        child_count = 0;
    } else {
        let dc = &stores.config.decomposer;
        let client = UreqClient;

        let brief = classify_brief(stores, &markdown);
        info!("doc entry: starting decomposition (brief={})", brief);

        let child_docs = decompose_hierarchy(&plan_doc, &run_dir, dc, &client, brief)
            .map_err(|e| eyre!("Decomposition failed: {}", e))?;

        child_count = child_docs.len();
        info!("doc entry: decomposition produced {} child docs", child_count);

        double_write_old_records(stores, &plan_doc, &markdown, &child_docs, &run_dir);

        for child in child_docs {
            persist_doc(stores, child, event_tx)?;
        }
    }

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

    // Pre-create CoordinatorState (InterviewMode::Skip -> starts at Planning)
    {
        let coord_state = CoordinatorState::new(goal_id.clone(), InterviewMode::Skip);
        let cs_id = coord_state.id.clone();
        if let Some(store_arc) = &stores.store {
            let _ = store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(coord_state.clone());
        }
        stores.write_coordinator_states()?.insert(cs_id, coord_state);
    }

    // Start the Coordinator agent (best-effort; no-op if no Tokio runtime in tests)
    let (coordinator_session_id, coordinator_already_running) = if tokio::runtime::Handle::try_current().is_ok() {
        let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
        let start_resp = super::dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
        let already_running = start_resp.is_error();
        let session_id = start_resp
            .result
            .as_ref()
            .and_then(|r| r.get("session_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        (session_id, already_running)
    } else {
        debug!("No Tokio runtime available; Coordinator agent not auto-started");
        (String::new(), false)
    };

    let _ = event_tx.send(DaemonEvent::new(
        "doc.plan_accepted",
        json!({ "doc_id": plan_doc_id, "goal_id": goal_id }),
    ));

    Ok(DaemonResponse::ok(
        req_id,
        json!({
            "doc_id": plan_doc_id,
            "run_dir": run_dir.to_string_lossy(),
            "child_count": child_count,
            "goal_id": goal_id,
            "coordinator_session_id": coordinator_session_id,
            "coordinator_already_running": coordinator_already_running,
        }),
    ))
}

/// Double-write old-style Plan/Spec/Phase/Work records alongside Docs so the
/// Coordinator (which reads old stores) can execute the plan.
///
/// Non-fatal: logs warnings on any failure and continues. The Doc records are
/// the source of truth; old records are transitional.
fn double_write_old_records(
    stores: &Arc<Stores>,
    plan_doc: &Doc,
    plan_markdown: &str,
    child_docs: &[Doc],
    run_dir: &Path,
) {
    let title = extract_plan_title(plan_markdown);
    let ac_joined = plan_doc.acceptance_criteria.join("\n");

    // Create old Plan record
    let mut old_plan = Plan::new(title, plan_markdown.to_string(), ac_joined);
    old_plan.force_status(PlanStatus::Active);
    let old_plan_id = old_plan.id.clone();

    let persist_plan = (|| {
        if let Some(store_arc) = &stores.store {
            store_arc
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(old_plan.clone())
                .map_err(|e| eyre!("Failed to persist old Plan: {}", e))?;
        }
        stores.write_plans()?.insert(old_plan_id.clone(), old_plan);
        Ok::<(), eyre::Error>(())
    })();
    if let Err(e) = persist_plan {
        warn!("double-write: failed to create old Plan: {}", e);
        return;
    }

    let mut doc_to_old: HashMap<String, String> = HashMap::new();
    doc_to_old.insert(plan_doc.id.clone(), old_plan_id);

    // Sort by kind so parents are always created before children
    let specs: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Spec).collect();
    let phases: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Phase).collect();
    let works: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Work).collect();

    let mut phase_counters: HashMap<String, u32> = HashMap::new();

    for child in specs.iter().chain(phases.iter()).chain(works.iter()) {
        let Some(ref doc_parent_id) = child.parent_id else {
            continue;
        };
        let Some(old_parent_id) = doc_to_old.get(doc_parent_id).cloned() else {
            warn!("double-write: no old record for parent Doc {}", doc_parent_id);
            continue;
        };

        let content = match std::fs::read_to_string(run_dir.join(&child.markdown)) {
            Ok(c) => c,
            Err(e) => {
                warn!("double-write: failed to read {}: {}", child.markdown, e);
                continue;
            }
        };
        let child_title = extract_plan_title(&content);

        let old_id = match child.kind {
            DocKind::Spec => {
                let mut spec = Spec::new(old_parent_id, child_title, content);
                spec.force_status(SpecStatus::Active);
                let old_id = spec.id.clone();
                let res = (|| {
                    if let Some(store_arc) = &stores.store {
                        store_arc
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .create(spec.clone())
                            .map_err(|e| eyre!("Failed to persist old Spec: {}", e))?;
                    }
                    stores.write_specs()?.insert(old_id.clone(), spec);
                    Ok::<(), eyre::Error>(())
                })();
                if let Err(e) = res {
                    warn!("double-write: failed to create old Spec: {}", e);
                    continue;
                }
                old_id
            }
            DocKind::Phase => {
                let order = phase_counters.entry(doc_parent_id.clone()).or_insert(0);
                let current_order = *order;
                *order += 1;
                let mut phase = Phase::new(old_parent_id, child_title, content, current_order);
                phase.force_status(PhaseStatus::Active);
                let old_id = phase.id.clone();
                let res = (|| {
                    if let Some(store_arc) = &stores.store {
                        store_arc
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .create(phase.clone())
                            .map_err(|e| eyre!("Failed to persist old Phase: {}", e))?;
                    }
                    stores.write_phases()?.insert(old_id.clone(), phase);
                    Ok::<(), eyre::Error>(())
                })();
                if let Err(e) = res {
                    warn!("double-write: failed to create old Phase: {}", e);
                    continue;
                }
                old_id
            }
            DocKind::Work => {
                let mut work = Work::new(old_parent_id, child_title, content);
                work.force_status(WorkStatus::Ready);
                work.acceptance_criteria = child.acceptance_criteria.clone();
                work.dependencies = child
                    .dependencies
                    .iter()
                    .filter_map(|dep_doc_id| doc_to_old.get(dep_doc_id).cloned())
                    .collect();
                let old_id = work.id.clone();
                let res = (|| {
                    if let Some(store_arc) = &stores.store {
                        store_arc
                            .lock()
                            .map_err(|_| eyre!("taskstore lock poisoned"))?
                            .create(work.clone())
                            .map_err(|e| eyre!("Failed to persist old Work: {}", e))?;
                    }
                    stores.write_works()?.insert(old_id.clone(), work);
                    Ok::<(), eyre::Error>(())
                })();
                if let Err(e) = res {
                    warn!("double-write: failed to create old Work: {}", e);
                    continue;
                }
                old_id
            }
            DocKind::Plan => continue,
        };
        doc_to_old.insert(child.id.clone(), old_id);
    }
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
fn classify_brief(stores: &Arc<Stores>, plan_content: &str) -> bool {
    let tg = &stores.config.tier_gate;
    if !tg.enabled {
        return false;
    }

    let tier_config = ValidatorConfig {
        enabled: true,
        provider: tg.provider.clone(),
        model: tg.model.clone(),
        api_key_env: tg.api_key_env.clone(),
        max_tokens: tg.max_tokens,
        temperature: tg.temperature,
    };
    let client = LlmClient::with_ureq(tier_config);
    let prompt_template = crate::prompts::store().tier_gate.clone();
    if prompt_template.is_empty() {
        return false;
    }
    let prompt = format!("{}\n{}", prompt_template, plan_content);
    match client.call(&prompt) {
        Ok(response) => response.trim().to_lowercase() == "brief",
        Err(e) => {
            log::warn!("Tier classification failed, defaulting to Full: {}", e);
            false
        }
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::handlers::tests::{test_event_tx, test_integrator_config, test_stores, test_worktree_mgr};
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

    #[test]
    fn test_doc_accept_missing_markdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({}));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[test]
    fn test_doc_accept_empty_markdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": "   " }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[test]
    fn test_doc_accept_skip_decompose_creates_doc() {
        let dir = TestDir::new("doc-accept-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let markdown = "# Auth Refactor Plan\n\n## Summary\n\nRefactor the auth module.";
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": markdown, "skip_decompose": true }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req);

        assert!(!resp.is_error(), "Expected success, got: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert!(result["doc_id"].as_str().unwrap().starts_with("pl-"));
        assert_eq!(result["child_count"], 0);

        // Doc should be in the in-memory store
        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 1);
        let doc = docs.values().next().unwrap();
        assert_eq!(doc.kind, DocKind::Plan);
        assert!(doc.markdown.ends_with(".md"));

        // plan.md should exist on disk
        let run_dir_str = result["run_dir"].as_str().unwrap();
        let plan_path = std::path::Path::new(run_dir_str).join(&doc.markdown);
        assert!(plan_path.exists(), "plan.md not found at {}", plan_path.display());
        let content = std::fs::read_to_string(&plan_path).unwrap();
        assert!(content.contains("Auth Refactor Plan"));
    }

    #[test]
    fn test_doc_accept_creates_goal_and_state() {
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
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());

        // CoordinatorGoal created
        let goals = stores.read_coordinator_goals().unwrap();
        assert_eq!(goals.len(), 1);
        assert!(goals.values().next().unwrap().active);

        // CoordinatorState created
        let states = stores.read_coordinator_states().unwrap();
        assert_eq!(states.len(), 1);
    }

    // --- doc.inject handler ---

    #[test]
    fn test_doc_inject_missing_path() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({}));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("path"));
    }

    #[test]
    fn test_doc_inject_nonexistent_file() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({ "path": "/nonexistent/plan.md" }));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not found"));
    }

    #[test]
    fn test_doc_inject_from_file_skip_decompose() {
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
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "Expected success, got: {:?}", resp.error);

        let result = resp.result.unwrap();
        assert!(result["doc_id"].as_str().unwrap().starts_with("pl-"));
        assert_eq!(result["child_count"], 0);

        let docs = stores.read_docs().unwrap();
        assert_eq!(docs.len(), 1);
        let doc = docs.values().next().unwrap();
        assert_eq!(doc.kind, DocKind::Plan);

        // The plan.md content should be copied to the run directory
        let run_dir = std::path::Path::new(result["run_dir"].as_str().unwrap());
        let written = std::fs::read_to_string(run_dir.join(&doc.markdown)).unwrap();
        assert!(written.contains("Feature Implementation"));
    }

    // --- persist_doc ---

    #[test]
    fn test_persist_doc_in_memory() {
        let stores = test_stores();
        let tx = test_event_tx();
        let doc = Doc::new(DocKind::Plan, None, "plan-test.md".to_string());
        let id = doc.id.clone();

        persist_doc(&stores, doc, &tx).unwrap();

        let docs = stores.read_docs().unwrap();
        assert!(docs.contains_key(&id));
    }
}
