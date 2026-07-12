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
    let outcomes = run_validation(&[], Duration::from_secs(5), dir.path()).await.unwrap();
    assert!(outcomes.is_empty());
}

#[tokio::test]
async fn passing_command_succeeds() {
    let dir = temp_dir();
    let outcomes = run_validation(&["true".to_string()], Duration::from_secs(5), dir.path())
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].command, "true");
    assert_eq!(outcomes[0].exit_code, 0);
    // Evidence is captured on the green path too (Phase 12: "success and
    // failure both").
    assert!(!outcomes[0].output_digest.is_empty());
}

#[tokio::test]
async fn failing_command_returns_error() {
    let dir = temp_dir();
    let failure = run_validation(&["false".to_string()], Duration::from_secs(5), dir.path())
        .await
        .unwrap_err();
    assert_eq!(failure.error.command, "false");
    assert_eq!(failure.error.exit_code, Some(1));
    // The failing command still produced a CommandOutcome (evidence for
    // the red run), not just a bare error.
    assert_eq!(failure.outcomes.len(), 1);
    assert_eq!(failure.outcomes[0].exit_code, 1);
}

#[tokio::test]
async fn failing_command_captures_output() {
    let dir = temp_dir();
    let failure = run_validation(
        &["echo 'hello failure' && exit 1".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert!(
        failure.error.log.contains("hello failure"),
        "log was: {}",
        failure.error.log
    );
    assert_eq!(failure.error.exit_code, Some(1));
    assert!(
        failure.outcomes[0].output_excerpt.contains("hello failure"),
        "outcome excerpt was: {}",
        failure.outcomes[0].output_excerpt
    );
}

#[tokio::test]
async fn timeout_returns_error_with_no_exit_code() {
    let dir = temp_dir();
    let failure = run_validation(&["sleep 60".to_string()], Duration::from_millis(200), dir.path())
        .await
        .unwrap_err();
    assert_eq!(failure.error.command, "sleep 60");
    assert!(failure.error.exit_code.is_none());
    assert!(
        failure.error.log.contains("timed out"),
        "log was: {}",
        failure.error.log
    );
    // A timeout never reaches an exit code: no CommandOutcome evidence,
    // matching the Reviewer's spawn-error precedent (environment problem,
    // not a check outcome).
    assert!(failure.outcomes.is_empty());
}

#[tokio::test]
async fn first_failure_stops_sequence() {
    let dir = temp_dir();
    let failure = run_validation(
        &["false".to_string(), "echo 'should not run'".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.error.command, "false");
    assert_eq!(failure.outcomes.len(), 1, "only the failing command produced evidence");
}

#[tokio::test]
async fn multiple_passing_commands_all_run() {
    let dir = temp_dir();
    let outcomes = run_validation(
        &["true".to_string(), "true".to_string(), "true".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 3, "every command produced a CommandOutcome");
}

#[tokio::test]
async fn second_command_failure_preserves_first_commands_outcome() {
    let dir = temp_dir();
    let failure = run_validation(
        &["true".to_string(), "false".to_string()],
        Duration::from_secs(5),
        dir.path(),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.error.command, "false");
    assert_eq!(failure.outcomes.len(), 2, "the passing command's evidence is kept");
    assert_eq!(failure.outcomes[0].command, "true");
    assert_eq!(failure.outcomes[0].exit_code, 0);
    assert_eq!(failure.outcomes[1].command, "false");
    assert_eq!(failure.outcomes[1].exit_code, 1);
}
