use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

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
    bwrap_available: bool,
}

impl LaneRouter {
    /// Construct the router and enforce the sandbox posture.
    ///
    /// - `Required` + no bwrap => error (daemon refuses to start).
    /// - `Preferred` + no bwrap => warn + continue unsandboxed.
    /// - `Off` => skip detection entirely (quiet).
    pub fn new(sandbox: SandboxMode) -> Result<Self, RouterInitError> {
        let bwrap_available = match sandbox {
            SandboxMode::Off => false,
            _ => detect_bwrap_functional(),
        };
        match (sandbox, bwrap_available) {
            (SandboxMode::Required, false) => {
                return Err(RouterInitError::BwrapRequired);
            }
            (SandboxMode::Preferred, false) => {
                warn!("bwrap not available; Local lane will run UNSANDBOXED (sandbox: preferred)");
            }
            _ => {}
        }
        Ok(Self::build(sandbox, bwrap_available))
    }

    fn build(sandbox: SandboxMode, bwrap_available: bool) -> Self {
        let policies = HashMap::from([
            (Lane::Local, LanePolicy::local()),
            (Lane::Net, LanePolicy::net()),
            (Lane::Heavy, LanePolicy::heavy()),
        ]);
        let semaphores = policies
            .iter()
            .map(|(lane, policy)| (*lane, Arc::new(Semaphore::new(policy.max_slots))))
            .collect();

        info!(
            ?sandbox,
            bwrap_available,
            local_slots = LanePolicy::local().max_slots,
            net_slots = LanePolicy::net().max_slots,
            heavy_slots = LanePolicy::heavy().max_slots,
            "LaneRouter initialized"
        );

        Self {
            policies,
            semaphores,
            sandbox,
            bwrap_available,
        }
    }

    pub fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox
    }

    pub fn bwrap_available(&self) -> bool {
        self.bwrap_available
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

        debug!(?lane, timeout_secs = timeout, "router spawn acquiring slot");

        let permit = self.semaphores[&lane]
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ToolError::LaneClosed(lane))?;

        let wrap = policy.sandbox_net && self.bwrap_available && !matches!(self.sandbox, SandboxMode::Off);

        let (final_cmd, kill_strategy) = if wrap {
            debug!("wrapping command with bwrap");
            (bwrap_command(cmd, working_dir), KillStrategy::BwrapChild)
        } else {
            (cmd, KillStrategy::Pgid)
        };

        let result = spawn_with_process_group(final_cmd, timeout, kill_strategy, persist)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()));

        drop(permit);
        debug!(?lane, "router spawn released slot");

        result
    }
}

#[cfg(test)]
mod tests;
