//! Stage 7 daemon-shutdown regression guard.
//!
//! What this file guards:
//! - `drain_implementer_tasks` is reached during daemon shutdown without
//!   deadlocking against an empty JoinSet.
//! - The DaemonContext fields (`context_builder`, `implementer_config`,
//!   `worktree_cleanup_policy`, `implementer_tasks`) are initialized at
//!   daemon startup without panic.
//!
//! What this file does NOT guard: the Stage 7 implementer-spawn path
//! itself. The full E2E exit criterion — "on a toy target repo, a Work
//! item produces a Bundle whose commit diff shows real file edits" —
//! requires a live or mocked Anthropic backend to script both the
//! decomposer and the implementer responses. That deep-E2E is deferred
//! to Stage 9's first-gate run. See the deletion note below for why the
//! earlier `plan_create_exercises_stage_7_spawn_path_without_crash`
//! test was removed.

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

    // Phase 12 (validation-by-default): `integrator.require-validation`
    // now defaults to `true`, so an empty `validation-commands` list
    // refuses daemon startup. This file tests the Stage 7 shutdown-drain
    // path, not Integrator validation; opt out explicitly.
    let loopr_dir = target.join(".loopr");
    fs::create_dir_all(&loopr_dir).unwrap();
    fs::write(
        loopr_dir.join("config.yml"),
        "integrator:\n  require-validation: false\n",
    )
    .unwrap();
}

// `stop_daemon` lives in `common::stop_daemon_for` so all daemon-spawning
// integration tests share the same panic-safe `DaemonAutoStop` guard.

// `plan_create_exercises_stage_7_spawn_path_without_crash` was deleted
// 2026-05-25 after architectural review (see
// `docs/design/2026-05-25-stage-7-wiring-test-no-network.md`). The test
// claimed to guard the Stage 7 implementer spawn path, but verification
// showed the decomposer's no-API-key path returns
// `Err(DecomposerError::LlmFailed)` (see `crates/decomposer/src/decompose.rs:199`),
// which `handle_plan_create` catches and short-circuits at
// `crates/loopr/src/transport/handler.rs:229-236` — the `Ok(works)`
// branch containing the spawn loop is never reached. The test was a
// placebo. The Err-branch coverage it actually exercised is already
// provided by `crates/loopr/src/transport/handler/tests.rs::plan_create_with_failing_llm_still_persists_plan_and_leaves_works_empty`,
// and the fork-daemon + drain lifecycle is covered by the sibling
// `plan_create_daemon_shutdown_drains_implementer_tasks_cleanly` below.
// The real Stage 7 spawn-path coverage is deferred to Stage 9's
// first-gate run, exactly as the deleted test's own docstring admitted.

#[test]
fn plan_create_daemon_shutdown_drains_implementer_tasks_cleanly() {
    let td = TempDir::new().unwrap();
    let target = td.path();
    init_target(target);

    let _stop = DaemonAutoStop::for_target(target);

    // Auto-fork the daemon via a fast, non-LLM client request. The
    // earlier shape used `loopr plan create` for this, but plan.create
    // routes through the decomposer (real LLM call without an API
    // key takes 7-15s before the deterministic fallback), which
    // intermittently exceeded the client's 10s `client-request-secs`
    // cap. `loopr plans` used to be the cheapest auto-fork trigger, but
    // Phase 16 of `docs/design/2026-07-11-verified-swarm.md` made read
    // verbs report "no daemon" instead of auto-forking; `daemon start`
    // is now the cheapest explicit fork.
    loopr()
        .args(["-C", target.to_str().unwrap(), "daemon", "start"])
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
