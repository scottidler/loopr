//! Phase 2 (Tier-1 cleanup) integration tests for the daemon-boot
//! corruption gate.
//!
//! `build_context` is exercised directly so we can assert on its
//! return value without booting a full IPC listener. The gate fires
//! after `reconcile` returns and before any listener binds:
//!
//! - corruption present + `accept_corruption = false`  →  `CorruptionGate { count: N }`
//! - corruption present + `accept_corruption = true`   →  `Ok(_)`, warn emitted
//!
//! Reconcile only sweeps the Work and Bundle JSONLs (per Phase 2's
//! design). These tests inject corruption into each.

mod common;

use std::io::Write;
use std::path::Path;

use tempfile::TempDir;

use llm::ScriptedLlm;
use loopr::config::Config;
use loopr::daemon::build_context;
use loopr::error::LooprError;
use store::TASKSTORE_SUBPATH;
use telemetry::{ProcessId, SessionId};
use tools::SandboxMode;

use common::init_git_repo;

fn jsonl_path(target: &Path, collection: &str) -> std::path::PathBuf {
    target.join(TASKSTORE_SUBPATH).join(format!("{collection}.jsonl"))
}

fn append_raw(path: &Path, line: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir parent");
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open jsonl");
    writeln!(f, "{line}").expect("write line");
}

/// Touch the per-collection JSONL with a single corrupt line BEFORE
/// `build_context` runs. `Store::open` accepts an existing or absent
/// taskstore dir; what matters is that reconcile's tolerant pre-pass
/// sees the malformed line on its first read.
fn seed_corrupt_bundle(target: &Path) {
    append_raw(&jsonl_path(target, "bundles"), "{ definitely not json");
}

fn seed_corrupt_work(target: &Path) {
    append_raw(&jsonl_path(target, "works"), "this row is not json");
}

async fn run_build(target: &Path, accept_corruption: bool) -> Result<(), LooprError> {
    let session_id = SessionId::parse("20260422-000000").expect("SessionId::parse");
    let process_id = ProcessId::parse("pc-test01").expect("ProcessId::parse");
    let target_slug = "-test-corruption-gate".to_string();
    let mut config = Config::default();
    config.tools.sandbox = SandboxMode::Off;

    let snapshot = std::sync::Arc::new(std::sync::Mutex::new(telemetry::digest::process::ProcessSnapshot::new(
        "test-stub-model",
    )));
    let _ctx = build_context(
        target.to_path_buf(),
        session_id,
        target_slug,
        process_id,
        0,
        ScriptedLlm::new(),
        config,
        accept_corruption,
        snapshot,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_refuses_boot_when_corrupt_bundle_present() {
    let td = TempDir::new().expect("tempdir");
    init_git_repo(td.path());
    seed_corrupt_bundle(td.path());

    let err = run_build(td.path(), false).await.expect_err("gate must refuse boot");
    match err {
        LooprError::CorruptionGate { count } => {
            assert!(count >= 1, "expected at least one corrupt record, got {count}");
        }
        other => panic!("expected CorruptionGate, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_refuses_boot_when_corrupt_work_present() {
    let td = TempDir::new().expect("tempdir");
    init_git_repo(td.path());
    seed_corrupt_work(td.path());

    let err = run_build(td.path(), false).await.expect_err("gate must refuse boot");
    match err {
        LooprError::CorruptionGate { count } => {
            assert!(count >= 1, "expected at least one corrupt record, got {count}");
        }
        other => panic!("expected CorruptionGate, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_lets_boot_proceed_with_corrupt_records() {
    let td = TempDir::new().expect("tempdir");
    init_git_repo(td.path());
    seed_corrupt_bundle(td.path());
    seed_corrupt_work(td.path());

    run_build(td.path(), true)
        .await
        .expect("--accept-corruption must allow boot");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_target_boots_without_gate_trip() {
    let td = TempDir::new().expect("tempdir");
    init_git_repo(td.path());

    run_build(td.path(), false)
        .await
        .expect("clean target must boot under default gate posture");
}
