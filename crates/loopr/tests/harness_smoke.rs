//! Phase 4 smoke test for the in-process daemon test harness.
//!
//! Spawns a daemon backed by an empty `ScriptedLlm`, verifies the
//! socket appears, shuts it down immediately. No LLM calls are
//! exercised; the test proves `spawn_test_daemon` + `serve_core` +
//! `TestDaemon::shutdown` compose without deadlocking.

mod common;

use llm::ScriptedLlm;

use common::harness::spawn_test_daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn harness_starts_and_shuts_down_cleanly() {
    let stub = ScriptedLlm::new();
    let daemon = spawn_test_daemon(stub.clone()).await.expect("spawn_test_daemon");

    assert!(
        daemon.handle().socket_path().exists(),
        "socket should exist after spawn"
    );

    daemon.shutdown().await.expect("shutdown");

    assert!(
        stub.is_empty(),
        "no LLM calls should have been made; queue remaining: {:?}",
        stub.remaining()
    );
}
