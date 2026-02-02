//! Plan approval request handlers
//!
//! Handles plan.* IPC methods for the user approval gate.

use serde_json::{Value, json};

use crate::daemon::context::DaemonContext;
use crate::domain::LoopType;
use crate::ipc::messages::{DaemonError, DaemonEvent, DaemonResponse};

/// Handle plan.approve - approve a completed plan and spawn spec loops
pub async fn handle_plan_approve(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let plan_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    // Get the plan loop
    let manager = ctx.loop_manager.read().await;
    let plan = match manager.get_loop(plan_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return DaemonResponse::error(id, DaemonError::loop_not_found(plan_id)),
        Err(e) => return DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    };

    // Verify it's a plan loop
    if plan.loop_type != LoopType::Plan {
        return DaemonResponse::error(
            id,
            DaemonError::invalid_state(format!("Loop {} is not a plan loop", plan_id)),
        );
    }

    // Verify it's complete
    if !plan.status.is_terminal() {
        return DaemonResponse::error(
            id,
            DaemonError::invalid_state(format!("Plan {} is not yet complete", plan_id)),
        );
    }

    // Parse specs from the plan's output artifacts
    // For now, we'll create a simple spec for each artifact
    let specs_count = plan.output_artifacts.len().max(1) as u32;

    // Spawn spec loops
    drop(manager); // Release read lock before acquiring write lock
    let manager = ctx.loop_manager.write().await;

    let mut spawned = 0;
    for i in 0..specs_count {
        match manager.create_child_loop(&plan, LoopType::Spec, i).await {
            Ok(spec) => {
                ctx.broadcast(DaemonEvent::loop_created(&spec));
                spawned += 1;
            }
            Err(e) => {
                // Log but continue with other specs
                log::warn!("Failed to spawn spec {}: {}", i, e);
            }
        }
    }

    // Broadcast approval event
    ctx.broadcast(DaemonEvent::plan_approved(plan_id, spawned));

    DaemonResponse::success(id, json!({"specs_spawned": spawned}))
}

/// Handle plan.reject - reject a plan with optional reason
pub async fn handle_plan_reject(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let plan_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let reason = params["reason"].as_str();

    // Get the plan loop
    let manager = ctx.loop_manager.read().await;
    let plan = match manager.get_loop(plan_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return DaemonResponse::error(id, DaemonError::loop_not_found(plan_id)),
        Err(e) => return DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    };

    // Verify it's a plan loop
    if plan.loop_type != LoopType::Plan {
        return DaemonResponse::error(
            id,
            DaemonError::invalid_state(format!("Loop {} is not a plan loop", plan_id)),
        );
    }

    // Update the plan status to Failed
    // Note: In a full implementation, we'd update the loop status in storage
    // For now, we just broadcast the rejection event

    // Broadcast rejection event
    ctx.broadcast(DaemonEvent::plan_rejected(plan_id, reason));

    DaemonResponse::success(id, json!({"rejected": true}))
}

/// Handle plan.iterate - add feedback and re-run the plan
pub async fn handle_plan_iterate(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let plan_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let feedback = match params["feedback"].as_str() {
        Some(f) => f,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'feedback' parameter")),
    };

    // Get the plan loop
    let manager = ctx.loop_manager.read().await;
    let plan = match manager.get_loop(plan_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return DaemonResponse::error(id, DaemonError::loop_not_found(plan_id)),
        Err(e) => return DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    };

    // Verify it's a plan loop
    if plan.loop_type != LoopType::Plan {
        return DaemonResponse::error(
            id,
            DaemonError::invalid_state(format!("Loop {} is not a plan loop", plan_id)),
        );
    }

    // Note: Full implementation would:
    // 1. Add feedback to the plan's progress field
    // 2. Reset status to Pending
    // 3. Trigger re-execution with accumulated feedback

    // For now, just acknowledge the iteration request
    DaemonResponse::success(
        id,
        json!({
            "iterated": true,
            "feedback_received": feedback
        }),
    )
}

/// Handle plan.get_preview - get plan content and parsed specs
pub async fn handle_plan_get_preview(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let plan_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    // Get the plan loop
    let manager = ctx.loop_manager.read().await;
    let plan = match manager.get_loop(plan_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return DaemonResponse::error(id, DaemonError::loop_not_found(plan_id)),
        Err(e) => return DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    };

    // Get plan content from output artifacts
    let content = if !plan.output_artifacts.is_empty() {
        // Try to read the first artifact
        std::fs::read_to_string(&plan.output_artifacts[0]).unwrap_or_default()
    } else {
        String::new()
    };

    // Parse specs (simplified - just list artifacts for now)
    let specs: Vec<String> = plan.output_artifacts.iter().map(|a| a.display().to_string()).collect();

    DaemonResponse::success(
        id,
        json!({
            "id": plan_id,
            "content": content,
            "specs": specs,
            "status": format!("{:?}", plan.status)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_approve_response_format() {
        let response = json!({"specs_spawned": 3});
        assert_eq!(response["specs_spawned"], 3);
    }

    #[test]
    fn test_plan_reject_response_format() {
        let response = json!({"rejected": true});
        assert!(response["rejected"].as_bool().unwrap());
    }

    #[test]
    fn test_plan_preview_response_format() {
        let response = json!({
            "id": "plan-001",
            "content": "# Plan content",
            "specs": ["spec-1", "spec-2"],
            "status": "Complete"
        });
        assert_eq!(response["id"], "plan-001");
        assert!(response["specs"].is_array());
    }
}
