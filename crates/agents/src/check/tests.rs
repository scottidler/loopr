#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tempfile::TempDir;
use tools::{LaneRouter, SandboxMode};

use super::{CheckRunner, ProductionCheckRunner};

fn runner() -> ProductionCheckRunner {
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
    ProductionCheckRunner::new(router, None)
}

#[tokio::test]
async fn true_command_is_green() {
    let dir = TempDir::new().unwrap();
    let r = runner();
    let out = r.run(dir.path(), &["true".to_string()]).await;
    assert_eq!(out.len(), 1);
    assert!(out[0].spawn_error.is_none(), "true should spawn cleanly");
    assert_eq!(out[0].exit_code, 0);
    assert!(!out[0].is_red());
}

#[tokio::test]
async fn false_command_is_red_not_spawn_error() {
    let dir = TempDir::new().unwrap();
    let r = runner();
    let out = r.run(dir.path(), &["false".to_string()]).await;
    assert_eq!(out.len(), 1);
    // A clean spawn with a nonzero exit is a CODE signal, not an env error.
    assert!(
        out[0].spawn_error.is_none(),
        "false spawns cleanly; nonzero exit is code, not env"
    );
    assert_ne!(out[0].exit_code, 0);
    assert!(out[0].is_red());
}

#[tokio::test]
async fn missing_program_is_spawn_error_not_red() {
    let dir = TempDir::new().unwrap();
    let r = runner();
    let out = r
        .run(dir.path(), &["loopr-definitely-not-a-real-binary-xyzzy".to_string()])
        .await;
    assert_eq!(out.len(), 1);
    // The spawn boundary failed: env problem, NOT a code signal.
    assert!(
        out[0].spawn_error.is_some(),
        "missing program must surface as a spawn error"
    );
    assert!(!out[0].is_red(), "a spawn error is not a red code signal");
}

#[tokio::test]
async fn empty_command_is_spawn_error() {
    let dir = TempDir::new().unwrap();
    let r = runner();
    let out = r.run(dir.path(), &["   ".to_string()]).await;
    assert_eq!(out.len(), 1);
    assert!(out[0].spawn_error.is_some());
}

#[tokio::test]
async fn multiple_commands_run_in_order() {
    let dir = TempDir::new().unwrap();
    let r = runner();
    let out = r.run(dir.path(), &["true".to_string(), "false".to_string()]).await;
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].exit_code, 0);
    assert!(out[1].is_red());
}
