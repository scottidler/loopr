use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use domain::Plan;

use crate::envelope::DaemonRequest;
use crate::error::RpcError;
use crate::records::{RecordGetParams, RecordListParams};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, IntoStaticStr)]
pub enum MethodName {
    #[strum(serialize = "system.handshake")]
    SystemHandshake,
    #[strum(serialize = "system.status")]
    SystemStatus,
    #[strum(serialize = "plan.create")]
    PlanCreate,
    #[strum(serialize = "record.list")]
    RecordList,
    #[strum(serialize = "record.get")]
    RecordGet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Handshake(HandshakeParams),
    Status,
    PlanCreate(PlanCreateParams),
    RecordList(RecordListParams),
    RecordGet(RecordGetParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeParams {
    pub protocol_version: u32,
    /// Client's resolved session-id. Additive field: older clients may
    /// omit it, in which case the daemon records the connection under
    /// its own daemon-boot session-id. Newer daemons treat `None` as
    /// equivalent to daemon-boot-session attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCreateParams {
    pub goal: String,
}

impl TryFrom<&DaemonRequest> for Method {
    type Error = RpcError;
    fn try_from(req: &DaemonRequest) -> Result<Self, Self::Error> {
        use std::str::FromStr;
        let name = MethodName::from_str(&req.method).map_err(|_| RpcError::MethodNotFound(req.method.clone()))?;
        match name {
            MethodName::SystemHandshake => {
                let params: HandshakeParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::Handshake(params))
            }
            MethodName::SystemStatus => {
                if !req.params.is_null() && !matches!(&req.params, serde_json::Value::Object(m) if m.is_empty()) {
                    return Err(RpcError::InvalidParams("system.status takes no params".into()));
                }
                Ok(Method::Status)
            }
            MethodName::PlanCreate => {
                let params: PlanCreateParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::PlanCreate(params))
            }
            MethodName::RecordList => {
                let params: RecordListParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::RecordList(params))
            }
            MethodName::RecordGet => {
                let params: RecordGetParams =
                    serde_json::from_value(req.params.clone()).map_err(|e| RpcError::InvalidParams(e.to_string()))?;
                Ok(Method::RecordGet(params))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeResult {
    pub protocol_version: u32,
    pub daemon_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResult {
    pub started_at: String,
    pub pid: u32,
    pub active_plans: u32,
    pub active_works: u32,
}

/// Success payload for `plan.create`: the newly persisted Plan record.
/// `Plan` does not implement `PartialEq`/`Eq` (`created_at`/`updated_at`
/// would make equality slippery), so neither does this wrapper. Wire
/// round-trip is asserted by encoding a known-good JSON string and
/// comparing byte stability in the seam tests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCreateResult {
    pub plan: Plan,
}
