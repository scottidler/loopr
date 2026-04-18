use std::sync::Arc;

use eyre::eyre;
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct BundleCreateParams {
    work_id: String,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    noop_reason: Option<String>,
    #[serde(default)]
    base_tick_id: Option<String>,
    #[serde(default)]
    claims: Option<serde_json::Value>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    head_commit: Option<String>,
    /// Modern key for changed paths.
    #[serde(default)]
    paths: Option<Vec<String>>,
    /// Backward-compatible alias for `paths`.
    #[serde(default)]
    files_changed: Option<Vec<String>>,
    #[serde(default)]
    loc_changed: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct BundleIdParams {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct BundleListParams {
    #[serde(default)]
    work_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct BundleTransitionParams {
    id: String,
    target_status: BundleStatus,
    #[serde(default)]
    role: Option<Role>,
    #[serde(default)]
    verification: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct BundleUpdateParams {
    id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    paths: Option<Vec<String>>,
    #[serde(default)]
    files_changed: Option<Vec<String>>,
    #[serde(default)]
    loc_changed: Option<u64>,
    #[serde(default)]
    claims: Option<serde_json::Value>,
    #[serde(default)]
    verification: Option<String>,
    #[serde(default)]
    locks_used: Option<Vec<String>>,
    #[serde(default)]
    base_tick_id: Option<String>,
}

/// Find the latest Published Tick (by highest tick number).
pub(super) fn find_latest_published_tick(stores: &Arc<Stores>) -> Option<Tick> {
    let ticks = stores.read_ticks().ok()?;
    ticks
        .values()
        .filter(|t| t.status() == TickStatus::Published)
        .max_by_key(|t| t.number)
        .cloned()
}

#[instrument(skip_all, fields(work_id = ?req.params.get("work-id")))]
pub(super) fn handle_bundle_create(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let parsed: BundleCreateParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        let work_id = parsed.work_id;

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

        let noop_reason = parsed.noop_reason;
        let branch_name = parsed.branch_name.unwrap_or_default();

        // branch_name is required for normal bundles but optional for noop bundles
        if branch_name.is_empty() && noop_reason.is_none() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("branch-name is required"),
            ));
        }

        let base_tick_id = parsed.base_tick_id;

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
        let claims: Vec<String> = match parsed.claims {
            Some(serde_json::Value::Array(arr)) => {
                arr.into_iter().filter_map(|v| v.as_str().map(String::from)).collect()
            }
            Some(serde_json::Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![s]
                }
            }
            _ => Vec::new(),
        };

        let mut bundle = Bundle::new(work_id.clone(), base_tick_id, branch_name, claims);
        bundle.description = parsed.description;
        bundle.noop_reason = noop_reason;
        bundle.head_commit = parsed.head_commit;

        // M8: Accept both "paths" and "files-changed" (normalize param name)
        if let Some(files) = parsed.paths.or(parsed.files_changed) {
            bundle.paths = files;
        }

        // Parse loc_changed if provided
        if let Some(loc) = parsed.loc_changed {
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
        let parsed: BundleIdParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        let id = parsed.id.as_str();

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

#[instrument(skip_all, fields(work_id = ?req.params.get("work-id")))]
pub(super) fn handle_bundle_list(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    try_handler!(req.id, {
        let raw = if req.params.is_null() { serde_json::json!({}) } else { req.params.clone() };
        let parsed: BundleListParams = match serde_json::from_value(raw) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        let wi_filter = parsed.work_id;

        // Try TaskStore first, fall back to HashMap
        if let Some(store) = &stores.store {
            let filters: Vec<Filter> = if let Some(wid) = &wi_filter {
                vec![Filter {
                    field: "work_id".to_string(),
                    op: FilterOp::Eq,
                    value: IndexValue::String(wid.clone()),
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
            .filter(|b| wi_filter.as_deref().is_none_or(|wid| b.work_id.as_str() == wid))
            .collect();

        match serde_json::to_value(&bundle_list) {
            Ok(v) => Ok(DaemonResponse::ok(req.id, v)),
            Err(e) => Ok(DaemonResponse::err(req.id, RpcError::internal(&e.to_string()))),
        }
    })
}

#[instrument(skip_all, fields(id = ?req.params.get("id"), target_status = ?req.params.get("target-status")))]
pub(super) fn handle_bundle_transition(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    fsm: &FsmInterpreter,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let parsed: BundleTransitionParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        let id = parsed.id;
        let target_status = parsed.target_status;
        let role = parsed.role.unwrap_or(Role::Coordinator);

        let mut bundles = stores.write_bundles()?;

        // Read bundle info first for validation
        let (from, bundle_wi_id, paths, mut verification) = match bundles.get(&id) {
            Some(b) => (b.status(), b.work_id.clone(), b.paths.clone(), b.verification.clone()),
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundle", &id))),
        };

        // Allow setting verification during transition (e.g., Reviewer sets it when transitioning to Reviewed)
        if let Some(v) = parsed.verification {
            verification = v;
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
        let parsed: BundleUpdateParams = match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(DaemonResponse::err(req.id, RpcError::invalid_params(&e.to_string()))),
        };
        let id = parsed.id;

        let mut bundles = stores.write_bundles()?;
        let bundle = match bundles.get_mut(&id) {
            Some(b) => b,
            None => return Ok(DaemonResponse::err(req.id, RpcError::not_found("bundles", &id))),
        };

        if let Some(desc) = parsed.description {
            bundle.description = Some(desc);
        }
        // M8: Accept both "paths" and "files-changed" in update
        if let Some(paths) = parsed.paths.or(parsed.files_changed) {
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
            bundle.paths = paths;
        }
        // Parse loc_changed if provided
        if let Some(loc) = parsed.loc_changed {
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
        if let Some(claims_val) = parsed.claims {
            bundle.claims = match claims_val {
                serde_json::Value::Array(arr) => arr.into_iter().filter_map(|v| v.as_str().map(String::from)).collect(),
                serde_json::Value::String(s) => {
                    if s.is_empty() {
                        Vec::new()
                    } else {
                        vec![s]
                    }
                }
                _ => Vec::new(),
            };
        }
        if let Some(verification) = parsed.verification {
            bundle.verification = verification;
        }
        if let Some(locks) = parsed.locks_used {
            bundle.locks_used = locks;
        }
        if let Some(base_tick_id) = parsed.base_tick_id {
            bundle.base_tick_id = Some(base_tick_id);
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
