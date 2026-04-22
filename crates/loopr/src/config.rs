//! Top-level `Config` composed from each stage crate's own config. Loaded
//! from `<target>/.loopr/config.yml` if present; otherwise falls back to
//! each sub-config's `Default`.
//!
//! Stage 6 composes only `LlmConfig`; future stages add their own keys
//! (scoped under their crate name). The top-level struct owns the
//! composition so `loopr` never re-derives stage-specific defaults.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use agents::AgentsConfig;
use llm::LlmConfig;
use tools::ToolsConfig;
use worktree::WorktreeConfig;

use crate::error::LooprError;

/// Relative path (from target root) to the loopr config file.
const CONFIG_SUBPATH: &str = ".loopr/config.yml";

/// Top-level configuration composed from each stage crate's config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Config {
    pub llm: LlmConfig,
    #[serde(default, skip_serializing)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub worktree: WorktreeConfig,
    /// Per-role agent knobs: `agents.implementer.*` and
    /// `agents.reviewer.*` on disk. Stage 8 wiring capstone will
    /// thread these through the daemon so `ImplementerConfig::default()`
    /// and `ReviewerConfig::default()` call sites become
    /// `ctx.config.agents.implementer.clone()` etc.
    #[serde(default)]
    pub agents: AgentsConfig,
}

impl Config {
    /// Load from `<target>/.loopr/config.yml`. Missing file yields
    /// `Default::default()` so Stage 6's "no config, just env vars"
    /// path works out of the box. Parse errors surface as
    /// `LooprError::DaemonStartup`.
    ///
    /// After deserializing, env-var overrides are applied so operators can
    /// tweak policy without editing the config file. Precedence at this
    /// layer is **ENV > config > default**; the CLI override (`--worktree-
    /// cleanup`) is higher-precedence and applied by the caller after load.
    /// The full precedence is **CLI > ENV > config > default** per the
    /// design doc.
    pub fn load(target: &Path) -> Result<Self, LooprError> {
        let path = target.join(CONFIG_SUBPATH);
        let mut config: Self = if path.exists() {
            let body = fs::read_to_string(&path)
                .map_err(|e| LooprError::DaemonStartup(format!("read {}: {e}", path.display())))?;
            serde_yaml::from_str(&body)
                .map_err(|e| LooprError::DaemonStartup(format!("parse {}: {e}", path.display())))?
        } else {
            Self::default()
        };

        if let Ok(value) = std::env::var(WORKTREE_CLEANUP_ENV) {
            let parsed: worktree::AttemptCleanupPolicy = serde_yaml::from_str(value.trim())
                .map_err(|e| LooprError::DaemonStartup(format!("invalid {WORKTREE_CLEANUP_ENV}={value:?}: {e}")))?;
            config.worktree.cleanup_policy = parsed;
        }

        Ok(config)
    }
}

/// Environment variable that overrides the worktree cleanup policy.
pub const WORKTREE_CLEANUP_ENV: &str = "LOOPR_WORKTREE_CLEANUP_POLICY";

/// Resolve the API key for the configured LLM. Env-only in Stage 6:
/// reads the env var named by `config.llm.api_key_env`. When the env
/// var is unset, returns a placeholder string that makes
/// `AnthropicClient::new` succeed but any actual LLM call will fail
/// with 401. The daemon still starts; `plan.create` records the
/// `Plan`, and the decomposer's single retry-and-bail path logs the
/// failure without tearing down the process. This is the Stage 6
/// "graceful degradation" compromise — real users set the env var
/// and get real decomposition; CI without a key keeps booting.
pub const PLACEHOLDER_API_KEY: &str = "unset-placeholder";

pub fn resolve_api_key(llm: &LlmConfig) -> String {
    std::env::var(&llm.api_key_env).unwrap_or_else(|_| PLACEHOLDER_API_KEY.to_string())
}

#[cfg(test)]
mod tests;
