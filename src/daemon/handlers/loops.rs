//! Loop request handlers
//!
//! Handles loop.* IPC methods by delegating to LoopManager.

use serde_json::{Value, json};

// Storage trait needed for delete method
#[allow(unused_imports)]
use crate::storage::Storage;

use crate::daemon::context::DaemonContext;
use crate::domain::LoopType;
use crate::ipc::messages::{DaemonError, DaemonEvent, DaemonResponse};

/// Handle loop.list - list all loops
pub async fn handle_loop_list(id: u64, ctx: &DaemonContext) -> DaemonResponse {
    let manager = ctx.loop_manager.read().await;
    match manager.list_loops().await {
        Ok(loops) => {
            let loops_json: Vec<Value> = loops.iter().filter_map(|l| serde_json::to_value(l).ok()).collect();
            DaemonResponse::success(id, json!({"loops": loops_json}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.get - get a single loop by ID
pub async fn handle_loop_get(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.get_loop(loop_id).await {
        Ok(Some(loop_record)) => match serde_json::to_value(&loop_record) {
            Ok(value) => DaemonResponse::success(id, json!({"loop": value})),
            Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
        },
        Ok(None) => DaemonResponse::error(id, DaemonError::loop_not_found(loop_id)),
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.create_plan - create a new plan loop
pub async fn handle_loop_create_plan(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let description = match params["description"].as_str() {
        Some(d) => d,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'description' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.create_loop(LoopType::Plan, description).await {
        Ok(loop_record) => {
            // Broadcast event
            ctx.broadcast(DaemonEvent::loop_created(&loop_record));

            DaemonResponse::success(
                id,
                json!({
                    "id": loop_record.id,
                    "status": "created"
                }),
            )
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.start - start executing a loop
pub async fn handle_loop_start(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.start_loop(loop_id).await {
        Ok(()) => {
            // Broadcast update event
            if let Ok(Some(loop_record)) = manager.get_loop(loop_id).await {
                ctx.broadcast(DaemonEvent::loop_updated(&loop_record));
            }
            DaemonResponse::success(id, json!({"started": true}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.pause - pause a running loop
pub async fn handle_loop_pause(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.pause_loop(loop_id).await {
        Ok(()) => {
            // Broadcast update event
            if let Ok(Some(loop_record)) = manager.get_loop(loop_id).await {
                ctx.broadcast(DaemonEvent::loop_updated(&loop_record));
            }
            DaemonResponse::success(id, json!({"paused": true}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.resume - resume a paused loop
pub async fn handle_loop_resume(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.resume_loop(loop_id).await {
        Ok(()) => {
            // Broadcast update event
            if let Ok(Some(loop_record)) = manager.get_loop(loop_id).await {
                ctx.broadcast(DaemonEvent::loop_updated(&loop_record));
            }
            DaemonResponse::success(id, json!({"resumed": true}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.cancel - stop/cancel a running loop
pub async fn handle_loop_cancel(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    let manager = ctx.loop_manager.read().await;
    match manager.stop_loop(loop_id).await {
        Ok(()) => {
            // Broadcast update event
            if let Ok(Some(loop_record)) = manager.get_loop(loop_id).await {
                ctx.broadcast(DaemonEvent::loop_updated(&loop_record));
            }
            DaemonResponse::success(id, json!({"cancelled": true}))
        }
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

/// Handle loop.delete - delete a loop from storage
pub async fn handle_loop_delete(id: u64, params: &Value, ctx: &DaemonContext) -> DaemonResponse {
    let loop_id = match params["id"].as_str() {
        Some(id) => id,
        None => return DaemonResponse::error(id, DaemonError::invalid_params("Missing 'id' parameter")),
    };

    // Delete from storage
    match ctx.storage.delete("loops", loop_id) {
        Ok(()) => DaemonResponse::success(id, json!({"deleted": true})),
        Err(e) => DaemonResponse::error(id, DaemonError::internal_error(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Integration tests for these handlers would require setting up
    // a full DaemonContext with mock components. Unit tests are limited
    // since handlers directly interact with the context.

    #[test]
    fn test_handle_loop_list_response_format() {
        // Test that we can construct the expected response format
        let loops_json: Vec<Value> = vec![];
        let response = json!({"loops": loops_json});
        assert!(response["loops"].is_array());
    }

    #[test]
    fn test_handle_loop_get_response_format() {
        // Test that we can construct the expected response format
        let response = json!({"loop": null});
        assert!(response["loop"].is_null());
    }
}
