use std::sync::Arc;

use eyre::eyre;
use tokio::sync::broadcast;
use tracing::{debug, instrument};

use crate::domain::bundle::{Bundle, BundleStatus};
use crate::domain::role::Role;
use crate::domain::tick::{Tick, TickStatus};
use crate::domain::transition::Transition;
use crate::domain::work::WorkStatus;
use crate::fsm::runtime::FsmInterpreter;
use crate::fsm::status::FsmStatus;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};

use taskstore::{Filter, FilterOp, IndexValue};

use crate::daemon::context::Stores;

use super::{parse_optional_param, parse_required_param};

/// Find the latest Published Tick (by highest tick number).
pub(super) fn find_latest_published_tick(stores: &Arc<Stores>) -> Option<Tick> {
    let ticks = stores.read_ticks().ok()?;
    ticks
        .values()
        .filter(|t| t.status() == TickStatus::Published)
        .max_by_key(|t| t.number)
        .cloned()
}

#[instrument(skip_all, fields(work_id = ?req.params.get("work_id")))]
pub(super) fn handle_bundle_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let work_id = match req.params.get("work_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::invalid_params("work_id is required"),
                ));
            }
        };

        // Verify parent work exists and is not in a terminal state
        {
            let works = stores.read_works()?;
            match works.get(&work_id) {
                None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("work", &work_id))),
                Some(work)
                    if matches!(
                        work.status(),
                        WorkStatus::Done | WorkStatus::Superseded | WorkStatus::Abandoned
                    ) =>
                {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Cannot create bundle under {} work '{}'",
                            work.status(),
                            work_id
                        )),
                    ));
                }
                _ => {}
            }
        }

        let noop_reason = req
            .params
            .get("noop_reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let branch_name = req
            .params
            .get("branch_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // branch_name is required for normal bundles but optional for noop bundles
        if branch_name.is_empty() && noop_reason.is_none() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("branch_name is required"),
            ));
        }

        let base_tick_id = req
            .params
            .get("base_tick_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Staleness guard: reject if base_tick_id is behind the latest Published Tick
        let latest_published = find_latest_published_tick(stores);
        match (&base_tick_id, &latest_published) {
            // Published tick exists but bundle has no base_tick_id
            (None, Some(latest)) => {
                let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_id, "(none)", &latest.id));
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::stale_bundle("(none)", &latest.id),
                ));
            }
            // Published tick exists and bundle's base_tick_id doesn't match it
            (Some(base_id), Some(latest)) if base_id != &latest.id => {
                let _ = event_tx.send(DaemonEvent::bundle_rejected_stale(&work_id, base_id, &latest.id));
                return Ok(DaemonResponse::err(req.id, RpcError::stale_bundle(base_id, &latest.id)));
            }
            // No published tick and no base_tick_id: bootstrap case, OK
            // base_tick_id matches latest published: OK
            _ => {}
        }

        // M1: Parse claims as array (backward-compat: also accepts string)
        let claims: Vec<String> = match req.params.get("claims") {
            Some(serde_json::Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            Some(serde_json::Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![s.clone()]
                }
            }
            _ => Vec::new(),
        };

        let description = req
            .params
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let head_commit = req
            .params
            .get("head_commit")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut bundle = Bundle::new(work_id.clone(), base_tick_id, branch_name, claims);
        bundle.description = description;
        bundle.noop_reason = noop_reason;
        bundle.head_commit = head_commit;

        // M8: Accept both "paths" and "files_changed" (normalize param name)
        let paths_val = req.params.get("paths").or_else(|| req.params.get("files_changed"));
        if let Some(files) = paths_val.and_then(|v| v.as_array()) {
            bundle.paths = files.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }

        // Parse loc_changed if provided
        if let Some(loc) = req.params.get("loc_changed").and_then(|v| v.as_u64()) {
            bundle.loc_changed = Some(loc as u32);
        }

        // Gap #22: BundleSizePolicy enforcement on create
        let policy = &stores.config.strategy.bundle_size;
        if !bundle.paths.is_empty() && bundle.paths.len() as u32 > policy.max_files_touched {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed(&format!(
                    "Bundle touches {} files, exceeds max_files_touched={}",
                    bundle.paths.len(),
                    policy.max_files_touched
                )),
            ));
        }
        if let Some(loc) = bundle.loc_changed
            && loc > policy.max_loc_changed
        {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed(&format!(
                    "Bundle changes {} lines, exceeds max_loc_changed={}",
                    loc, policy.max_loc_changed
                )),
            ));
        }

        // Dispute detection: if this work has a prior rejection whose verification text
        // overlaps with this bundle's claims on structural keywords, mark as disputed.
        // Disputed bundles are routed to an arbitrator instead of the normal reviewer queue.
        {
            const DISPUTE_MARKERS: &[&str] = &["signature", "interface contract", "function contract", "has signature"];
            if let Ok(prior_bundles) = stores.read_bundles() {
                let has_structural_rejection = prior_bundles
                    .values()
                    .filter(|b| b.work_id == work_id && b.status() == BundleStatus::Rejected)
                    .any(|b| {
                        let text = b.verification.to_lowercase();
                        DISPUTE_MARKERS.iter().any(|m| text.contains(m))
                    });
                if has_structural_rejection {
                    let claims_lower = bundle.claims.join(" ").to_lowercase();
                    if DISPUTE_MARKERS.iter().any(|m| claims_lower.contains(m)) {
                        bundle.disputed = true;
                        tracing::info!(
                            "bundle {} marked disputed: work {} has structural rejection with keyword overlap",
                            bundle.id,
                            work_id
                        );
                    }
                }
            }
        }

        let bundle_json = match serde_json::to_value(&bundle) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        let id = bundle.id.clone();

        // Persist to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .create(bundle.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        stores.write_bundles()?.insert(id.clone(), bundle);
        let _ = event_tx.send(DaemonEvent::record_created("bundle", &id));

        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_bundle_get(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .get::<Bundle>(id)
            {
                Ok(Some(bundle)) => {
                    return match serde_json::to_value(&bundle) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Ok(None) => {}
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let bundles = stores.read_bundles()?;
        match bundles.get(id) {
            Some(bundle) => match serde_json::to_value(bundle) {
                Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
            },
            None => Ok(DaemonResponse::err(req.id, RpcError::not_found("bundle", id))),
        }
    })
}

#[instrument(skip_all, fields(work_id = ?req.params.get("work_id")))]
pub(super) fn handle_bundle_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let wi_filter = req.params.get("work_id").and_then(|v| v.as_str());

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(wid) = wi_filter {
                vec![Filter {
                    field: "work_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(wid.to_string()),
                }]
            } else {
                vec![]
            };
            match store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .list::<Bundle>(&filters)
            {
                Ok(bundles) => {
                    return match serde_json::to_value(&bundles) {
                        Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
                        Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
                    };
                }
                Err(e) => {
                    return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
                }
            }
        }

        let bundles = stores.read_bundles()?;
        let bundle_list: Vec<&Bundle> = bundles
            .values()
            .filter(|b| wi_filter.is_none() || Some(b.work_id.as_str()) == wi_filter)
            .collect();

        match serde_json::to_value(&bundle_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id"), target_status = ?req.params.get("target_status")))]
pub(super) fn handle_bundle_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    fsm: &FsmInterpreter,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let target_status: BundleStatus = match parse_required_param(&req, "target_status") {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let role: Role = match parse_optional_param(&req, "role", Role::Coordinator) {
            Ok(v) => v,
            Err(resp) => return Ok(resp),
        };

        let mut bundles = stores.write_bundles()?;

        // Read bundle info first for validation
        let (from, bundle_wi_id, paths, mut verification) = match bundles.get(&id) {
            Some(b) => (b.status(), b.work_id.clone(), b.paths.clone(), b.verification.clone()),
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundle", &id))),
        };

        // Allow setting verification during transition (e.g., Reviewer sets it when transitioning to Reviewed)
        if let Some(v) = req.params.get("verification").and_then(|v| v.as_str()) {
            verification = v.to_string();
        }

        let role_str = role.to_string();
        match fsm.validate_transition(
            BundleStatus::fsm_name(),
            from.to_yaml_name(),
            target_status.to_yaml_name(),
            &role_str,
        ) {
            Err(e) => {
                let _ = event_tx.send(DaemonEvent::transition_rejected(
                    "bundles",
                    &id,
                    &format!("{:?}", from),
                    &format!("{:?}", target_status),
                    &role_str,
                    &e.to_string(),
                ));
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::transition_rejected(&e.to_string()),
                ));
            }
            Ok(Transition::Unchanged) => {
                return Ok(DaemonResponse::ok(req.id, serde_json::Value::Null));
            }
            Ok(Transition::Changed) => {}
        }

        // #18: At most one Accepted Bundle per Work
        if target_status == BundleStatus::Accepted {
            let has_accepted = bundles
                .values()
                .any(|b| b.work_id == bundle_wi_id && b.id != id && b.status() == BundleStatus::Accepted);
            if has_accepted {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed("Work already has an Accepted Bundle"),
                ));
            }
        }

        // Gap #17: Bundle cannot touch locked resources it doesn't own
        if target_status == BundleStatus::Integrating {
            let locks = stores.read_locks()?;
            for path in &paths {
                if let Some(lock) = locks.values().find(|l| l.resource == *path && l.is_active())
                    && lock.holder_id != bundle_wi_id
                {
                    return Ok(DaemonResponse::err(
                        req.id,
                        RpcError::precondition_failed(&format!(
                            "Bundle touches locked resource '{}' owned by '{}'",
                            path, lock.holder_id
                        )),
                    ));
                }
            }
        }

        // Gap #18: Verification metadata required for Reviewed+
        if matches!(
            target_status,
            BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged
        ) && !matches!(
            from,
            BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating
        ) && verification.is_empty()
        {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::precondition_failed("Bundle must have verification metadata before Reviewed+"),
            ));
        }

        // Now get mutable reference and apply the transition
        let bundle = bundles.get_mut(&id).ok_or_else(|| eyre!("record not found: {id}"))?;
        bundle.force_status(target_status);
        bundle.updated_at = crate::id::now_millis();
        // Apply verification from transition params if provided
        if !verification.is_empty() && bundle.verification.is_empty() {
            bundle.verification = verification;
        }
        let bundle_clone = bundle.clone();
        drop(bundles);

        // Persist transition to TaskStore if available
        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(bundle_clone.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let bundle_json = match serde_json::to_value(&bundle_clone) {
            Ok(v) => v,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        };

        debug!(
            "[transition] bundle.{}: {:?} -> {:?} by {}",
            id, from, target_status, role
        );
        let _ = event_tx.send(DaemonEvent::transition_completed(
            "bundle",
            &id,
            &from.to_string(),
            &target_status.to_string(),
            &role.to_string(),
        ));

        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id")))]
pub(super) fn handle_bundle_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let id = match req.params.get("id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params("id is required"))),
        };

        let mut bundles = stores.write_bundles()?;
        let bundle = match bundles.get_mut(&id) {
            Some(b) => b,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundles", &id))),
        };

        if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
            bundle.description = Some(desc.to_string());
        }
        // M8: Accept both "paths" and "files_changed" in update
        let paths_val = req.params.get("paths").or_else(|| req.params.get("files_changed"));
        if let Some(paths) = paths_val.and_then(|v| v.as_array()) {
            // Gap #22: BundleSizePolicy enforcement on update
            let policy = &stores.config.strategy.bundle_size;
            if paths.len() as u32 > policy.max_files_touched {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Bundle touches {} files, exceeds max_files_touched={}",
                        paths.len(),
                        policy.max_files_touched
                    )),
                ));
            }
            bundle.paths = paths.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        // Parse loc_changed if provided
        if let Some(loc) = req.params.get("loc_changed").and_then(|v| v.as_u64()) {
            let policy = &stores.config.strategy.bundle_size;
            if loc as u32 > policy.max_loc_changed {
                return Ok(DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Bundle changes {} lines, exceeds max_loc_changed={}",
                        loc, policy.max_loc_changed
                    )),
                ));
            }
            bundle.loc_changed = Some(loc as u32);
        }
        // M1: Parse claims as array (backward-compat: also accepts string)
        if let Some(claims_val) = req.params.get("claims") {
            bundle.claims = match claims_val {
                serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                serde_json::Value::String(s) => {
                    if s.is_empty() {
                        Vec::new()
                    } else {
                        vec![s.clone()]
                    }
                }
                _ => Vec::new(),
            };
        }
        if let Some(verification) = req.params.get("verification").and_then(|v| v.as_str()) {
            bundle.verification = verification.to_string();
        }
        if let Some(locks) = req.params.get("locks_used").and_then(|v| v.as_array()) {
            bundle.locks_used = locks.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
        if let Some(base_tick_id) = req.params.get("base_tick_id").and_then(|v| v.as_str()) {
            bundle.base_tick_id = Some(base_tick_id.to_string());
        }
        bundle.updated_at = crate::id::now_millis();

        if let Some(store) = &stores.store
            && let Err(e) = store
                .lock()
                .map_err(|_| eyre!("taskstore lock poisoned"))?
                .update(bundle.clone())
        {
            return Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string())));
        }

        let bundle_json = serde_json::to_value(&*bundle)?;
        let _ = event_tx.send(DaemonEvent::record_updated("bundles", &id));
        Ok(DaemonResponse::ok(req.id, bundle_json))
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
