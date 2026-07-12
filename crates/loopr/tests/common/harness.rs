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
use telemetry::{ProcessId, SessionId};
use tools::SandboxMode;

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
        // Phase 6: nested unwrap pattern (mirrors production
        // `daemon::serve` shutdown). DaemonContext now holds
        // `Arc<Store>` plus a `SummaryFanout` decorator that itself
        // holds two more `Arc<Store>` clones.
        match Arc::try_unwrap(ctx) {
            Ok(owned) => {
                let store_clone = Arc::clone(&owned.store);
                drop(owned);
                match Arc::try_unwrap(store_clone) {
                    Ok(store) => store
                        .close()
                        .await
                        .map_err(|e| LooprError::DaemonStartup(format!("test store close: {e}")))?,
                    Err(still_shared) => {
                        eprintln!(
                            "TestDaemon::shutdown: Arc<Store> still shared (strong_count={}); store falls back to sync Drop",
                            Arc::strong_count(&still_shared)
                        );
                    }
                }
            }
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
    let session_id = SessionId::parse("20260422-000000").expect("SessionId::parse");
    let process_id = ProcessId::parse("pc-test01").expect("ProcessId::parse");
    let target_slug = "-test-harness".to_string();
    // Default Config has `SandboxMode::Required` which needs bwrap on the
    // host. Tests run without that assumption, so force `Off`; individual
    // tests that want to exercise sandbox paths can build their own Config.
    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;
    // Director polls every 5s by default and idles 15s - far too slow for
    // tests. Both intervals are 0 so each tokio::time::sleep just yields,
    // giving the Director a tight reactive loop on the test runtime.
    // Production callers override via config.yml.
    config.agents.director.poll_interval_secs = 0;
    config.agents.director.idle_interval_secs = 0;
    // Phase 12 (validation-by-default): this shared harness is not
    // exercising validation semantics, so every daemon it spawns opts
    // out of the require-validation-by-default startup gate. Tests that
    // DO want to exercise validation build their own `Config` directly.
    config.integrator.require_validation = false;

    // Tests boot with a clean target, so the corruption gate is a no-op
    // by default (no JSONL exists yet). Pass `false` to keep the
    // production-default semantics — tests that want to exercise
    // corruption can write a malformed JSONL pre-boot and pass `true`.
    // Phase 7: tests use a fresh ProcessSnapshot per spawn; no metering
    // happens by default (the test passes the unmetered LLM stub).
    let snapshot = std::sync::Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        "test-stub-model",
    )));
    let ctx = build_context(
        target.clone(),
        session_id,
        target_slug,
        process_id,
        0,
        llm,
        config,
        false,
        snapshot,
    )
    .await?;
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
