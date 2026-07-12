//! Phase 19 (`docs/design/2026-07-11-verified-swarm.md`): failure-path
//! reaping, end-to-end against the real daemon pipeline.
//!
//! Drives a Work to a real `IntegrationFailed` (a real
//! `integrator.validation-commands` failure, Phase 12) via the daemon's
//! own cold-boot reconcile requeue -- an `Accepted` Bundle is pre-seeded
//! directly into the store (bypassing the decompose/implement/review LLM
//! dance, which has its own pre-existing OCC-race flake unrelated to this
//! phase), so `spawn_integrator_for_bundle` runs for real against a real
//! git worktree with no Director/Reviewer LLM round-trip in the critical
//! path. Asserts the Phase 10 warm worktree this leaves behind:
//!
//! 1. Survives while the Work sits `Blocked` (non-terminal, retryable --
//!    the operator-inspectable evidence Phase 10/19 were designed to
//!    preserve).
//! 2. Is reaped (worktree dir AND `loopr/wk-*` branch both gone) the
//!    instant the Work is driven to a terminal status.
//!
//! The terminalizing step is an explicit `work.override` IPC call (the
//! Phase 18 operator verb), keeping the whole scenario deterministic.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tokio::time::sleep;

use domain::{Plan, PlanId, Work, WorkId, WorkStatus};
use ipc::{MethodName, WorkOverrideResult};
use llm::{LlmClient, LlmError, Message, ToolCall, ToolSchema, Usage};
use loopr::config::Config;
use loopr::daemon::{DaemonHandle, build_context, serve_core};
use loopr::transport::IpcClient;
use store::Store;
use telemetry::digest::process::ProcessSnapshot;
use telemetry::{ProcessId, SessionId};
use tools::SandboxMode;

use common::init_git_repo;

/// A Director LLM that never has anything actionable in this scenario
/// (there is no Implementer/Reviewer round -- the Bundle is pre-seeded
/// straight to `Accepted`), so it always reports "nothing actionable".
/// The test drives every state change itself (cold-boot reconcile
/// requeue, then an explicit `work.override` IPC call), so the Director
/// must never race those assertions with an action of its own.
#[derive(Clone)]
struct DoNothingDirectorLlm;

impl LlmClient for DoNothingDirectorLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: ToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        unreachable!("this scenario never exercises a tool-call LLM turn (decomposer)")
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        Ok((
            r#"[{"action":"done","summary":"nothing actionable"}]"#.to_string(),
            Usage::default(),
        ))
    }
}

/// Create `loopr/plan-<plan_id>` at the target's current HEAD, mirroring
/// `daemon::git::ensure_integration_branch` (not reachable from an
/// external test binary -- that module is `pub(crate)`).
fn create_integration_branch(target: &Path, plan_id: &PlanId) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["branch", &format!("loopr/plan-{plan_id}"), "HEAD"])
        .status()
        .expect("git branch");
    assert!(status.success(), "failed to create integration branch");
}

fn resolve_head(path: &Path) -> String {
    let out = std::process::Command::new("git")
        .current_dir(path)
        .env("LC_ALL", "C")
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse HEAD");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Break-to-prove: pre-seed an `Accepted` Bundle (real on-disk worktree,
/// real `loopr/plan-<id>` integration branch, real
/// `integrator.validation-commands: ["false"]`), let the daemon's own
/// cold-boot reconcile drive it through the REAL integrator into a real
/// `IntegrationFailed`, confirm the Work's warm worktree survives while
/// `Blocked` (retryable, non-terminal), then terminalize it with an
/// explicit operator abandon and confirm both the worktree directory AND
/// its `loopr/wk-*` branch are gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn integration_failed_work_worktree_survives_blocked_then_reaps_on_abandon() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let target = tempdir.path().to_path_buf();
    init_git_repo(&target);
    let sha = resolve_head(&target);

    // --- Seed: Plan (Active) + Work (InReview) + a real warm worktree
    // (retained, exactly as Phase 10 leaves it after a review round) +
    // an Accepted Bundle referencing that worktree's branch. ---
    let plan_id;
    let work_id;
    let wt_path;
    let wt_branch;
    {
        let store = Store::open(&target).await.expect("Store::open");

        let plan = Plan::new("force-integration-failed".to_string());
        plan_id = plan.id.clone();
        create_integration_branch(&target, &plan_id);

        let mut work = Work::new(plan_id.clone(), "Add VERSION.md".to_string());
        work.status = WorkStatus::InReview;
        work_id = work.id.clone();

        let wt_root = target.join(".loopr").join("worktrees");
        let wt = worktree::Worktree::create(&target, &wt_root, work_id.clone(), &sha).expect("Worktree::create");
        wt_path = wt.path().to_path_buf();
        wt_branch = wt.branch().to_string();
        // Retain: the Phase 10 warm-cache contract keeps this worktree on
        // disk past the Implementer's own return, exactly what a real
        // `spawn_implementer_for_work` -> InReview path does.
        wt.retain();

        let mut bundle = domain::Bundle::new(work_id.clone(), wt_branch.clone(), vec!["seeded".to_string()]);
        bundle.head_commit = Some(sha.clone());
        bundle.status = domain::BundleStatus::Accepted;

        store.plans().create(plan).await.expect("plans().create");
        store.works().create(work).await.expect("works().create");
        store.bundles().create(bundle).await.expect("bundles().create");
        store.close().await.expect("store.close");
    }

    // --- Boot the daemon. Cold-boot reconcile's `requeue_bundles` sees
    // the Accepted Bundle and spawns the REAL integrator against it. ---
    let session_id = SessionId::parse("20260422-000000").expect("SessionId::parse");
    let process_id = ProcessId::parse("pc-reap01").expect("ProcessId::parse");
    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;
    config.agents.director.poll_interval_secs = 0;
    config.agents.director.idle_interval_secs = 0;
    // Phase 12: force a real post-merge validation failure so the Bundle
    // lands `IntegrationFailed` for real, not via a test-only status
    // shortcut.
    config.integrator.require_validation = true;
    config.integrator.validation_commands = vec!["false".to_string()];
    let snapshot = Arc::new(std::sync::Mutex::new(ProcessSnapshot::new("test-reap-model")));

    let ctx = build_context(
        target.clone(),
        session_id,
        "-test-reap".to_string(),
        process_id,
        0,
        DoNothingDirectorLlm,
        config,
        false,
        snapshot,
    )
    .await
    .expect("build_context");
    let handle = DaemonHandle::from_context(&ctx);
    let socket = handle.socket_path().to_path_buf();
    let task = tokio::spawn(async move { serve_core(ctx).await });
    wait_for_socket(&socket).await;

    // 1. The pre-seeded Bundle's real IntegrationFailed drives the Work
    // to Blocked.
    wait_for_work_status(&target, &work_id, WorkStatus::Blocked, Duration::from_secs(20))
        .await
        .expect("Work should reach Blocked after a real IntegrationFailed");

    // 2. In-flight retention: the Phase 10 warm worktree must still be on
    // disk while the Work sits Blocked (retryable, non-terminal) -- the
    // exact evidence an operator inspects for an environment failure.
    assert!(wt_path.exists(), "worktree dir must exist while Blocked");
    assert!(branch_exists(&target, &wt_branch), "branch must exist while Blocked");

    // 3. Operator terminalizes the Work (deterministic).
    let mut client = IpcClient::connect(&socket, loopr::transport::ClientTimeouts::default())
        .await
        .expect("connect");
    client.handshake(None).await.expect("handshake");
    let (resp, _events) = client
        .request(
            MethodName::WorkOverride,
            json!({ "work_id": work_id.to_string(), "target_status": "abandoned" }),
        )
        .await
        .expect("work.override request");
    assert!(resp.error.is_none(), "work.override errored: {:?}", resp.error);
    let result: WorkOverrideResult = serde_json::from_value(resp.result.expect("result")).expect("decode");
    assert_eq!(result.work.status, WorkStatus::Abandoned);

    // 4. Break-to-prove: once terminal, both the worktree dir AND its
    // branch are gone -- the Phase 19 reap, not just the record status.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && (wt_path.exists() || branch_exists(&target, &wt_branch)) {
        sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !wt_path.exists(),
        "worktree dir should be reaped once Work is Abandoned"
    );
    assert!(
        !branch_exists(&target, &wt_branch),
        "branch should be reaped once Work is Abandoned"
    );

    handle.shutdown();
    task.await.expect("daemon task join").expect("daemon returned Ok");
}

// ---------------------------------------------------------------------------
// Shared helpers.
// ---------------------------------------------------------------------------

async fn wait_for_socket(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if socket.exists() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket never appeared at {}", socket.display());
}

/// Poll `works.jsonl` until `work_id` reaches `status`, or the deadline
/// elapses.
async fn wait_for_work_status(
    target: &Path,
    work_id: &WorkId,
    status: WorkStatus,
    deadline: Duration,
) -> Result<Work, String> {
    let start = Instant::now();
    while Instant::now().duration_since(start) < deadline {
        if let Some(w) = load_work(target, work_id)
            && w.status == status
        {
            return Ok(w);
        }
        sleep(Duration::from_millis(30)).await;
    }
    let observed = load_work(target, work_id).map(|w| w.status);
    Err(format!(
        "deadline exceeded waiting for Work status {status:?}; observed: {observed:?}"
    ))
}

fn load_work(target: &Path, work_id: &WorkId) -> Option<Work> {
    let path = target.join(".loopr").join("taskstore").join("works.jsonl");
    let bytes = std::fs::read(&path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut latest: Option<Work> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<Work>(line)
            && &record.id == work_id
        {
            latest = Some(record);
        }
    }
    latest
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .env("LC_ALL", "C")
        .args(["branch", "--list", branch])
        .output()
        .expect("git branch --list");
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}
