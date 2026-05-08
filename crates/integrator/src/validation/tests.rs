#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tempfile::TempDir;

use super::run_validation;

fn temp_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn empty_commands_succeeds() {
    let dir = temp_dir();
    run_validation(&[], Duration::from_secs(5), dir.path()).await.unwrap();
}

#[tokio::test]
async fn passing_command_succeeds() {
    let dir = temp_dir();
    run_validation(&["true".to_string()], Duration::from_secs(5), dir.path())
        .await
        .unwrap();
}

#[tokio::test]
async fn failing_command_returns_error() {
    let dir = temp_dir();
    let err = run_validation(&["false".to_string()], Duration::from_secs(5), dir.path())
        .await
        .unwrap_err();
    assert_eq!(err.command, "false");
    assert_eq!(err.exit_code, Some(1));
}

#[tokio::test]
async fn failing_command_captures_output() {
    let dir = temp_dir();
    let err = run_validation(
        &["echo 'hello failure' && exit 1".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert!(err.log.contains("hello failure"), "log was: {}", err.log);
    assert_eq!(err.exit_code, Some(1));
}

#[tokio::test]
async fn timeout_returns_error_with_no_exit_code() {
    let dir = temp_dir();
    let err = run_validation(&["sleep 60".to_string()], Duration::from_millis(200), dir.path())
        .await
        .unwrap_err();
    assert_eq!(err.command, "sleep 60");
    assert!(err.exit_code.is_none());
    assert!(err.log.contains("timed out"), "log was: {}", err.log);
}

#[tokio::test]
async fn first_failure_stops_sequence() {
    let dir = temp_dir();
    let err = run_validation(
        &["false".to_string(), "echo 'should not run'".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.command, "false");
}

#[tokio::test]
async fn multiple_passing_commands_all_run() {
    let dir = temp_dir();
    run_validation(
        &["true".to_string(), "true".to_string(), "true".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap();
}
