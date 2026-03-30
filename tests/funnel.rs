//! Integration tests: Coordinator Interviewing FSM (Constrained Persona Matrix)
//!
//! Drive the Coordinator through its `Interviewing` FSM state using scripted,
//! turn-indexed persona responses fed over Unix socket JSON-RPC IPC.
//!
//! All LLM-driven tests are `#[ignore]` - they require a live ANTHROPIC_API_KEY
//! and are slow (3-10 LLM calls each). Run them manually:
//!
//! ```sh
//! cargo test --test funnel -- --ignored
//! ```
//!
//! Timeout is configurable via `LOOPR_TEST_TIMEOUT_SECS` (default: 120).
//!
//! # IPC Flow
//!
//! 1. `coordinator.set_goal`   - persists the goal (does NOT start the agent)
//! 2. `agent.start`            - spawns the Coordinator agent task
//! 3. `coordinator.interview_question` (event) - Coordinator asks a question
//! 4. `coordinator.interview_respond`  - driver sends turn-indexed answer
//! 5. Repeat 3-4 until `record.created` (collection: "plan") is received

#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use eyre::{Result, eyre};
use loopr::config::{Config, DaemonConfig, ProjectConfig};
use loopr::daemon::context::DaemonContext;
use loopr::daemon::daemon_main;
use loopr::ipc::client::IpcClient;
use loopr::ipc::protocol::{DaemonEvent, IpcMessage};
use serde_json::json;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Unique ID helper (no external deps)
// ---------------------------------------------------------------------------

fn test_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}", std::process::id(), n)
}

// ---------------------------------------------------------------------------
// Persona fixture
// ---------------------------------------------------------------------------

/// A scripted interview persona with hardcoded turn-indexed responses.
///
/// The `PersonaDriver` feeds `responses[turn]` as the answer to each
/// `coordinator.interview_question` event, falling back to `fallback` once
/// the scripted responses are exhausted.
pub struct PersonaFixture {
    /// Human-readable name used in diagnostics and failure messages.
    pub name: &'static str,
    /// Initial goal text sent via `coordinator.set_goal`.
    pub initial_goal: &'static str,
    /// Turn-indexed answers: `responses[0]` answers the first question, etc.
    pub responses: &'static [&'static str],
    /// Answer used for every question once `responses` is exhausted.
    pub fallback: &'static str,
}

/// Golden Path: a clear, well-scoped request that the Coordinator should
/// convert to a coherent Draft Plan with minimal interview friction.
pub const GOLDEN_PATH: PersonaFixture = PersonaFixture {
    name: "Golden Path",
    initial_goal: "Build a CLI todo app in Rust with SQLite persistence",
    responses: &[
        "Yes, I want add, list, and done subcommands.",
        "A single local SQLite file is fine. No sync needed.",
        "That plan looks good to me.",
    ],
    fallback: "That sounds fine, go ahead.",
};

// ---------------------------------------------------------------------------
// Temp dir RAII
// ---------------------------------------------------------------------------

/// A temporary directory that is removed (with all contents) when dropped.
struct TempTestDir(PathBuf);

impl TempTestDir {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{}-{}", prefix, test_id()));
        std::fs::create_dir_all(&path).expect("failed to create test temp dir");
        Self(path)
    }

    fn path(&self) -> &PathBuf {
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
struct DaemonHandle {
    pub socket_path: PathBuf,
    task: JoinHandle<eyre::Result<()>>,
    _temp_dir: TempTestDir,
}

impl DaemonHandle {
    /// Spawn a fresh daemon in `temp_dir` and wait for its socket to appear.
    ///
    /// The config overrides socket_path, pid_path, and repo_path to the temp
    /// directory. `auto_start_coordinator` and pull-based workers remain off
    /// (Config::default) so the test drives startup explicitly via `agent.start`.
    async fn spawn(temp_dir: TempTestDir) -> Result<Self> {
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

        let session_id = format!("funnel-{}", test_id());
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

// ---------------------------------------------------------------------------
// Persona driver
// ---------------------------------------------------------------------------

/// Drive a `PersonaFixture` through the Coordinator interview via IPC.
///
/// Sends `coordinator.set_goal` + `agent.start`, then responds to each
/// `coordinator.interview_question` event with the turn-indexed answer from
/// `fixture.responses` (or `fixture.fallback` once exhausted).
///
/// Events collected as side-effects of `request()` calls are buffered and
/// processed before reading further from the socket, preventing dropped events
/// under race conditions where the Coordinator responds before the client's
/// `recv()` loop is entered.
///
/// Returns the plan ID from the `record.created` (collection: "plan") event.
async fn run_persona(socket_path: &PathBuf, fixture: &PersonaFixture) -> Result<String> {
    let mut client = IpcClient::connect(socket_path).await?;
    let mut turn = 0usize;
    let mut buffered: VecDeque<DaemonEvent> = VecDeque::new();

    // Set goal. Does NOT start the agent - that requires a separate agent.start call.
    let (goal_resp, goal_events) = client
        .request("coordinator.set_goal", json!({"goal": fixture.initial_goal}))
        .await
        .map_err(|e| eyre!("coordinator.set_goal failed: {e}"))?;
    if goal_resp.is_error() {
        return Err(eyre!("coordinator.set_goal returned error: {:?}", goal_resp.error));
    }
    buffered.extend(goal_events);

    // Start the Coordinator agent. In sync test contexts auto-start is skipped
    // (handlers.rs:4218), so agent.start must always be called explicitly.
    let (start_resp, start_events) = client
        .request("agent.start", json!({"agent_type": "coordinator"}))
        .await
        .map_err(|e| eyre!("agent.start failed: {e}"))?;
    if start_resp.is_error() {
        return Err(eyre!("agent.start returned error: {:?}", start_resp.error));
    }
    buffered.extend(start_events);

    let timeout_secs = std::env::var("LOOPR_TEST_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(120);

    let plan_id = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            // Drain buffered events (side-effects of request() calls) first.
            let event = if let Some(e) = buffered.pop_front() {
                e
            } else {
                match client.recv().await? {
                    Some(IpcMessage::Event(e)) => e,
                    Some(IpcMessage::Response(_)) => continue, // stray response - skip
                    None => return Err(eyre!("daemon disconnected before plan was created")),
                }
            };

            match event.event.as_str() {
                "coordinator.interview_question" => {
                    let answer = fixture.responses.get(turn).copied().unwrap_or(fixture.fallback);
                    let (_, resp_events) = client
                        .request("coordinator.interview_respond", json!({"answer": answer}))
                        .await?;
                    buffered.extend(resp_events);
                    turn += 1;
                }
                "record.created" if event.data["collection"].as_str() == Some("plan") => {
                    let id = event.data["id"].as_str().unwrap_or_default().to_string();
                    return Ok::<String, eyre::Error>(id);
                }
                _ => {} // ignore agent.status_changed and other events
            }
        }
    })
    .await
    .map_err(|_| {
        eyre!(
            "persona '{}': timed out after {}s waiting for record.created (collection: plan)",
            fixture.name,
            timeout_secs,
        )
    })??;

    Ok(plan_id)
}

// ---------------------------------------------------------------------------
// Persona tests (ignored - require ANTHROPIC_API_KEY, 3-10 LLM calls each)
// ---------------------------------------------------------------------------

/// Phase 1 - Golden Path: assert `record.created` for plans within timeout.
///
/// This is the foundational plumbing test: does a well-scoped goal produce
/// a plan record via the full interview -> ProposePlan -> record.created path?
#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and live Coordinator LLM; run with: cargo test --test funnel -- --ignored"]
async fn test_persona_golden_path() {
    let temp_dir = TempTestDir::new("loopr-funnel");
    let daemon = DaemonHandle::spawn(temp_dir).await.unwrap();
    let plan_id = run_persona(&daemon.socket_path, &GOLDEN_PATH).await.unwrap();
    assert!(
        !plan_id.is_empty(),
        "expected non-empty plan_id from Golden Path interview"
    );
}

// ---------------------------------------------------------------------------
// Fast structural tests (always run - no LLM, no daemon required)
// ---------------------------------------------------------------------------

#[test]
fn test_persona_fixture_fields() {
    assert!(!GOLDEN_PATH.name.is_empty(), "name must not be empty");
    assert!(!GOLDEN_PATH.initial_goal.is_empty(), "initial_goal must not be empty");
    assert!(!GOLDEN_PATH.fallback.is_empty(), "fallback must not be empty");
    assert!(
        !GOLDEN_PATH.responses.is_empty(),
        "GOLDEN_PATH must have at least one scripted response"
    );
}

#[test]
fn test_temp_test_dir_lifecycle() {
    let dir = TempTestDir::new("loopr-funnel-fixture");
    let path = dir.path().clone();
    assert!(path.exists(), "temp dir should exist after creation");
    drop(dir);
    assert!(!path.exists(), "temp dir should be removed on drop");
}
