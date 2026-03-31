use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, eyre};
use log::{debug, info, warn};
use tokio::sync::Semaphore;

use crate::tools::lane::{Lane, LanePolicy};
use crate::tools::sandbox::{bwrap_command, detect_bwrap, log_bwrap_status};
use crate::tools::spawn::{SpawnResult, shell_command, spawn_with_process_group};

/// Manages lane semaphores and dispatches tool execution with isolation.
pub struct LaneRouter {
    policies: HashMap<Lane, LanePolicy>,
    semaphores: HashMap<Lane, Arc<Semaphore>>,
    bwrap_available: bool,
}

impl Default for LaneRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneRouter {
    pub fn new() -> Self {
        let policies = HashMap::from([
            (Lane::Local, LanePolicy::local()),
            (Lane::Net, LanePolicy::net()),
            (Lane::Heavy, LanePolicy::heavy()),
        ]);
        let semaphores = policies
            .iter()
            .map(|(lane, policy)| (*lane, Arc::new(Semaphore::new(policy.max_slots))))
            .collect();

        let bwrap_available = detect_bwrap();
        log_bwrap_status();

        info!(
            "LaneRouter initialized: local={} slots, net={} slots, heavy={} slots, bwrap={}",
            LanePolicy::local().max_slots,
            LanePolicy::net().max_slots,
            LanePolicy::heavy().max_slots,
            bwrap_available,
        );

        Self {
            policies,
            semaphores,
            bwrap_available,
        }
    }

    /// Execute a shell command in the appropriate lane.
    ///
    /// 1. Acquires a slot semaphore (blocks if lane is full)
    /// 2. Builds the command (plain shell for now; bwrap wrapping added in Phase 3)
    /// 3. Spawns with setsid() for process group isolation
    /// 4. Releases slot on completion
    pub async fn spawn(
        &self,
        command: &str,
        working_dir: &Path,
        lane: Lane,
        timeout_secs: Option<u64>,
    ) -> Result<SpawnResult> {
        let policy = self
            .policies
            .get(&lane)
            .ok_or_else(|| eyre!("unknown lane: {:?}", lane))?;
        let timeout = timeout_secs
            .unwrap_or(policy.default_timeout_secs)
            .min(policy.max_timeout_secs);

        debug!(
            "LaneRouter::spawn(lane={}, timeout={}s, command={})",
            lane, timeout, command
        );

        // 1. Acquire slot (blocks until available)
        let permit = self.semaphores[&lane]
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| eyre!("lane {} semaphore closed", lane))?;

        debug!("acquired slot in lane {}", lane);

        // 2. Build command - bwrap for sandboxed lanes, plain shell otherwise
        let cmd = if policy.sandbox_net && self.bwrap_available {
            debug!("wrapping command with bwrap --unshare-net");
            bwrap_command(command, working_dir)
        } else {
            shell_command(command, working_dir)
        };

        // 3. Spawn with setsid() for process group isolation
        let result = spawn_with_process_group(cmd, timeout).await;

        // 4. Slot released on drop
        drop(permit);
        debug!("released slot in lane {}", lane);

        result
    }

    /// Get the policy for a lane.
    pub fn policy(&self, lane: Lane) -> Option<&LanePolicy> {
        self.policies.get(&lane)
    }

    /// Whether bwrap sandboxing is available for the Local lane.
    pub fn bwrap_available(&self) -> bool {
        self.bwrap_available
    }

    /// Get the number of available slots for a lane.
    pub fn available_slots(&self, lane: Lane) -> usize {
        self.semaphores.get(&lane).map(|s| s.available_permits()).unwrap_or(0)
    }

    /// Start a tool in the background, returning immediately with a task handle.
    ///
    /// The agent can poll the output_path or await the handle for the result.
    /// Slot acquisition happens inside the background task (via self.spawn).
    pub fn spawn_background(
        self: &Arc<Self>,
        command: &str,
        working_dir: &Path,
        lane: Lane,
        timeout_secs: Option<u64>,
    ) -> BackgroundTask {
        let task_id = format!(
            "bg-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        let output_dir = std::env::temp_dir().join("loopr-bg");
        let _ = std::fs::create_dir_all(&output_dir);
        let output_path = output_dir.join(format!("{}.log", task_id));

        let router = Arc::clone(self);
        let cmd = command.to_string();
        let dir = working_dir.to_path_buf();
        let out = output_path.clone();
        let tid = task_id.clone();

        let handle = tokio::spawn(async move {
            debug!("background task {} starting", tid);
            let result = router
                .spawn(&cmd, &dir, lane, timeout_secs)
                .await
                .unwrap_or_else(|e| SpawnResult {
                    stdout: String::new(),
                    stderr: e.to_string(),
                    exit_code: -1,
                    duration_ms: 0,
                    timed_out: false,
                    persisted_output_path: None,
                });

            if let Err(e) = std::fs::write(
                &out,
                format!(
                    "exit_code: {}\n---stdout---\n{}\n---stderr---\n{}",
                    result.exit_code, result.stdout, result.stderr
                ),
            ) {
                warn!("background task {} failed to write output: {}", tid, e);
            }

            debug!("background task {} completed: exit_code={}", tid, result.exit_code);
            result
        });

        BackgroundTask {
            task_id,
            output_path,
            handle,
        }
    }
}

/// Handle for a backgrounded tool execution.
pub struct BackgroundTask {
    /// Unique identifier for this background task.
    pub task_id: String,
    /// Path where the full output will be written on completion.
    pub output_path: PathBuf,
    /// Join handle to await the result.
    pub handle: tokio::task::JoinHandle<SpawnResult>,
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_router_new() {
        let router = LaneRouter::new();
        assert!(router.policy(Lane::Local).is_some());
        assert!(router.policy(Lane::Net).is_some());
        assert!(router.policy(Lane::Heavy).is_some());
    }

    #[test]
    fn test_router_initial_slots() {
        let router = LaneRouter::new();
        assert_eq!(router.available_slots(Lane::Local), 10);
        assert_eq!(router.available_slots(Lane::Net), 5);
        assert_eq!(router.available_slots(Lane::Heavy), 1);
    }

    #[tokio::test]
    async fn test_router_spawn_local() {
        let router = LaneRouter::new();
        let result = router
            .spawn("echo hello", &std::env::temp_dir(), Lane::Local, Some(10))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn test_router_spawn_heavy() {
        let router = LaneRouter::new();
        let result = router
            .spawn("echo heavy", &std::env::temp_dir(), Lane::Heavy, Some(10))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout.trim(), "heavy");
    }

    #[tokio::test]
    async fn test_router_timeout_clamped() {
        let router = LaneRouter::new();
        // Local max is 60s; requesting 999 should clamp to 60
        let result = router
            .spawn("echo clamped", &std::env::temp_dir(), Lane::Local, Some(999))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn test_router_slot_release_after_completion() {
        let router = LaneRouter::new();
        assert_eq!(router.available_slots(Lane::Heavy), 1);

        let result = router
            .spawn("echo done", &std::env::temp_dir(), Lane::Heavy, Some(10))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 0);

        // Slot should be back
        assert_eq!(router.available_slots(Lane::Heavy), 1);
    }

    #[tokio::test]
    async fn test_router_heavy_serializes() {
        let router = Arc::new(LaneRouter::new());
        let r1 = router.clone();
        let r2 = router.clone();
        let dir = std::env::temp_dir();

        // Both tasks start at the same time but Heavy has 1 slot
        let (a, b) = tokio::join!(
            r1.spawn("echo first", &dir, Lane::Heavy, Some(10)),
            r2.spawn("echo second", &dir, Lane::Heavy, Some(10)),
        );

        assert_eq!(a.unwrap().exit_code, 0);
        assert_eq!(b.unwrap().exit_code, 0);
        assert_eq!(router.available_slots(Lane::Heavy), 1);
    }
}
