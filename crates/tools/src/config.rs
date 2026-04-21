use serde::{Deserialize, Serialize};

use crate::sandbox::SandboxMode;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ToolsConfig {
    pub sandbox: SandboxMode,
    pub path_deny_patterns: Vec<String>,
    pub bash_denylist_extend: Vec<DenyEntryConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DenyEntryConfig {
    pub tokens: Vec<String>,
    pub reason: String,
}

#[cfg(test)]
mod tests;
