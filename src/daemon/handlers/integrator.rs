use std::process::Command;
use std::sync::Arc;

use eyre::eyre;
use log::debug;
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::IntegratorConfig;
use crate::domain::tick::TickStatus;
use crate::domain::validation::ValidationReport;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

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
pub(super) fn get_git_head_sha(repo_path: &std::path::Path) -> Option<String> {
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

        // Verify tick exists and is in Sealing state
        {
            let ticks = stores.read_ticks()?;
            let tick = match ticks.get(&tick_id) {
                Some(t) => t,
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            };
            if tick.status != TickStatus::Sealing {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::transition_rejected(&format!(
                        "tick must be in Sealing state to validate (currently {:?})",
                        tick.status
                    )),
                ));
            }
        }

        // Transition to Validating
        {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = TickStatus::Validating;
            tick.updated_at = crate::id::now_millis();

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

        // Run validation commands
        let (all_passed, validation_log) = run_validation_commands(&integrator_config.validation_commands);

        // Emit validation.completed event
        let _ = event_tx.send(DaemonEvent::validation_completed(&tick_id, all_passed, &validation_log));

        // Transition to Published or Failed based on results
        let final_status = if all_passed { TickStatus::Published } else { TickStatus::Failed };

        let tick_json = {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = final_status;
            tick.validation_log = validation_log;
            tick.updated_at = crate::id::now_millis();

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
                Some(t) => t.status,
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("tick", &tick_id))),
            }
        };

        // If Open, transition to Sealing first
        if current_status == TickStatus::Open {
            let mut ticks = stores.write_ticks()?;
            let tick = ticks
                .get_mut(&tick_id)
                .ok_or_else(|| eyre!("record not found: {tick_id}"))?;
            tick.status = TickStatus::Sealing;
            tick.updated_at = crate::id::now_millis();

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

pub(super) fn handle_validator_validate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_validator_validate()");
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
                let plans = stores.read_plans()?;
                let plan = match plans.get(&target_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &target_id)));
                    }
                };
                drop(plans);
                validator.validate_plan(&target_id, &plan.title, &plan.description, &plan.acceptance_criteria)
            }
            "spec" | "specs" => {
                let specs = stores.read_specs()?;
                let spec = match specs.get(&target_id) {
                    Some(s) => s.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &target_id)));
                    }
                };
                drop(specs);
                // Get parent plan title for context
                let plan_title = stores
                    .read_plans()?
                    .get(&spec.plan_id)
                    .map(|p| p.title.clone())
                    .unwrap_or_default();
                validator.validate_spec(&target_id, &spec.title, &spec.description, &plan_title)
            }
            "phase" | "phases" => {
                let phases = stores.read_phases()?;
                let phase = match phases.get(&target_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &target_id)));
                    }
                };
                drop(phases);
                // Get parent spec title for context
                let spec_title = stores
                    .read_specs()?
                    .get(&phase.spec_id)
                    .map(|s| s.title.clone())
                    .unwrap_or_default();
                validator.validate_phase(&target_id, &phase.title, &phase.description, phase.order, &spec_title)
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

pub(super) fn handle_coverage_evaluate(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_coverage_evaluate()");
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
                let plans = stores.read_plans()?;
                let plan = match plans.get(&parent_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("plan", &parent_id)));
                    }
                };
                drop(plans);
                // Gather all Spec children of this Plan
                let specs = stores.read_specs()?;
                let child_specs: Vec<_> = specs.values().filter(|s| s.plan_id == parent_id).collect();
                let children_ids: Vec<String> = child_specs.iter().map(|s| s.id.clone()).collect();
                let specs_list = child_specs
                    .iter()
                    .map(|s| format!("- [{}] {}: {}", s.id, s.title, s.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(specs);
                evaluator.evaluate_plan_specs(
                    &parent_id,
                    &plan.title,
                    &plan.description,
                    &plan.acceptance_criteria,
                    &specs_list,
                    children_ids,
                )
            }
            "spec" | "specs" => {
                let specs = stores.read_specs()?;
                let spec = match specs.get(&parent_id) {
                    Some(s) => s.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("spec", &parent_id)));
                    }
                };
                drop(specs);
                let plan_title = {
                    let plans = stores.read_plans()?;
                    plans.get(&spec.plan_id).map(|p| p.title.clone()).unwrap_or_default()
                };
                let phases = stores.read_phases()?;
                let child_phases: Vec<_> = phases.values().filter(|p| p.spec_id == parent_id).collect();
                let children_ids: Vec<String> = child_phases.iter().map(|p| p.id.clone()).collect();
                let phases_list = child_phases
                    .iter()
                    .map(|p| format!("- [{}] {} (order: {}): {}", p.id, p.title, p.order, p.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(phases);
                evaluator.evaluate_spec_phases(
                    &parent_id,
                    &spec.title,
                    &spec.description,
                    &plan_title,
                    &phases_list,
                    children_ids,
                )
            }
            "phase" | "phases" => {
                let phases = stores.read_phases()?;
                let phase = match phases.get(&parent_id) {
                    Some(p) => p.clone(),
                    None => {
                        return Ok(DaemonResponse::err(req.id, RpcError::not_found("phase", &parent_id)));
                    }
                };
                drop(phases);
                let spec_title = {
                    let specs = stores.read_specs()?;
                    specs.get(&phase.spec_id).map(|s| s.title.clone()).unwrap_or_default()
                };
                let works = stores.read_works()?;
                let child_works: Vec<_> = works.values().filter(|w| w.phase_id == parent_id).collect();
                let children_ids: Vec<String> = child_works.iter().map(|w| w.id.clone()).collect();
                let works_list = child_works
                    .iter()
                    .map(|w| format!("- [{}] {}: {}", w.id, w.title, w.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                drop(works);
                let params = crate::evaluator::PhaseWorksParams {
                    id: parent_id.clone(),
                    title: phase.title,
                    description: phase.description,
                    order: phase.order,
                    spec_title,
                };
                evaluator.evaluate_phase_works(&params, &works_list, children_ids)
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
        let tool_runner = &stores.tool_runner;
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
                    })
                })
            })
            .collect();
        Ok(DaemonResponse::ok(req.id, json!({ "tools": tools })))
    })
}
