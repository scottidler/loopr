//! In-process daemon test harness.
//!
//! `spawn_test_daemon` wires up a minimal `DaemonContext<L>` against a
//! fresh tempdir and kicks off `serve_core` in a tokio task, returning a
//! `TestDaemon` that owns the tempdir, the task handle, and a
//! `DaemonHandle` for shutdown. No fork, no signal handlers, no pid file.
//!
//! **Ownership contract:** `TestDaemon` does NOT hold an
//! `Arc<DaemonContext>` clone; that Arc lives inside the spawned task and
//! is returned by `serve_core` on completion. `shutdown(self)` consumes
//! the harness, awaits the task, `try_unwrap`s the returned Arc, and
//! closes the store — mirroring production's `serve` wrapper on a test
//! path that skips signal handling.
//!
//! **Tests MUST call `shutdown().await`.** Dropping without calling it
//! detaches the tokio task and immediately drops the tempdir; any
//! pending writes hit ENOENT. `#[must_use]` on the struct surfaces a
//! compiler warning. No `Drop` impl — `block_on` from inside a
//! `#[tokio::test]` runtime panics ("cannot start a runtime from within
//! a runtime"), so attempting to shut down from `Drop` is worse than
//! the default JoinHandle-detach.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use llm::LlmClient;
use loopr::config::Config;
use loopr::daemon::{DaemonContext, DaemonHandle, build_context, serve_core};
use loopr::error::LooprError;
use telemetry::RunId;

use super::init_git_repo;

const SOCKET_POLL_DEADLINE_MS: u64 = 2000;
const SOCKET_POLL_INTERVAL_MS: u64 = 10;

#[must_use = "TestDaemon leaks its tokio task unless .shutdown().await is called"]
pub struct TestDaemon<L: LlmClient + Send + Sync + 'static> {
    handle: DaemonHandle,
    task: tokio::task::JoinHandle<Result<Arc<DaemonContext<L>>, LooprError>>,
    /// Kept alive so the store's on-disk files outlive the daemon.
    /// Drops LAST (after the task has joined inside `shutdown`).
    tempdir: TempDir,
}

impl<L: LlmClient + Send + Sync + 'static> TestDaemon<L> {
    pub fn handle(&self) -> &DaemonHandle {
        &self.handle
    }

    pub fn target(&self) -> &Path {
        self.tempdir.path()
    }

    pub fn task_mut(&mut self) -> &mut tokio::task::JoinHandle<Result<Arc<DaemonContext<L>>, LooprError>> {
        &mut self.task
    }

    /// Consume the harness, drive clean shutdown.
    ///
    /// 1. Flip shutdown atomics via the handle.
    /// 2. Await the spawned `serve_core` task; propagate its `JoinError`
    ///    or `LooprError`.
    /// 3. `try_unwrap` the returned Arc and `store.close().await`.
    /// 4. Drop the tempdir (last field; cleaned up after the store is
    ///    closed so no pending writer hits ENOENT).
    pub async fn shutdown(self) -> Result<(), LooprError> {
        self.handle.shutdown();
        let ctx = self
            .task
            .await
            .map_err(|e| LooprError::DaemonStartup(format!("test task join: {e}")))??;
        match Arc::try_unwrap(ctx) {
            Ok(owned) => owned
                .store
                .close()
                .await
                .map_err(|e| LooprError::DaemonStartup(format!("test store close: {e}")))?,
            Err(still_shared) => {
                eprintln!(
                    "TestDaemon::shutdown: DaemonContext Arc still shared (strong_count={}); store falls back to sync Drop",
                    Arc::strong_count(&still_shared)
                );
            }
        }
        Ok(())
    }
}

/// Spawn a daemon pipeline in-process with the supplied `LlmClient`.
///
/// Creates a fresh `TempDir`, `git init`'s it with an empty initial
/// commit, opens a `Store`, builds a `DaemonContext<L>` with
/// `Config::default()`, and spawns `serve_core` into a tokio task. Polls
/// until the IPC socket exists (`wait_for_socket` pattern from
/// `daemon.rs`), then returns the `TestDaemon`.
pub async fn spawn_test_daemon<L>(llm: L) -> Result<TestDaemon<L>, LooprError>
where
    L: LlmClient + Send + Sync + 'static,
{
    let tempdir = TempDir::new().expect("tempdir");
    init_git_repo(tempdir.path());

    let target = tempdir.path().to_path_buf();
    let run_id = RunId::parse("20260422-000000").expect("RunId::parse");
    let config = Config::default();

    let ctx = build_context(target.clone(), run_id, 0, llm, config).await?;
    let handle = DaemonHandle::from_context(&ctx);

    let task = tokio::spawn(async move { serve_core(ctx).await });

    wait_for_socket(handle.socket_path()).await?;

    Ok(TestDaemon { handle, task, tempdir })
}

async fn wait_for_socket(socket: &Path) -> Result<(), LooprError> {
    let deadline = Instant::now() + Duration::from_millis(SOCKET_POLL_DEADLINE_MS);
    while Instant::now() < deadline {
        if socket.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(SOCKET_POLL_INTERVAL_MS)).await;
    }
    Err(LooprError::DaemonStartup(format!(
        "socket never appeared at {}",
        socket.display()
    )))
}
