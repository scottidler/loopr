use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "RpcErrorWire", from = "RpcErrorWire")]
pub enum RpcError {
    // JSON-RPC 2.0 standard codes
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    #[error("internal error: {0}")]
    Internal(String),

    // loopr-specific codes
    #[error("transition rejected: {0}")]
    TransitionRejected(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("stale bundle: {0}")]
    StaleBundle(String),
    #[error("validation required: {0}")]
    ValidationRequired(String),
    #[error("pool exhausted: {0}")]
    PoolExhausted(String),
    #[error("precondition failed: {0}")]
    PreconditionFailed(String),
    #[error("protocol version mismatch: {0}")]
    ProtocolVersionMismatch(String),

    // Forward-compat: codes the current enum doesn't recognize
    #[error("rpc error {code}: {message}")]
    Unknown { code: i32, message: String },
}

impl RpcError {
    pub const CODE_PARSE_ERROR: i32 = -32700;
    pub const CODE_INVALID_REQUEST: i32 = -32600;
    pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
    pub const CODE_INVALID_PARAMS: i32 = -32602;
    pub const CODE_INTERNAL: i32 = -32603;

    pub const CODE_TRANSITION_REJECTED: i32 = -32000;
    pub const CODE_NOT_FOUND: i32 = -32001;
    pub const CODE_STALE_BUNDLE: i32 = -32002;
    pub const CODE_VALIDATION_REQUIRED: i32 = -32003;
    pub const CODE_POOL_EXHAUSTED: i32 = -32004;
    pub const CODE_PRECONDITION_FAILED: i32 = -32005;
    pub const CODE_PROTOCOL_VERSION_MISMATCH: i32 = -32006;

    pub fn code(&self) -> i32 {
        match self {
            RpcError::ParseError(_) => Self::CODE_PARSE_ERROR,
            RpcError::InvalidRequest(_) => Self::CODE_INVALID_REQUEST,
            RpcError::MethodNotFound(_) => Self::CODE_METHOD_NOT_FOUND,
            RpcError::InvalidParams(_) => Self::CODE_INVALID_PARAMS,
            RpcError::Internal(_) => Self::CODE_INTERNAL,
            RpcError::TransitionRejected(_) => Self::CODE_TRANSITION_REJECTED,
            RpcError::NotFound(_) => Self::CODE_NOT_FOUND,
            RpcError::StaleBundle(_) => Self::CODE_STALE_BUNDLE,
            RpcError::ValidationRequired(_) => Self::CODE_VALIDATION_REQUIRED,
            RpcError::PoolExhausted(_) => Self::CODE_POOL_EXHAUSTED,
            RpcError::PreconditionFailed(_) => Self::CODE_PRECONDITION_FAILED,
            RpcError::ProtocolVersionMismatch(_) => Self::CODE_PROTOCOL_VERSION_MISMATCH,
            RpcError::Unknown { code, .. } => *code,
        }
    }

    pub fn message(&self) -> String {
        match self {
            RpcError::ParseError(m)
            | RpcError::InvalidRequest(m)
            | RpcError::MethodNotFound(m)
            | RpcError::InvalidParams(m)
            | RpcError::Internal(m)
            | RpcError::TransitionRejected(m)
            | RpcError::NotFound(m)
            | RpcError::StaleBundle(m)
            | RpcError::ValidationRequired(m)
            | RpcError::PoolExhausted(m)
            | RpcError::PreconditionFailed(m)
            | RpcError::ProtocolVersionMismatch(m) => m.clone(),
            RpcError::Unknown { message, .. } => message.clone(),
        }
    }

    pub fn protocol_version_mismatch(client: u32, daemon: u32) -> Self {
        RpcError::ProtocolVersionMismatch(format!("client={client} daemon={daemon}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorWire {
    pub code: i32,
    pub message: String,
}

impl From<RpcError> for RpcErrorWire {
    fn from(err: RpcError) -> Self {
        RpcErrorWire {
            code: err.code(),
            message: err.message(),
        }
    }
}

impl From<RpcErrorWire> for RpcError {
    fn from(w: RpcErrorWire) -> Self {
        match w.code {
            RpcError::CODE_PARSE_ERROR => RpcError::ParseError(w.message),
            RpcError::CODE_INVALID_REQUEST => RpcError::InvalidRequest(w.message),
            RpcError::CODE_METHOD_NOT_FOUND => RpcError::MethodNotFound(w.message),
            RpcError::CODE_INVALID_PARAMS => RpcError::InvalidParams(w.message),
            RpcError::CODE_INTERNAL => RpcError::Internal(w.message),
            RpcError::CODE_TRANSITION_REJECTED => RpcError::TransitionRejected(w.message),
            RpcError::CODE_NOT_FOUND => RpcError::NotFound(w.message),
            RpcError::CODE_STALE_BUNDLE => RpcError::StaleBundle(w.message),
            RpcError::CODE_VALIDATION_REQUIRED => RpcError::ValidationRequired(w.message),
            RpcError::CODE_POOL_EXHAUSTED => RpcError::PoolExhausted(w.message),
            RpcError::CODE_PRECONDITION_FAILED => RpcError::PreconditionFailed(w.message),
            RpcError::CODE_PROTOCOL_VERSION_MISMATCH => RpcError::ProtocolVersionMismatch(w.message),
            other => RpcError::Unknown {
                code: other,
                message: w.message,
            },
        }
    }
}
