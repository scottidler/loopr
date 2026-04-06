//! Shared test infrastructure for integration tests.
//!
//! Provides `TempTestDir`, `DaemonHandle`, and `test_id()` used by
//! bridge, chat_prompts, and funnel integration tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eyre::{Result, eyre};
use loopr::config::{Config, DaemonConfig, ProjectConfig};
use loopr::daemon::context::DaemonContext;
use loopr::daemon::daemon_main;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Unique ID helper (no external deps)
// ---------------------------------------------------------------------------

pub fn test_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

// ---------------------------------------------------------------------------
// Temp dir RAII
// ---------------------------------------------------------------------------

/// A temporary directory that is removed (with all contents) when dropped.
pub struct TempTestDir(PathBuf);

impl TempTestDir {
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{}-{}", prefix, test_id()));
        std::fs::create_dir_all(&path).expect("failed to create test temp dir");
        Self(path)
    }

    pub fn path(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempTestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Test daemon handle
// ---------------------------------------------------------------------------

/// An in-process daemon spawned for integration testing.
///
/// Uses an isolated temp directory and socket path per test instance.
/// The daemon Tokio task is aborted when this handle is dropped.
///
/// **Note:** The supervisor task spawned inside `daemon_main` is not
/// automatically aborted. It will exit once its `Stores` Arc and broadcast
/// channel sender are released after the `DaemonContext` is dropped.
pub struct DaemonHandle {
    pub socket_path: PathBuf,
    task: JoinHandle<eyre::Result<()>>,
    _temp_dir: TempTestDir,
}

impl DaemonHandle {
    /// Spawn a fresh daemon in `temp_dir` and wait for its socket to appear.
    ///
    /// The config overrides socket_path, pid_path, and repo_path to the temp
    /// directory. Pull-based workers remain off (Config::default) so the test
    /// drives startup explicitly via `agent.start`.
    pub async fn spawn(temp_dir: TempTestDir) -> Result<Self> {
        // Initialize prompts before starting daemon tasks
        loopr::prompts::init_defaults();

        let socket_path = temp_dir.path().join("daemon.sock");
        let pid_path = temp_dir.path().join("daemon.pid");
        let repo_path = temp_dir.path().join("repo");
        let session_dir = temp_dir.path().join("session");
        std::fs::create_dir_all(&repo_path)?;
        std::fs::create_dir_all(&session_dir)?;

        let config = Config {
            daemon: DaemonConfig {
                socket_path: socket_path.clone(),
                pid_path,
            },
            project: ProjectConfig {
                repo_path,
                ..ProjectConfig::default()
            },
            ..Config::default()
        };

        let session_id = format!("test-{}", test_id());
        let (ctx, _) = DaemonContext::shared(config, session_id, session_dir)?;
        let task = tokio::spawn(async move { daemon_main(ctx).await });

        // Wait for socket to appear (up to 5 seconds).
        for _ in 0..50 {
            if socket_path.exists() {
                return Ok(Self {
                    socket_path,
                    task,
                    _temp_dir: temp_dir,
                });
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        task.abort();
        Err(eyre!("daemon socket never appeared at {}", socket_path.display()))
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
