use serde::{Deserialize, Serialize};
use strum::{Display, EnumString, IntoStaticStr};

use crate::envelope::DaemonRequest;
use crate::error::RpcError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, IntoStaticStr)]
pub enum MethodName {
    #[strum(serialize = "system.handshake")]
    SystemHandshake,
    #[strum(serialize = "system.status")]
    SystemStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Method {
    Handshake(HandshakeParams),
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeParams {
    pub protocol_version: u32,
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
