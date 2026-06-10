use serde::{Deserialize, Serialize};

use crate::sandbox::SandboxMode;

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ToolsConfig {
    pub sandbox: SandboxMode,
    pub path_deny_patterns: Vec<String>,
    pub bash_denylist_extend: Vec<DenyEntryConfig>,
    /// Per-lane tighten-only overrides (Phase-5 finding 13). A target's
    /// `.loopr/config.yml` may REDUCE slots / timeouts below the built-in
    /// defaults; values are clamped at the defaults so a target can never
    /// widen them (vision's "tighten-only" rule).
    pub lane_overrides: LaneOverrides,
}

/// Optional per-lane tighten-only knobs. Every field is `None` by default
/// (use the built-in `LanePolicy` const); a present value is clamped to be
/// no looser than the default.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct LaneOverrides {
    pub local: LaneTighten,
    pub net: LaneTighten,
    pub heavy: LaneTighten,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct LaneTighten {
    pub slots: Option<usize>,
    pub default_timeout_secs: Option<u64>,
    pub max_timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DenyEntryConfig {
    pub tokens: Vec<String>,
    pub reason: String,
}

#[cfg(test)]
mod tests;
