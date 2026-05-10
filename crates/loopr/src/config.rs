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

use std::time::Duration;

use agents::AgentsConfig;
use llm::LlmConfig;
use tools::ToolsConfig;
use worktree::WorktreeConfig;

use crate::error::LooprError;

/// Relative path (from target root) to the loopr config file.
const CONFIG_SUBPATH: &str = ".loopr/config.yml";

/// YAML-facing integrator knobs. Only the user-configurable fields are
/// exposed here; `IntegratorConfig::git_timeout` stays internal (not
/// user-tunable). Converted to `integrator::IntegratorConfig` in
/// `build_context`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct IntegratorSection {
    /// Shell commands run after a successful git merge, before Tick
    /// persistence. Each string is passed to `sh -c`. First non-zero
    /// exit rolls back the merge and returns ValidationFailed.
    /// Default: `[]` (skip validation entirely).
    pub validation_commands: Vec<String>,

    /// Wall-clock cap in seconds for each individual validation command.
    /// Default: 300.
    pub validation_timeout_secs: u64,
}

impl Default for IntegratorSection {
    fn default() -> Self {
        Self {
            validation_commands: vec![],
            validation_timeout_secs: 300,
        }
    }
}

impl IntegratorSection {
    pub fn into_integrator_config(self) -> integrator::IntegratorConfig {
        integrator::IntegratorConfig {
            validation_commands: self.validation_commands,
            validation_timeout: Duration::from_secs(self.validation_timeout_secs),
            ..integrator::IntegratorConfig::default()
        }
    }
}

/// IPC transport timeouts. Bounds every place where the daemon, its
/// per-connection handlers, or a short-lived client could otherwise wait
/// forever on a peer or on disk. See
/// `docs/design/2026-05-09-ipc-timeouts.md` for the full rationale.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct TransportSection {
    /// Wall-clock cap on `IpcClient::request_impl` (initial send + response
    /// loop). Catches a daemon that accepted the connection but is now
    /// hung. Default: 10s.
    pub client_request_secs: u64,

    /// Wall-clock cap on read silence inside `handle_client`. The pinned
    /// `Sleep` driving this is reset only when `framed.next()` yields real
    /// client traffic; broadcast events do NOT reset it. Default: 15s.
    pub server_idle_secs: u64,

    /// Wall-clock cap on each `framed.send(...).await` inside
    /// `handle_client` (both response-write and event-broadcast-write
    /// paths). Bounds the SIGSTOPped-client / full-send-buffer hang.
    /// Default: 10s.
    pub server_write_secs: u64,

    /// Wall-clock cap on `build_context` (Store::open + startup::reconcile +
    /// excludes install). Beyond this, the grandchild exits with
    /// `LooprError::DaemonStartup` rather than orphaning. Default: 60s.
    pub daemon_startup_secs: u64,
}

impl Default for TransportSection {
    fn default() -> Self {
        Self {
            client_request_secs: 10,
            server_idle_secs: 15,
            server_write_secs: 10,
            daemon_startup_secs: 60,
        }
    }
}

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
    /// Integrator knobs. Validation commands and timeout are the
    /// user-facing surface; git_timeout stays internal.
    #[serde(default)]
    pub integrator: IntegratorSection,
    /// IPC transport timeouts. Bounds dead-daemon, zombie-connection,
    /// and stuck-startup hangs.
    #[serde(default)]
    pub transport: TransportSection,
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
