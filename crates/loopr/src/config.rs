//! Top-level `Config` composed from each stage crate's own config. Loaded
//! from `<target>/.loopr/config.yml` if present; otherwise falls back to
//! each sub-config's `Default`.
//!
//! Stage 6 composes only `LlmConfig`; future stages add their own keys
//! (scoped under their crate name). The top-level struct owns the
//! composition so `loopr` never re-derives stage-specific defaults.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use std::time::Duration;

use agents::AgentsConfig;
use llm::{LlmConfig, ModelTiers};
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

    /// Phase C: when `true` (default), the daemon creates the per-Plan
    /// branch `loopr/plan-<id>`, integrates onto it, and the operator
    /// merges to `main`. When `false`, the daemon skips branch creation
    /// and the Integrator merges directly onto the checked-out branch
    /// (refusing on a dirty tree). loopr never merges to `main` itself.
    pub integration_branch: bool,
}

impl Default for IntegratorSection {
    fn default() -> Self {
        Self {
            validation_commands: vec![],
            validation_timeout_secs: 300,
            integration_branch: true,
        }
    }
}

impl IntegratorSection {
    pub fn into_integrator_config(self) -> integrator::IntegratorConfig {
        integrator::IntegratorConfig {
            validation_commands: self.validation_commands,
            validation_timeout: Duration::from_secs(self.validation_timeout_secs),
            integration_branch: self.integration_branch,
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

/// Cost budgets (vision "Budgets"). Both caps default to `None`
/// (unlimited) per the options-with-sane-defaults rule — a fresh target
/// runs uncapped until an operator opts into a ceiling. Enforcement is
/// soft-pause only: hitting a cap stops new agent spawns and emits a
/// `budget.exceeded` event; in-flight agents finish and are never killed.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct BudgetsSection {
    /// Per-run (per-daemon-process) cumulative LLM cost cap in U.S.
    /// dollars. Checked at the daemon's spawn gates against the live
    /// `ProcessSnapshot` cost; on breach the daemon stops spawning new
    /// implementers and Directors. `None` = unlimited.
    pub per_run_cost_usd: Option<f64>,
    /// Per-Work cumulative LLM cost cap in U.S. dollars. The implementer
    /// accumulates its calls' cost across iterations; on breach it
    /// escalates the Work (the implementer's "stop this Work" signal).
    /// `None` = unlimited.
    pub per_work_cost_usd: Option<f64>,
}

/// Top-level configuration composed from each stage crate's config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct Config {
    pub llm: LlmConfig,
    /// Named model tiers (`models.primary` / `lightweight` / `advisor`).
    /// Roles reference a tier by name or supply a literal model ID;
    /// `resolve_model_tiers` rewrites every role's model reference to a
    /// concrete model ID after load, so downstream consumers never see a
    /// tier name. Swapping every role's model version is a one-line edit
    /// here (vision "Role-to-model mapping").
    #[serde(default)]
    pub models: ModelTiers,
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
    /// Decomposition knobs (`decomposer.max-children`).
    #[serde(default)]
    pub decomposer: decomposer::DecomposerConfig,
    /// Integrator knobs. Validation commands and timeout are the
    /// user-facing surface; git_timeout stays internal.
    #[serde(default)]
    pub integrator: IntegratorSection,
    /// IPC transport timeouts. Bounds dead-daemon, zombie-connection,
    /// and stuck-startup hangs.
    #[serde(default)]
    pub transport: TransportSection,
    /// Cost budgets (per-run and per-Work). Both unlimited by default.
    #[serde(default)]
    pub budgets: BudgetsSection,
}

impl Config {
    /// Load the composed config with the layered precedence
    /// **baked-in < XDG user < target < env** (CLI flags for config knobs
    /// remain future work — only `--log-level` and the worktree-cleanup
    /// override exist today, applied by their own callers/passes).
    ///
    /// - **baked-in:** every field's `Default`.
    /// - **XDG user:** `$XDG_CONFIG_HOME/loopr/loopr.yml` (or
    ///   `~/.config/loopr/loopr.yml`), via the `xdg_config_dir` helper —
    ///   NOT `dirs::config_dir()`, which ignores `$XDG_CONFIG_HOME` on macOS.
    /// - **target:** `<target>/.loopr/config.yml`.
    /// - **env:** generic `LOOPR_<SECTION>__<KEY>` overrides plus the
    ///   dedicated `LOOPR_WORKTREE_CLEANUP_POLICY`.
    ///
    /// XDG and target are deep-merged as YAML `Value`s (serde has no native
    /// deep-merge), so a key set only in the XDG layer survives a target
    /// file that omits it. Missing files at every layer yield
    /// `Default::default()`. Parse / deserialize errors surface as
    /// `LooprError::DaemonStartup`.
    pub fn load(target: &Path) -> Result<Self, LooprError> {
        let mut merged = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let mut have_layer = false;

        // XDG user layer (lowest file layer). Best-effort: the
        // `~/.config/loopr/loopr.yml` path is shared across ALL loopr
        // versions on the machine (v3, v4, v5, ...), so this version
        // cannot demand it conform to its own schema. A file that does not
        // validate as a v5 `Config` (e.g. a leftover v3/v4 config with
        // keys like `debug` / `agents.enabled`) is WARNED about and
        // skipped rather than bricking daemon startup. Files this version
        // OWNS — the target `.loopr/config.yml`, written by `loopr init` —
        // stay strict below. Read / parse / validate failures all degrade
        // to warn-and-skip here.
        if let Some(xdg_path) = xdg_config_file()
            && xdg_path.exists()
        {
            match Self::read_optional_layer(&xdg_path) {
                Ok(Some(value)) => {
                    deep_merge(&mut merged, value);
                    have_layer = true;
                }
                Ok(None) => {}
                Err(reason) => {
                    tracing::warn!(
                        path = %xdg_path.display(),
                        reason = %reason,
                        "XDG user config is not valid for this loopr version; ignoring it \
                         (it is shared across loopr versions). Remove or migrate it to silence this."
                    );
                }
            }
        }

        // Target layer (overrides XDG).
        let target_path = target.join(CONFIG_SUBPATH);
        if target_path.exists() {
            let body = fs::read_to_string(&target_path)
                .map_err(|e| LooprError::DaemonStartup(format!("read {}: {e}", target_path.display())))?;
            let value: serde_yaml::Value = serde_yaml::from_str(&body)
                .map_err(|e| LooprError::DaemonStartup(format!("parse {}: {e}", target_path.display())))?;
            deep_merge(&mut merged, value);
            have_layer = true;
        }

        // Generic env layer (overrides both files): LOOPR_<SECTION>__<KEY>.
        let env_applied = apply_env_overrides(&mut merged);

        let mut config: Self = if have_layer || env_applied {
            serde_yaml::from_value(merged)
                .map_err(|e| LooprError::DaemonStartup(format!("config deserialize: {e}")))?
        } else {
            Self::default()
        };

        // Dedicated worktree-cleanup env override (back-compat; also
        // expressible as `LOOPR_WORKTREE__CLEANUP_POLICY` via the generic
        // pass above).
        if let Ok(value) = std::env::var(WORKTREE_CLEANUP_ENV) {
            let parsed: worktree::AttemptCleanupPolicy = serde_yaml::from_str(value.trim())
                .map_err(|e| LooprError::DaemonStartup(format!("invalid {WORKTREE_CLEANUP_ENV}={value:?}: {e}")))?;
            config.worktree.cleanup_policy = parsed;
        }

        config.resolve_model_tiers();
        Ok(config)
    }

    /// Read a best-effort config layer (the shared XDG user config) and
    /// return its YAML `Value` only if it both parses AND validates as a
    /// v5 `Config` standalone (every key known, under `deny_unknown_fields`;
    /// `#[serde(default)]` fills the absent ones, so a partial file is
    /// fine). `Ok(None)` is reserved for an empty file; `Err(reason)` means
    /// the caller should warn and skip the layer. Validating standalone
    /// keeps the later merged deserialize strict (target typos still fail)
    /// while letting a foreign/legacy XDG file be ignored instead of
    /// bricking startup.
    fn read_optional_layer(path: &Path) -> Result<Option<serde_yaml::Value>, String> {
        let body = fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
        let value: serde_yaml::Value = serde_yaml::from_str(&body).map_err(|e| format!("parse: {e}"))?;
        if value.is_null() {
            return Ok(None);
        }
        // Validate as a standalone Config (strict) before accepting it.
        serde_yaml::from_value::<Self>(value.clone()).map_err(|e| format!("not valid for this version: {e}"))?;
        Ok(Some(value))
    }

    /// Rewrite every role's model reference to a concrete model ID by
    /// resolving it against the `models:` tier table. A tier name
    /// (`primary` / `lightweight` / `advisor`) becomes that tier's model;
    /// a literal model ID is left unchanged. Run once after load so the
    /// `AnthropicClient`, the `ProcessSnapshot`, and the per-role agent
    /// configs all see concrete model IDs, never tier names.
    fn resolve_model_tiers(&mut self) {
        self.llm.model = self.models.resolve(&self.llm.model);
        self.agents.director.model = self.models.resolve(&self.agents.director.model);
    }
}

/// Environment variable that overrides the worktree cleanup policy.
pub const WORKTREE_CLEANUP_ENV: &str = "LOOPR_WORKTREE_CLEANUP_POLICY";

/// XDG config dir, honoring `$XDG_CONFIG_HOME` and falling back to
/// `$HOME/.config`. NOT `dirs::config_dir()` — that ignores
/// `$XDG_CONFIG_HOME` on macOS (returns `~/Library/Application Support`),
/// so config an operator drops in `~/.config` would be silently never
/// found (per `rules/rust.md` "Platform paths").
fn xdg_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Some(path);
        }
    }
    dirs::home_dir().map(|h| h.join(".config"))
}

/// Path to the XDG user config file: `<xdg-config>/loopr/loopr.yml`.
fn xdg_config_file() -> Option<PathBuf> {
    xdg_config_dir().map(|d| d.join("loopr").join("loopr.yml"))
}

/// Deep-merge `overlay` into `base`: mappings merge key-by-key
/// (recursively); scalars and sequences are replaced wholesale by the
/// overlay. Used to layer the target config over the XDG user config so a
/// key present only in the lower layer survives an upper layer that omits
/// it (serde_yaml has no native deep-merge).
fn deep_merge(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
    match (base, overlay) {
        (serde_yaml::Value::Mapping(b), serde_yaml::Value::Mapping(o)) => {
            for (k, ov) in o {
                match b.get_mut(&k) {
                    Some(bv) => deep_merge(bv, ov),
                    None => {
                        b.insert(k, ov);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Set `val` at the nested `path` inside `root`, creating intermediate
/// mappings as needed. Used by the generic env-override pass.
fn set_at_path(root: &mut serde_yaml::Value, path: &[String], val: serde_yaml::Value) {
    let Some((head, rest)) = path.split_first() else {
        return;
    };
    if !root.is_mapping() {
        *root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let serde_yaml::Value::Mapping(map) = root else {
        return;
    };
    let key = serde_yaml::Value::String(head.clone());
    if rest.is_empty() {
        map.insert(key, val);
        return;
    }
    if !map.get(&key).map(serde_yaml::Value::is_mapping).unwrap_or(false) {
        map.insert(key.clone(), serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    }
    if let Some(child) = map.get_mut(&key) {
        set_at_path(child, rest, val);
    }
}

/// Apply generic `LOOPR_<SECTION>__<KEY>` environment overrides onto the
/// merged config `Value`. The double-underscore `__` separates nesting
/// levels; within each segment a single `_` becomes `-` (kebab) and the
/// segment is lowercased, matching the config's serde key naming. Examples:
/// `LOOPR_LLM__MODEL` -> `llm.model`,
/// `LOOPR_BUDGETS__PER_RUN_COST_USD` -> `budgets.per-run-cost-usd`,
/// `LOOPR_TRANSPORT__CLIENT_REQUEST_SECS` -> `transport.client-request-secs`.
///
/// The `__` marker is required: `LOOPR_*` vars without it (e.g.
/// `LOOPR_TARGET`, `LOOPR_WORKTREE_CLEANUP_POLICY`, `LOOPR_LOG`) are NOT
/// config-field overrides and are skipped. Values are parsed as YAML
/// scalars (so `30` is an int, `true` a bool), falling back to a string.
/// An override naming an unknown field surfaces later as a `deny_unknown_
/// fields` deserialize error — a loud signal for a typo, not a silent drop.
/// Returns whether any override was applied.
fn apply_env_overrides(value: &mut serde_yaml::Value) -> bool {
    let mut applied = false;
    for (k, v) in std::env::vars() {
        let Some(rest) = k.strip_prefix("LOOPR_") else {
            continue;
        };
        if !rest.contains("__") {
            continue;
        }
        let path: Vec<String> = rest
            .split("__")
            .map(|seg| seg.to_ascii_lowercase().replace('_', "-"))
            .collect();
        if path.iter().any(String::is_empty) {
            continue;
        }
        let parsed: serde_yaml::Value =
            serde_yaml::from_str(&v).unwrap_or_else(|_| serde_yaml::Value::String(v.clone()));
        set_at_path(value, &path, parsed);
        applied = true;
    }
    applied
}

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
