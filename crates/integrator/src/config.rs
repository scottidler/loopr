//! `IntegratorConfig`: per-integrator runtime knobs.
//!
//! First gate: `allow_multi_bundle = false` forces single-Bundle Ticks.
//! When a real run needs multi-Bundle, flip the default along with a
//! separate design doc that reconciles the rollback-divergence
//! noted in the Integrator design.

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct IntegratorConfig {
    /// Maximum wall-clock duration for any single git subprocess.
    /// Default 60s (`GIT_TIMEOUT_SECS_DEFAULT`).
    pub git_timeout: Duration,

    /// First-gate guardrail. When `false`, `integrate` rejects any
    /// call with more than one Bundle by returning
    /// `IntegrationError::MultiBundleNotSupported { count }`.
    /// Default `false`.
    pub allow_multi_bundle: bool,

    /// Shell commands run after a successful git merge, before Tick
    /// persistence. Each string is passed to `sh -c`. Commands run
    /// sequentially; the first non-zero exit rolls back the merge and
    /// returns `IntegrationError::ValidationFailed`.
    /// Default `vec![]` (skip validation entirely).
    pub validation_commands: Vec<String>,

    /// Wall-clock cap for each individual validation command.
    /// Default 300s (`VALIDATION_TIMEOUT_SECS_DEFAULT`).
    pub validation_timeout: Duration,
}

const GIT_TIMEOUT_SECS_DEFAULT: u64 = 60;
const VALIDATION_TIMEOUT_SECS_DEFAULT: u64 = 300;

impl Default for IntegratorConfig {
    fn default() -> Self {
        Self {
            git_timeout: Duration::from_secs(GIT_TIMEOUT_SECS_DEFAULT),
            allow_multi_bundle: false,
            validation_commands: vec![],
            validation_timeout: Duration::from_secs(VALIDATION_TIMEOUT_SECS_DEFAULT),
        }
    }
}
