#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tempfile::TempDir;

use super::{cap_head_tail, run_validation};

fn temp_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn cap_head_tail_returns_full_when_under_cap() {
    let s = cap_head_tail(b"short output", 64);
    assert_eq!(s, "short output");
}

#[test]
fn cap_head_tail_keeps_head_and_tail_when_oversized() {
    // 200 'a' bytes then a 20-byte failure marker at the very end.
    let mut bytes = vec![b'a'; 200];
    bytes.extend_from_slice(b"FAILURE_AT_THE_TAIL!");
    let capped = cap_head_tail(&bytes, 40);
    // Head-only truncation would have dropped the tail marker entirely;
    // head+tail keeps it.
    assert!(capped.contains("FAILURE_AT_THE_TAIL!"), "tail must survive: {capped}");
    assert!(capped.contains("bytes omitted"), "elision marker present: {capped}");
    assert!(capped.starts_with("aaaa"), "head retained: {capped}");
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
