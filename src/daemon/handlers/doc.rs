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

use eyre::{bail, eyre};
use tracing::{info, instrument, warn};

use serde_json::json;
use tokio::sync::broadcast;

use crate::config::{IntegratorConfig, InterviewMode, ValidatorConfig};
use crate::daemon::context::Stores;
use crate::decomposer::{decompose_hierarchy, extract_acceptance_criteria};
use crate::domain::coordinator_goal::CoordinatorGoal;
use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};
use crate::domain::doc::{Doc, DocKind, write_doc_file};
use crate::domain::phase::{Phase, PhaseStatus};
use crate::domain::plan::{Plan, PlanStatus};
use crate::domain::spec::{Spec, SpecStatus};
use crate::domain::work::{Work, WorkStatus};
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::validator::client::{LlmClient, ReqwestClient};
use crate::worktree::manager::WorktreeManager;

use super::common::persist_doc;

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
            req.id,
            markdown,
            skip_decompose,
        )
        .await
    })
}

/// Shared entry pipeline: write plan.md, create Doc, optionally decompose, start Coordinator.
///
/// When `skip_decompose=false`: creates the plan Doc, spawns decomposition in a background
/// task, creates the goal in `Decomposing` state, starts the coordinator, and returns
/// immediately. `child_count` is `null` in the response - it will be available via the
/// `decomposition.completed` event.
///
/// When `skip_decompose=true`: behaves synchronously (used in tests).
pub(super) async fn accept_plan_markdown(
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

    // Create a transient working directory for this decomposition run.
    // Uses a timestamp-based name under system temp so runs don't accumulate in the repo.
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let run_dir = std::env::temp_dir().join(format!("loopr-run-{}", ts));
    std::fs::create_dir_all(&run_dir).map_err(|e| eyre!("Failed to create run directory: {}", e))?;
    info!("doc entry: run_dir = {}", run_dir.display());

    // Write plan.md into the run directory
    let filename = write_doc_file(&run_dir, DocKind::Plan, &title, &markdown, &[])
        .map_err(|e| eyre!("Failed to write plan.md: {}", e))?;

    // Build the plan Doc record
    let mut plan_doc = Doc::new(DocKind::Plan, None, title.clone(), filename);
    plan_doc.acceptance_criteria = extract_acceptance_criteria(&markdown);
    let plan_doc_id = plan_doc.id.clone();

    // Persist the plan Doc
    persist_doc(stores, plan_doc.clone(), event_tx)?;

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
        // Planning until the background decomposition task has persisted all child docs.
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

        // Spawn background decomposition task
        let stores_bg = Arc::clone(stores);
        let event_tx_bg = event_tx.clone();
        let plan_doc_bg = plan_doc.clone();
        let run_dir_bg = run_dir.clone();
        let markdown_bg = markdown.clone();
        let goal_id_bg = goal_id.clone();
        let dc = stores.config.decomposer.clone();

        tokio::spawn(async move {
            let decomposer_prefix = format!("[decomposer:{}]", goal_id_bg);
            let brief = classify_brief(&stores_bg, &markdown_bg).await;
            info!("{} starting decomposition (brief={})", decomposer_prefix, brief);
            let client = ReqwestClient::new();
            match decompose_hierarchy(&plan_doc_bg, &run_dir_bg, &dc, &client, brief).await {
                Ok((child_docs, partial_err)) => {
                    let child_count = child_docs.len();
                    info!(
                        "{} decomposition produced {} child docs",
                        decomposer_prefix, child_count
                    );
                    match double_write_old_records(&stores_bg, &plan_doc_bg, &markdown_bg, &child_docs, &run_dir_bg) {
                        Ok(()) => {
                            for child in child_docs {
                                if let Err(e) = persist_doc(&stores_bg, child, &event_tx_bg) {
                                    warn!("doc entry (bg): failed to persist child doc: {}", e);
                                }
                            }
                            if let Some(err) = partial_err {
                                // Partial success: some spec branches failed. Persist completed
                                // docs (done above) but signal failure so coordinator surfaces
                                // NeedsHelp instead of busy-polling.
                                if let Ok(mut states) = stores_bg.write_coordinator_states()
                                    && let Some(cs) = states.values_mut().find(|cs| cs.goal_id == *goal_id_bg)
                                {
                                    cs.decomposition_error = Some(err.clone());
                                    cs.updated_at = crate::id::now_millis();
                                    if let Some(store_arc) = &stores_bg.store
                                        && let Ok(mut store) = store_arc.lock()
                                    {
                                        let _ = store.update(cs.clone());
                                    }
                                }
                                let _ = event_tx_bg.send(DaemonEvent::new(
                                    "decomposition.failed",
                                    json!({ "goal_id": goal_id_bg, "error": err }),
                                ));
                            } else {
                                let _ = event_tx_bg.send(DaemonEvent::new(
                                    "decomposition.completed",
                                    json!({ "goal_id": goal_id_bg, "child_count": child_count }),
                                ));
                            }
                        }
                        Err(e) => {
                            warn!("{} persist failed: {}", decomposer_prefix, e);
                            if let Ok(mut states) = stores_bg.write_coordinator_states()
                                && let Some(cs) = states.values_mut().find(|cs| cs.goal_id == *goal_id_bg)
                            {
                                cs.decomposition_error = Some(e.to_string());
                                cs.updated_at = crate::id::now_millis();
                                if let Some(store_arc) = &stores_bg.store
                                    && let Ok(mut store) = store_arc.lock()
                                {
                                    let _ = store.update(cs.clone());
                                }
                            }
                            let _ = event_tx_bg.send(DaemonEvent::new(
                                "decomposition.failed",
                                json!({ "goal_id": goal_id_bg, "error": e.to_string() }),
                            ));
                        }
                    }
                }
                Err(e) => {
                    warn!("{} decomposition failed: {}", decomposer_prefix, e);
                    if let Ok(mut states) = stores_bg.write_coordinator_states()
                        && let Some(cs) = states.values_mut().find(|cs| cs.goal_id == *goal_id_bg)
                    {
                        cs.decomposition_error = Some(e.to_string());
                        cs.updated_at = crate::id::now_millis();
                        if let Some(store_arc) = &stores_bg.store
                            && let Ok(mut store) = store_arc.lock()
                        {
                            let _ = store.update(cs.clone());
                        }
                    }
                    let _ = event_tx_bg.send(DaemonEvent::new(
                        "decomposition.failed",
                        json!({ "goal_id": goal_id_bg, "error": e.to_string() }),
                    ));
                }
            }
        });
    }

    // Start the Coordinator agent
    let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
    let start_resp = Box::pin(super::dispatch(
        stores,
        event_tx,
        worktree_mgr,
        integrator_config,
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

    let _ = event_tx.send(DaemonEvent::new(
        "doc.plan_accepted",
        json!({ "doc_id": plan_doc_id, "goal_id": goal_id }),
    ));

    Ok(DaemonResponse::ok(
        req_id,
        json!({
            "doc_id": plan_doc_id,
            "run_dir": run_dir.to_string_lossy(),
            "child_count": child_count_for_response,
            "goal_id": goal_id,
            "coordinator_session_id": coordinator_session_id,
            "coordinator_already_running": coordinator_already_running,
        }),
    ))
}

/// Double-write old-style Plan/Spec/Phase/Work records alongside Docs so the
/// Coordinator (which reads old stores) can execute the plan.
///
/// Returns `Err` on any failure; the caller must treat a failed double-write as
/// `decomposition.failed` so the coordinator never gets stuck in `Decomposing`.
/// A fully-built old-format record ready for store insertion.
enum OldRecord {
    Plan(Plan),
    Spec(Spec),
    Phase(Phase),
    Work(Work),
}

impl OldRecord {
    fn old_id(&self) -> &str {
        match self {
            OldRecord::Plan(r) => &r.id,
            OldRecord::Spec(r) => &r.id,
            OldRecord::Phase(r) => &r.id,
            OldRecord::Work(r) => &r.id,
        }
    }
}

/// Build all old-format records in memory (Phase A of double_write_old_records).
///
/// Pure computation: reads markdown files, constructs Plan/Spec/Phase/Work values,
/// resolves parent IDs. Returns Err immediately if any file is missing or a parent
/// reference cannot be resolved. No DB writes happen in this function.
fn build_all_old_records(
    plan_doc: &Doc,
    plan_markdown: &str,
    child_docs: &[Doc],
    run_dir: &Path,
) -> eyre::Result<Vec<OldRecord>> {
    let mut records: Vec<OldRecord> = Vec::new();
    let mut doc_to_old: HashMap<String, String> = HashMap::new();

    use crate::domain::criteria::AcceptanceCriteria;
    let title = extract_plan_title(plan_markdown);
    let ac = AcceptanceCriteria(plan_doc.acceptance_criteria.clone());
    let mut old_plan = Plan::new(title, plan_markdown.to_string(), ac);
    old_plan.force_status(PlanStatus::Active);
    let old_plan_id = old_plan.id.clone();
    doc_to_old.insert(plan_doc.id.clone(), old_plan_id);
    records.push(OldRecord::Plan(old_plan));

    // Process specs before phases before works so parent records exist first.
    let specs: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Spec).collect();
    let phases: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Phase).collect();
    let works: Vec<&Doc> = child_docs.iter().filter(|d| d.kind == DocKind::Work).collect();
    let mut phase_counters: HashMap<String, u32> = HashMap::new();

    for child in specs.iter().chain(phases.iter()).chain(works.iter()) {
        let Some(ref doc_parent_id) = child.parent_id else {
            continue;
        };
        let Some(old_parent_id) = doc_to_old.get(doc_parent_id).cloned() else {
            bail!("double-write: no old record for parent Doc {}", doc_parent_id);
        };
        let content = std::fs::read_to_string(run_dir.join(&child.markdown))
            .map_err(|e| eyre!("double-write: failed to read {}: {}", child.markdown, e))?;
        let child_title = child.title.clone();

        let record = match child.kind {
            DocKind::Spec => {
                let mut spec = Spec::new(old_parent_id, child_title, content);
                spec.force_status(SpecStatus::Active);
                OldRecord::Spec(spec)
            }
            DocKind::Phase => {
                let order = phase_counters.entry(doc_parent_id.clone()).or_insert(0);
                let current_order = *order;
                *order += 1;
                let mut phase = Phase::new(old_parent_id, child_title, content, current_order);
                phase.force_status(PhaseStatus::Active);
                OldRecord::Phase(phase)
            }
            DocKind::Work => {
                let mut work = Work::new(old_parent_id, child_title, content);
                work.force_status(WorkStatus::Ready);
                work.acceptance_criteria =
                    crate::domain::criteria::AcceptanceCriteria(child.acceptance_criteria.clone());
                work.dependencies = child
                    .dependencies
                    .iter()
                    .filter_map(|dep_doc_id| doc_to_old.get(dep_doc_id).cloned())
                    .collect();
                OldRecord::Work(work)
            }
            DocKind::Plan => continue,
        };
        doc_to_old.insert(child.id.clone(), record.old_id().to_string());
        records.push(record);
    }

    Ok(records)
}

fn double_write_old_records(
    stores: &Arc<Stores>,
    plan_doc: &Doc,
    plan_markdown: &str,
    child_docs: &[Doc],
    run_dir: &Path,
) -> eyre::Result<()> {
    // Phase A: build all records in memory; fails fast before any DB write
    let records = build_all_old_records(plan_doc, plan_markdown, child_docs, run_dir)?;

    // Phase B: hold the TaskStore lock for the entire batch so no concurrent reader
    // observes a half-written hierarchy during the write window
    if let Some(store_arc) = &stores.store {
        let mut store = store_arc.lock().map_err(|_| eyre!("taskstore lock poisoned"))?;
        for record in &records {
            match record {
                OldRecord::Plan(r) => store
                    .create(r.clone())
                    .map_err(|e| eyre!("failed to persist old Plan: {}", e))?,
                OldRecord::Spec(r) => store
                    .create(r.clone())
                    .map_err(|e| eyre!("failed to persist old Spec: {}", e))?,
                OldRecord::Phase(r) => store
                    .create(r.clone())
                    .map_err(|e| eyre!("failed to persist old Phase: {}", e))?,
                OldRecord::Work(r) => store
                    .create(r.clone())
                    .map_err(|e| eyre!("failed to persist old Work: {}", e))?,
            };
        }
        // lock drops here
    }

    // Phase C: update in-memory maps
    for record in records {
        match record {
            OldRecord::Plan(r) => {
                stores.write_plans()?.insert(r.id.clone(), r);
            }
            OldRecord::Spec(r) => {
                stores.write_specs()?.insert(r.id.clone(), r);
            }
            OldRecord::Phase(r) => {
                stores.write_phases()?.insert(r.id.clone(), r);
            }
            OldRecord::Work(r) => {
                stores.write_works()?.insert(r.id.clone(), r);
            }
        }
    }

    Ok(())
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
        enabled: true,
        provider: tg.provider.clone(),
        model: tg.model.clone(),
        api_key_env: tg.api_key_env.clone(),
        max_tokens: tg.max_tokens,
        temperature: tg.temperature,
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

    #[tokio::test]
    async fn test_doc_accept_missing_markdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({}));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[tokio::test]
    async fn test_doc_accept_empty_markdown() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": "   " }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("markdown"));
    }

    #[tokio::test]
    async fn test_doc_accept_skip_decompose_creates_doc() {
        let dir = TestDir::new("doc-accept-test");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let markdown = "# Auth Refactor Plan\n\n## Summary\n\nRefactor the auth module.";
        let req = DaemonRequest::new(1, "doc.accept", json!({ "markdown": markdown, "skip_decompose": true }));
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;

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

        // Title must be stored on the Doc (not "Untitled Plan")
        assert_eq!(doc.title, "Auth Refactor Plan", "Doc.title must match the H1 heading");
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
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;
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
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;
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
        let resp = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req).await;
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
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({}));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req).await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("path"));
    }

    #[tokio::test]
    async fn test_doc_inject_nonexistent_file() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "doc.inject", json!({ "path": "/nonexistent/plan.md" }));
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req).await;
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
        let resp = handle_doc_inject(&stores, &tx, &wm, &test_integrator_config(), req).await;
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
        let doc = Doc::new(DocKind::Plan, None, "Test Plan".to_string(), "plan-test.md".to_string());
        let id = doc.id.clone();

        persist_doc(&stores, doc, &tx).unwrap();

        let docs = stores.read_docs().unwrap();
        assert!(docs.contains_key(&id));
    }

    // --- double_write_old_records ---

    #[test]
    fn test_double_write_returns_err_when_child_file_missing() {
        // If a child doc references a markdown file that doesn't exist on disk,
        // double_write_old_records must return Err (not silently skip).
        let dir = TestDir::new("double-write-missing-file");
        let mut stores = crate::daemon::context::Stores::new();
        stores.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores);

        let plan_markdown = "# My Plan\n\n## Summary\n\nDo stuff.\n";
        let plan_doc = Doc::new(DocKind::Plan, None, "My Plan".to_string(), "plan.md".to_string());

        // Child references a file that doesn't exist in the run_dir
        let mut child = Doc::new(
            DocKind::Spec,
            Some(plan_doc.id.clone()),
            "Spec One".to_string(),
            "spec-one.md".to_string(), // file is not created on disk
        );
        child.parent_id = Some(plan_doc.id.clone());

        let run_dir = dir.to_path_buf();
        let result = double_write_old_records(&stores, &plan_doc, plan_markdown, &[child], &run_dir);

        assert!(result.is_err(), "expected Err when child file is missing");
    }

    #[test]
    fn test_build_all_old_records_fails_on_missing_parent() {
        // A child doc whose parent_id is not in the plan hierarchy must cause
        // build_all_old_records to return Err before any DB write occurs.
        let dir = TestDir::new("double-write-missing-parent");

        let plan_markdown = "# Test Plan\n\n## Summary\n\nDo stuff.\n";
        let plan_doc = Doc::new(DocKind::Plan, None, "Test Plan".to_string(), "plan.md".to_string());

        // Child has an unknown parent_id (not the plan's id)
        let mut child = Doc::new(
            DocKind::Spec,
            Some("unknown-parent-id".to_string()),
            "Orphan Spec".to_string(),
            "orphan.md".to_string(),
        );
        child.parent_id = Some("unknown-parent-id".to_string());
        std::fs::write(dir.join("orphan.md"), "# Orphan").unwrap();

        let result = build_all_old_records(&plan_doc, plan_markdown, &[child], &dir);

        assert!(result.is_err(), "expected Err for missing parent reference");
        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("no old record for parent Doc"),
            "error should mention missing parent: {msg}"
        );
    }

    #[test]
    fn test_double_write_no_interleaved_reads_on_failure() {
        // When build_all_old_records fails (child file missing), double_write_old_records
        // must return Err without having written any record to the store or in-memory maps.
        let dir = TestDir::new("double-write-atomic");
        let mut stores_inner = crate::daemon::context::Stores::new();
        stores_inner.config.project.repo_path = dir.to_path_buf();
        let stores = std::sync::Arc::new(stores_inner);

        let plan_markdown = "# Atomic Plan\n\n## Summary\n\nDo stuff.\n";
        let plan_doc = Doc::new(DocKind::Plan, None, "Atomic Plan".to_string(), "plan.md".to_string());

        // One valid spec (file present) and one invalid spec (file missing)
        let mut good_spec = Doc::new(
            DocKind::Spec,
            Some(plan_doc.id.clone()),
            "Good Spec".to_string(),
            "good.md".to_string(),
        );
        good_spec.parent_id = Some(plan_doc.id.clone());
        std::fs::write(dir.join("good.md"), "# Good Spec").unwrap();

        let mut bad_spec = Doc::new(
            DocKind::Spec,
            Some(plan_doc.id.clone()),
            "Bad Spec".to_string(),
            "bad.md".to_string(), // file intentionally missing
        );
        bad_spec.parent_id = Some(plan_doc.id.clone());

        let result = double_write_old_records(&stores, &plan_doc, plan_markdown, &[good_spec, bad_spec], &dir);

        // Must fail (Phase A detected the missing file before any DB write)
        assert!(result.is_err(), "expected Err when a child file is missing");

        // In-memory store must be empty (no partial write)
        let plans = stores.read_plans().unwrap();
        assert!(plans.is_empty(), "no Plan should be in memory after Phase A failure");
        let specs = stores.read_specs().unwrap();
        assert!(specs.is_empty(), "no Spec should be in memory after Phase A failure");
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
        let resp1 = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req1).await;
        assert!(!resp1.is_error(), "first accept should succeed");
        let r1 = resp1.result.unwrap();
        assert!(!r1["coordinator_already_running"].as_bool().unwrap_or(true));

        let md2 = "# Plan B\n\n## Summary\n\nSecond plan.";
        let req2 = DaemonRequest::new(2, "doc.accept", json!({ "markdown": md2, "skip_decompose": true }));
        let resp2 = handle_doc_accept(&stores, &tx, &wm, &test_integrator_config(), req2).await;
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
