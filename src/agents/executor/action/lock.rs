use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle AcquireLock action.
pub(super) fn handle_acquire_lock(ctx: &AgentContext, resource: &str, holder_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    // Check if there's already an active lock on this resource
    let check_resp = bridge.request(
        "lock.list",
        serde_json::json!({ "resource": resource, "active_only": true }),
    );
    if check_resp.is_error() {
        return Err(eyre!("lock.list failed: {:?}", check_resp.error));
    }
    if let Some(result) = &check_resp.result
        && let Some(locks) = result.as_array()
        && !locks.is_empty()
    {
        // Resource already locked - return as ActionError so the LLM can self-correct
        let existing_holder = locks[0]
            .get("holder_id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .unwrap_or("unknown");
        let existing_id = locks[0]
            .get("id")
            .and_then(|v: &serde_json::Value| v.as_str())
            .unwrap_or("unknown");
        return Ok(ActionResult::ActionError(format!(
            "resource '{}' already locked by {} (lock_id: {})",
            resource, existing_holder, existing_id
        )));
    }

    // Create the lock - granted_by is the holder_id (self-granted by coordinator)
    let resp = bridge.request(
        "lock.create",
        serde_json::json!({
            "resource": resource,
            "holder_id": holder_id,
            "granted_by": holder_id,
        }),
    );
    if resp.is_error() {
        return Err(eyre!("lock.create failed: {:?}", resp.error));
    }
    let lock_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    agent_log.info(&format!(
        "Lock acquired: {} on '{}' for {}",
        lock_id, resource, holder_id
    ));
    Ok(ActionResult::LockAcquired(lock_id))
}

/// Handle ReleaseLock action.
pub(super) fn handle_release_lock(ctx: &AgentContext, lock_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let agent_log = &ctx.log;

    let resp = bridge.request("lock.release", serde_json::json!({ "id": lock_id }));
    if resp.is_error() {
        return Err(eyre!("lock.release failed: {:?}", resp.error));
    }
    agent_log.info(&format!("Lock released: {}", lock_id));
    Ok(ActionResult::LockReleased(lock_id.to_string()))
}
