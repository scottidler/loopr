use serde::{Deserialize, Serialize};

use crate::error::RpcError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl DaemonResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, error: RpcError) -> Self {
        Self {
            id,
            result: None,
            error: Some(error),
        }
    }
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DaemonEvent {
    pub event: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IpcMessage {
    Response(DaemonResponse),
    Event(DaemonEvent),
}
