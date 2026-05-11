//! Stage 7 wiring regression guard.
//!
//! The Stage 7 design doc's full E2E exit criterion — "on a toy target repo,
//! a Work item produces a Bundle whose commit diff shows real file edits" —
//! requires a live or mocked Anthropic backend to script both the decomposer
//! and the implementer responses. That deep-E2E is deferred to Stage 9's
//! first-gate run.
//!
//! What THIS test guards:
//! - `handle_plan_create` invokes the Stage 7 spawn path without crashing
//!   the daemon, even when the decomposer returns no Works (the expected
//!   outcome with no API key set).
//! - `drain_implementer_tasks` is reached during daemon shutdown without
//!   deadlocking against an empty JoinSet.
//! - The new DaemonContext fields (`context_builder`, `implementer_config`,
//!   `worktree_cleanup_policy`, `implementer_tasks`) are initialized at
//!   daemon startup without panic.
//!
//! If any of those regress, this test fails before any E2E has a chance to.

#![allow(clippy::unwrap_used)]

mod common;

use std::fs;
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use tempfile::TempDir;

use common::{DaemonAutoStop, stop_daemon_for};

fn loopr() -> Command {
    Command::cargo_bin("loopr").unwrap()
}

/// Init a minimal git repo so worktree creation (if the Stage 7 spawn path
/// gets far enough) has a valid base SHA to point at. Without `git init`
/// the spawn path short-circuits at `rev_parse_head`.
fn init_target(target: &std::path::Path) {
    let run = |args: &[&str]| {
        let status = StdCommand::new("git")
            .arg("-C")
            .arg(target)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "commit.gpgsign", "false"]);
    fs::write(target.join("README.md"), "seed\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "seed", "--no-gpg-sign"]);
}

// `stop_daemon` lives in `common::stop_daemon_for` so all daemon-spawning
// integration tests share the same panic-safe `DaemonAutoStop` guard.

#[test]
fn plan_create_exercises_stage_7_spawn_path_without_crash() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    // Panic-safe daemon cleanup: even if the `assert().success()` below
    // unwinds (the common failure path), Drop runs and SIGTERMs the
    // auto-forked daemon. Without this guard the daemon is reparented
    // to init and accumulates as an orphan across test runs.
    let _stop = DaemonAutoStop::for_target(target);

    // Run `loopr plan "..."`; this auto-forks a daemon, persists a Plan,
    // runs the decomposer (which either yields Works or errors out with no
    // API key), and for any yielded Work invokes the Stage 7 spawn path.
    loopr()
        .args(["-C", target.to_str().unwrap(), "plan", "create", "toy stage 7 goal"])
        .assert()
        .success();

    // Shutdown the daemon; exercises drain_implementer_tasks on whatever
    // implementer tasks are in-flight (zero or more). (The guard above
    // would also fire on success, but ordering matters for the
    // `taskstore` assertions below: stop_daemon_for blocks until the
    // daemon has flushed its final writes.)
    stop_daemon_for(target);

    // Plan was persisted even if decompose failed.
    let plans_jsonl = target.join(".loopr").join("taskstore").join("plans.jsonl");
    assert!(plans_jsonl.is_file(), "plans.jsonl exists");
    let plans_body = fs::read_to_string(&plans_jsonl).unwrap();
    assert!(
        plans_body.lines().any(|l| l.contains("toy stage 7 goal")),
        "plans.jsonl contains the goal text: {plans_body}"
    );

    // .loopr/taskstore/ is the committed-truth directory. bundles.jsonl may
    // or may not exist depending on whether any Work ran to completion;
    // both states are valid Stage-7 outcomes for this degraded-test
    // scenario (no LLM key). The test passes as long as the daemon did
    // not crash and plans persisted.
    let taskstore = target.join(".loopr").join("taskstore");
    assert!(taskstore.is_dir(), "taskstore dir exists");
}

#[test]
fn plan_create_daemon_shutdown_drains_implementer_tasks_cleanly() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    let _stop = DaemonAutoStop::for_target(target);

    // One plan, then immediate shutdown. Even with zero in-flight
    // implementer tasks, the drain_implementer_tasks step must handle
    // the empty-JoinSet path without deadlock or timeout.
    loopr()
        .args(["-C", target.to_str().unwrap(), "plan", "create", "drain-test"])
        .assert()
        .success();

    let start = Instant::now();
    stop_daemon_for(target);
    let elapsed = start.elapsed();

    // The empty-drain path should return immediately. If we hit the
    // IMPLEMENTER_DRAIN_TIMEOUT_SECS (30s) soft cap the drain is misbehaving.
    assert!(
        elapsed < Duration::from_secs(15),
        "daemon shutdown took {elapsed:?}; drain_implementer_tasks may be hanging"
    );
}
