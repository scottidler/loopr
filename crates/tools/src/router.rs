use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::{debug, info, instrument, warn};

use crate::config::{LaneTighten, ToolsConfig};
use crate::env::scrub_command;
use crate::error::ToolError;
use crate::lane::{Lane, LanePolicy};
use crate::sandbox::{SandboxMode, bwrap_command, detect_bwrap_functional};
use crate::spawn::{KillStrategy, PersistConfig, SpawnResult, spawn_with_process_group};

#[derive(thiserror::Error, Debug)]
pub enum RouterInitError {
    #[error("sandbox=required but bwrap is not functional on this host")]
    BwrapRequired,
}

pub struct LaneRouter {
    policies: HashMap<Lane, LanePolicy>,
    semaphores: HashMap<Lane, Arc<Semaphore>>,
    sandbox: SandboxMode,
    /// D14 (Architect R2 amendment): the implementation performs exactly one
    /// bwrap detection — `detect_bwrap_functional()`, which invokes bwrap
    /// with the full flag set against `/bin/true`. There is no separate
    /// `--version` binary-presence check. The spec originally requested
    /// `bwrap_available` + `bwrap_functional` as distinct log fields; since
    /// both would collapse to the same value in every code path, we carry
    /// one field, named for what it actually measures.
    bwrap_functional: bool,
}

impl LaneRouter {
    /// Construct the router and enforce the sandbox posture.
    ///
    /// - `Required` + no bwrap => error (daemon refuses to start).
    /// - `Preferred` + no bwrap => warn + continue unsandboxed.
    /// - `Off` => skip detection entirely (quiet).
    pub fn new(sandbox: SandboxMode) -> Result<Self, RouterInitError> {
        Self::with_config(sandbox, &ToolsConfig::default())
    }

    /// Construct with target-supplied tighten-only lane overrides
    /// (Phase-5 finding 13). The sandbox posture enforcement is identical to
    /// `new`; only the per-lane slot/timeout policies differ.
    pub fn with_config(sandbox: SandboxMode, cfg: &ToolsConfig) -> Result<Self, RouterInitError> {
        let bwrap_functional = match sandbox {
            SandboxMode::Off => false,
            _ => detect_bwrap_functional(),
        };
        match (sandbox, bwrap_functional) {
            (SandboxMode::Required, false) => {
                return Err(RouterInitError::BwrapRequired);
            }
            (SandboxMode::Preferred, false) => {
                warn!("bwrap not functional; Local lane will run UNSANDBOXED (sandbox: preferred)");
            }
            _ => {}
        }
        Ok(Self::build(sandbox, bwrap_functional, cfg))
    }

    fn build(sandbox: SandboxMode, bwrap_functional: bool, cfg: &ToolsConfig) -> Self {
        let policies = HashMap::from([
            (Lane::Local, tighten(LanePolicy::local(), &cfg.lane_overrides.local)),
            (Lane::Net, tighten(LanePolicy::net(), &cfg.lane_overrides.net)),
            (Lane::Heavy, tighten(LanePolicy::heavy(), &cfg.lane_overrides.heavy)),
        ]);
        let semaphores: HashMap<Lane, Arc<Semaphore>> = policies
            .iter()
            .map(|(lane, policy)| (*lane, Arc::new(Semaphore::new(policy.max_slots))))
            .collect();

        info!(
            ?sandbox,
            bwrap_functional,
            local_slots = policies[&Lane::Local].max_slots,
            net_slots = policies[&Lane::Net].max_slots,
            heavy_slots = policies[&Lane::Heavy].max_slots,
            "LaneRouter initialized"
        );

        Self {
            policies,
            semaphores,
            sandbox,
            bwrap_functional,
        }
    }

    pub fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn bwrap_functional(&self) -> bool {
        self.bwrap_functional
    }

    pub fn policy(&self, lane: Lane) -> Option<&LanePolicy> {
        self.policies.get(&lane)
    }

    pub fn available_slots(&self, lane: Lane) -> usize {
        self.semaphores.get(&lane).map(|s| s.available_permits()).unwrap_or(0)
    }

    /// Execute a pre-built `Command` under the lane's slot limit, timeout, and
    /// sandbox posture.
    ///
    /// Sandboxing decision:
    /// - `Local` + `sandbox != Off` + `bwrap_available` => wrap with bwrap.
    /// - Anything else => run plain.
    ///
    /// Kill strategy is chosen from the wrap decision (bwrap-wrapped =>
    /// `KillStrategy::BwrapChild`; plain => `KillStrategy::Pgid`), per D16.
    #[instrument(
        name = "router.spawn",
        level = "debug",
        skip_all,
        fields(
            lane = lane.as_str(),
            working_dir = %working_dir.display(),
            timeout_secs = ?timeout_secs,
            sandbox = ?self.sandbox,
        ),
        err,
    )]
    pub async fn spawn(
        &self,
        cmd: tokio::process::Command,
        lane: Lane,
        working_dir: &Path,
        timeout_secs: Option<u64>,
        persist: PersistConfig<'_>,
    ) -> Result<SpawnResult, ToolError> {
        let policy = self
            .policies
            .get(&lane)
            .copied()
            .ok_or(ToolError::ExecutionFailed(format!("unknown lane: {lane:?}")))?;
        let timeout = timeout_secs
            .unwrap_or(policy.default_timeout_secs)
            .min(policy.max_timeout_secs);

        debug!(
            lane = lane.as_str(),
            timeout_secs = timeout,
            sandbox = ?self.sandbox,
            "router: dispatched"
        );

        debug!(?lane, timeout_secs = timeout, "router spawn acquiring slot");

        let permit = self.semaphores[&lane]
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolError::LaneClosed(lane))?;

        let wrap = policy.sandbox && self.bwrap_functional && !matches!(self.sandbox, SandboxMode::Off);

        let (mut final_cmd, kill_strategy) = if wrap {
            debug!(network = policy.network, "wrapping command with bwrap");
            (
                bwrap_command(cmd, working_dir, policy.network),
                KillStrategy::BwrapChild,
            )
        } else {
            (cmd, KillStrategy::Pgid)
        };

        // D12: strip secret-bearing env vars from the subprocess. Applied
        // AFTER bwrap-wrapping because the env set on the outer bwrap
        // Command is what bwrap's `execve` of the inner shell inherits;
        // bwrap does not mutate env across its namespace boundary. The
        // scrub covers both wrapped and plain paths.
        scrub_command(&mut final_cmd);

        let result = spawn_with_process_group(final_cmd, timeout, kill_strategy, persist)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()));

        drop(permit);
        debug!(?lane, "router spawn released slot");

        result
    }
}

/// Apply a target's tighten-only override to a base lane policy. Each knob
/// is clamped to be no looser than the built-in default (`min`); slots floor
/// at 1 so a misconfigured `slots: 0` can't deadlock the lane. A `None` knob
/// keeps the default.
fn tighten(base: LanePolicy, ov: &LaneTighten) -> LanePolicy {
    LanePolicy {
        max_slots: ov.slots.map(|s| s.min(base.max_slots).max(1)).unwrap_or(base.max_slots),
        default_timeout_secs: ov
            .default_timeout_secs
            .map(|t| t.min(base.default_timeout_secs))
            .unwrap_or(base.default_timeout_secs),
        max_timeout_secs: ov
            .max_timeout_secs
            .map(|t| t.min(base.max_timeout_secs))
            .unwrap_or(base.max_timeout_secs),
        ..base
    }
}

#[cfg(test)]
mod tests;
