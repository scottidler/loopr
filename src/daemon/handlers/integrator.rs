use std::process::Command;
use std::sync::Arc;

use eyre::eyre;
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{debug, instrument};

use crate::config::IntegratorConfig;
use crate::domain::markdown::{build_children_markdown_content, read_full_markdown_or_empty};
use crate::domain::tick::TickStatus;
use crate::domain::validation::ValidationReport;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

/// Build the effective list of validation commands by combining global commands
/// with any phase-scoped commands from the bundles in this tick.
/// Currently returns global commands as-is; phase-scoped commands are a future extension.
fn effective_validation_commands(global_commands: &[String], _bundle_ids: &[String], _stores: &Stores) -> Vec<String> {
    global_commands.to_vec()
}

/// Run validation commands against the repo, returning (success, combined_log).
pub(super) fn run_validation_commands(commands: &[String]) -> (bool, String) {
    let mut log = String::new();
    for cmd in commands {
        log.push_str(&format!("=== Running: {cmd} ===\n"));
        let output = Command::new("sh").arg("-c").arg(cmd).output();
        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.is_empty() {
                    log.push_str(&stdout);
                    if !stdout.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !stderr.is_empty() {
                    log.push_str(&stderr);
                    if !stderr.ends_with('\n') {
                        log.push('\n');
                    }
                }
                if !out.status.success() {
                    log.push_str(&format!("=== FAILED (exit code {:?}) ===\n", out.status.code()));
                    return (false, log);
                }
                log.push_str("=== PASSED ===\n");
            }
            Err(e) => {
                log.push_str(&format!("=== FAILED to execute: {e} ===\n"));
                return (false, log);
            }
        }
    }
    (true, log)
}

/// Get the current git HEAD SHA in the given repo path.
pub fn get_git_head_sha(repo_path: &std::path::Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub(super) fn handle_integrator_validate(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_integrator_validate()");
        let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("tick_id is required"),
                ));
            }
        };

        // Verify tick exists and is in Sealing state; capture bundle_ids for phase validation
        let bundle_ids = {
            let ticks = stores.read_ticks()?;
            let tick = match ticks.get(&tick_id) {
                Some(t) => t,
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            };
            if tick.status() != TickStatus::Sealing {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::transition_rejected(&format!(
                        "tick must be in Sealing state to validate (currently {:?})",
                        tick.status()
                    )),
                ));
            }
            tick.bundle_ids.clone()
        };

        // Transition to Validating
        {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.force_status(TickStatus::Validating);

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }
        }
        debug!("[transition] tick.{}: Sealing -> Validating by Integrator", tick_id);
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "tick",
            &tick_id,
            "Sealing",
            "Validating",
            "Integrator",
        ));

        // Emit validation.started event
        let _ = event_tx.send(DaemonEvent::validation_started(&tick_id));

        // Run validation commands (global + phase-scoped, deduplicated)
        let effective_cmds = effective_validation_commands(&integrator_config.validation_commands, &bundle_ids, stores);
        let (all_passed, validation_log) = run_validation_commands(&effective_cmds);

        // Emit validation.completed event
        let _ = event_tx.send(DaemonEvent::validation_completed(&tick_id, all_passed, &validation_log));

        // Transition to Published or Failed based on results
        let final_status = if all_passed { TickStatus::Published } else { TickStatus::Failed };

        let tick_json = {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.force_status(final_status);
            tick.validation_log = validation_log;

            if all_passed {
                tick.integration_sha = get_git_head_sha(&stores.config.project.repo_path);
            }

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            match serde_json::to_value(&*tick) {
                Ok(v) => v,
                Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            }
        };

        if all_passed {
            let sha = tick_json
                .get("integration_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let _ = event_tx.send(DaemonEvent::tick_published(&tick_id, sha));
        } else {
            let _ = event_tx.send(DaemonEvent::tick_validation_failed(
                &tick_id,
                "validation commands failed",
            ));
        }

        Ok(DaemonResponse::ok(req.id, tick_json))
    })
}

pub(super) fn handle_integrator_publish(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_integrator_publish()");
        let tick_id = match req.params.get("tick_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("tick_id is required"),
                ));
            }
        };

        // Verify tick exists and determine current state
        let current_status = {
            let ticks = stores.read_ticks()?;
            match ticks.get(&tick_id) {
                Some(t) => t.status(),
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            }
        };

        // If Open, transition to Sealing first
        if current_status == TickStatus::Open {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.force_status(TickStatus::Sealing);

            // Persist to TaskStore if available
            if let Some(store) = &stores.store
                && let Err(e) = store
                    .lock()
                    .map_err(|_| eyre!("taskstore lock poisoned"))?
                    .update(tick.clone())
            {
                return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
            }

            debug!("[transition] tick.{}: Open -> Sealing by Integrator", tick_id);
            let _ = event_tx.send(DaemonEvent::transition_completed(
                "tick",
                &tick_id,
                "Open",
                "Sealing",
                "Integrator",
            ));
        } else if current_status != TickStatus::Sealing {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::transition_rejected(&format!(
                    "integrator.publish requires tick in Open or Sealing state (currently {:?})",
                    current_status
                )),
            ));
        }

        // Now delegate to validate (tick is in Sealing state)
        let validate_req = DaemonRequest::new(req.id, "integrator.validate", json!({ "tick_id": tick_id }));
        Ok(handle_integrator_validate(
            stores,
            event_tx,
            integrator_config,
            validate_req,
        ))
    })
}

// --- Validator handlers ---

#[instrument(skip_all, fields(collection = ?req.params.get("collection"), id = ?req.params.get("id")))]
pub(super) async fn handle_validator_validate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_async_handler!(req.id, {
        let validator = match &stores.validator {
            Some(v) => v.clone(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("validator is not enabled"),
                ));
            }
        };

        let collection = match req.params.get("collection").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("collection is required"),
                ));
            }
        };

        let target_id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required")));
            }
        };

        let report = match collection.as_str() {
            "plan" | "plans" => {
                let plan = {
                    let plans = stores.read_plans()?;
                    match plans.get(&target_id) {
                        Some(p) => p.clone(),
                        None => {
                            return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &target_id)));
                        }
                    }
                };
                let plan_md = read_full_markdown_or_empty(&stores.config.project.repo_path, &plan.id);
                validator.validate_plan(&target_id, &plan_md).await
            }
            "spec" | "specs" => {
                let spec = {
                    let specs = stores.read_specs()?;
                    match specs.get(&target_id) {
                        Some(s) => s.clone(),
                        None => {
                            return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &target_id)));
                        }
                    }
                };
                let spec_md = read_full_markdown_or_empty(&stores.config.project.repo_path, &spec.id);
                let plan_md = read_full_markdown_or_empty(&stores.config.project.repo_path, &spec.parent_id);
                validator.validate_spec(&target_id, &spec_md, &plan_md).await
            }
            "phase" | "phases" => {
                let phase = {
                    let phases = stores.read_phases()?;
                    match phases.get(&target_id) {
                        Some(p) => p.clone(),
                        None => {
                            return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &target_id)));
                        }
                    }
                };
                let phase_md = read_full_markdown_or_empty(&stores.config.project.repo_path, &phase.id);
                let spec_md = read_full_markdown_or_empty(&stores.config.project.repo_path, &phase.parent_id);
                validator.validate_phase(&target_id, &phase_md, &spec_md).await
            }
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!("unsupported collection for validation: {}", collection)),
                ));
            }
        };

        match report {
            Ok(report) => {
                // Persist to TaskStore
                if let Some(store) = &stores.store
                    && let Err(e) = store
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .create(report.clone())
                {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
                Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

// --- Coverage Evaluator handler ---

#[instrument(skip_all, fields(parent_collection = ?req.params.get("parent_collection"), parent_id = ?req.params.get("parent_id")))]
pub(super) async fn handle_coverage_evaluate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_async_handler!(req.id, {
        let evaluator = match &stores.evaluator {
            Some(e) => e.clone(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::internal("coverage evaluator not enabled"),
                ));
            }
        };

        let parent_collection = match req.params.get("parent_collection").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("parent_collection is required"),
                ));
            }
        };

        let parent_id = match req.params.get("parent_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("parent_id is required"),
                ));
            }
        };

        let report = match parent_collection.as_str() {
            "plan" | "plans" => {
                let children_ids = {
                    // Verify parent exists
                    let plans = stores.read_plans()?;
                    if !plans.contains_key(&parent_id) {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &parent_id)));
                    }
                    drop(plans);
                    let specs = stores.read_specs()?;
                    specs
                        .values()
                        .filter(|s| s.parent_id == parent_id)
                        .map(|s| s.id.clone())
                        .collect::<Vec<_>>()
                };
                let repo_path = &stores.config.project.repo_path;
                let parent_md = read_full_markdown_or_empty(repo_path, &parent_id);
                let children_md = build_children_markdown_content(repo_path, &children_ids);
                evaluator
                    .evaluate_plan_specs(&parent_id, &parent_md, &children_md, children_ids)
                    .await
            }
            "spec" | "specs" => {
                let children_ids = {
                    let specs = stores.read_specs()?;
                    if !specs.contains_key(&parent_id) {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &parent_id)));
                    }
                    drop(specs);
                    let phases = stores.read_phases()?;
                    phases
                        .values()
                        .filter(|p| p.parent_id == parent_id)
                        .map(|p| p.id.clone())
                        .collect::<Vec<_>>()
                };
                let repo_path = &stores.config.project.repo_path;
                let parent_md = read_full_markdown_or_empty(repo_path, &parent_id);
                let children_md = build_children_markdown_content(repo_path, &children_ids);
                evaluator
                    .evaluate_spec_phases(&parent_id, &parent_md, &children_md, children_ids)
                    .await
            }
            "phase" | "phases" => {
                let children_ids = {
                    let phases = stores.read_phases()?;
                    if !phases.contains_key(&parent_id) {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &parent_id)));
                    }
                    drop(phases);
                    let works = stores.read_works()?;
                    works
                        .values()
                        .filter(|w| w.parent_id == parent_id)
                        .map(|w| w.id.clone())
                        .collect::<Vec<_>>()
                };
                let repo_path = &stores.config.project.repo_path;
                let parent_md = read_full_markdown_or_empty(repo_path, &parent_id);
                let children_md = build_children_markdown_content(repo_path, &children_ids);
                evaluator
                    .evaluate_phase_works(&parent_id, &parent_md, &children_md, children_ids)
                    .await
            }
            _ => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params(&format!(
                        "unsupported parent_collection for coverage: {}",
                        parent_collection
                    )),
                ));
            }
        };

        match report {
            Ok(report) => {
                // Persist to TaskStore
                if let Some(store) = &stores.store
                    && let Err(e) = store
                        .lock()
                        .map_err(|_| eyre!("taskstore lock poisoned"))?
                        .create(report.clone())
                {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
                // Also store in memory
                stores
                    .write_coverage_reports()?
                    .insert(report.id.clone(), report.clone());
                Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?))
            }
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

pub(super) fn handle_validator_report(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_report()");
        let report_id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required")));
            }
        };

        // Read from TaskStore
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<ValidationReport>(&report_id)
            {
                Ok(Some(report)) => {
                    return Ok(DaemonResponse::ok(req.id, serde_json::to_value(&report)?));
                }
                Ok(None) => {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::not_found("validation_report", &report_id),
                    ));
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        Ok(DaemonResponse::err(
            req.id,
            RpcError::internal("TaskStore not available"),
        ))
    })
}

pub(super) fn handle_validator_reports(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_reports()");
        if let Some(store) = &stores.store {
            let mut filters = vec![];

            if let Some(target_id) = req.params.get("target_id").and_then(|v| v.as_str()) {
                filters.push(Filter {
                    field: "target_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(target_id.to_string()),
                });
            }

            if let Some(target_collection) = req.params.get("target_collection").and_then(|v| v.as_str()) {
                filters.push(Filter {
                    field: "target_collection".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(target_collection.to_string()),
                });
            }

            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<ValidationReport>(&filters)
            {
                Ok(reports) => Ok(DaemonResponse::ok(req.id, serde_json::to_value(&reports)?)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            }
        } else {
            Ok(DaemonResponse::ok(req.id, json!([])))
        }
    })
}

// --- Tool handlers ---

pub(super) fn handle_tool_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tool_list()");
        let tool_runner = stores.read_tool_runner()?;
        let names = tool_runner.available_tools();
        let tools: Vec<serde_json::Value> = names
            .iter()
            .filter_map(|name| {
                tool_runner.get_tool(name).map(|entry| {
                    json!({
                        "name": entry.name,
                        "command": entry.command,
                        "timeout_secs": entry.timeout_secs,
                        "worktree": entry.worktree,
                        "source": "config",
                    })
                })
            })
            .collect();
        Ok(DaemonResponse::ok(req.id, json!({ "tools": tools })))
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use crate::config::IntegratorConfig;
    use crate::daemon::context::Stores;
    use crate::daemon::handlers::tests::test_dispatch as dispatch;
    use crate::daemon::handlers::tests::{
        test_event_tx, test_integrator_config, test_stores, test_stores_with_git, test_stores_with_taskstore,
        test_stores_with_validator, test_worktree_mgr,
    };
    use crate::domain::validation::{ValidationReport, ValidationVerdict};
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    async fn create_sealing_tick(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
    ) -> String {
        let resp = dispatch(
            stores,
            tx,
            wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();
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
        )
        .await;
        assert!(!tr.is_error(), "transition failed: {:?}", tr.error);
        tick_id
    }

    #[tokio::test]
    async fn test_integrator_validate_success() {
        let (_dir, stores) = test_stores_with_git();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;

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
        )
        .await;
        assert!(!resp.is_error(), "unexpected error: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
        assert!(!result["validation_log"].as_str().unwrap().is_empty());
        assert!(result["integration_sha"].is_string());
    }

    #[tokio::test]
    async fn test_integrator_validate_failure() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;

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
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        assert!(result["validation_log"].as_str().unwrap().contains("FAILED"));
        assert!(result["integration_sha"].is_null());
    }

    #[tokio::test]
    async fn test_integrator_validate_wrong_state() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "integrator.validate", json!({"tick_id": tick_id})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Sealing"));
    }

    #[tokio::test]
    async fn test_integrator_validate_not_found() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({"tick_id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not found"));
    }

    #[tokio::test]
    async fn test_integrator_validate_missing_tick_id() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "integrator.validate", json!({})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("tick_id"));
    }

    #[tokio::test]
    async fn test_integrator_validate_events() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let mut rx = tx.subscribe();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;
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
        )
        .await;
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

    #[tokio::test]
    async fn test_integrator_publish_from_open() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        )
        .await;
        let tick_id = resp.result.unwrap()["id"].as_str().unwrap().to_string();

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
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Published");
    }

    #[tokio::test]
    async fn test_integrator_publish_from_sealing() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;

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
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Published");
    }

    #[tokio::test]
    async fn test_integrator_publish_wrong_state() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;

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
        )
        .await;

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "integrator.publish", json!({"tick_id": tick_id})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("Open or Sealing"));
    }

    #[tokio::test]
    async fn test_integrator_publish_validation_failure() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tick.create", json!({"number": 1})),
        )
        .await;
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
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap()["status"], "Failed");
    }

    #[tokio::test]
    async fn test_integrator_dispatch_routes() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        for method in &["integrator.validate", "integrator.publish"] {
            let resp = dispatch(
                &stores,
                &tx,
                &wm,
                &test_integrator_config(),
                DaemonRequest::new(1, *method, json!({})),
            )
            .await;
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

    #[tokio::test]
    async fn test_integrator_validate_multi_command_stops_on_first_failure() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let tick_id = create_sealing_tick(&stores, &tx, &wm).await;

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
        )
        .await;
        let result = resp.result.unwrap();
        assert_eq!(result["status"], "Failed");
        let log = result["validation_log"].as_str().unwrap();
        assert!(log.contains("first"));
        assert!(log.contains("FAILED"));
        assert!(!log.contains("should-not-run"));
    }

    #[tokio::test]
    async fn test_handle_validator_validate() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "plan-1"})),
        )
        .await;
        assert!(resp.is_error());

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        )
        .await;
        assert!(resp.is_error());

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
        )
        .await;
        assert!(resp.is_error());

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(4, "validator.validate", json!({"collection": "widgets", "id": "x"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported"));
    }

    #[tokio::test]
    async fn test_handle_validator_validate_no_validator() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "plans", "id": "x"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("not enabled"));
    }

    #[tokio::test]
    async fn test_handle_validator_validate_missing_params() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"id": "x"})),
        )
        .await;
        assert!(resp.is_error());

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.validate", json!({"collection": "plans"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_validator_validate_unknown_collection() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.validate", json!({"collection": "unknown", "id": "x"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("unsupported collection"));
    }

    #[tokio::test]
    async fn test_handle_validator_validate_not_found() {
        let (_dir, stores) = test_stores_with_validator();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

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
        )
        .await;
        assert!(resp.is_error());

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
        )
        .await;
        assert!(resp.is_error());

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
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_validator_report() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

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
        )
        .await;
        assert!(!resp.is_error(), "validator.report failed: {:?}", resp.error);
        assert_eq!(resp.result.unwrap()["verdict"], "pass");
    }

    #[tokio::test]
    async fn test_handle_validator_report_not_found() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "nonexistent"})),
        )
        .await;
        assert!(resp.is_error());
    }

    #[tokio::test]
    async fn test_handle_validator_report_no_taskstore() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.report", json!({"id": "any"})),
        )
        .await;
        assert!(resp.is_error());
        assert!(resp.error.unwrap().message.contains("TaskStore"));
    }

    #[tokio::test]
    async fn test_handle_validator_reports() {
        let (_dir, stores) = test_stores_with_taskstore();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

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

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        )
        .await;
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(2, "validator.reports", json!({"target_id": "plan-1"})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 1);

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(3, "validator.reports", json!({"target_collection": "plans"})),
        )
        .await;
        assert!(!resp.is_error());
        assert!(resp.result.unwrap().as_array().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn test_handle_validator_reports_no_taskstore() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "validator.reports", json!({})),
        )
        .await;
        assert!(!resp.is_error());
        assert_eq!(resp.result.unwrap().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_handle_tool_list() {
        let (_dir, stores) = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let resp = dispatch(
            &stores,
            &tx,
            &wm,
            &test_integrator_config(),
            DaemonRequest::new(1, "tool.list", json!({})),
        )
        .await;
        assert!(!resp.is_error());
        let result = resp.result.unwrap();
        assert!(result["tools"].is_array());
    }
}
