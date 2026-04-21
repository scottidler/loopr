use super::*;

use crate::shell::sh_command;

#[tokio::test]
async fn spawn_echo() {
    let cmd = sh_command("echo hello", &std::env::temp_dir());
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout.trim(), "hello");
    assert!(!result.timed_out);
    assert!(!result.truncated);
    assert!(result.persisted_output_path.is_none());
}

#[tokio::test]
async fn spawn_stderr_captured_separately() {
    let cmd = sh_command("echo err >&2", &std::env::temp_dir());
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "");
    assert_eq!(result.stderr.trim(), "err");
    assert_eq!(result.combined_output.trim(), "err");
}

#[tokio::test]
async fn spawn_stdout_and_stderr_both_land_in_combined() {
    let cmd = sh_command("echo out; echo err >&2", &std::env::temp_dir());
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(
        result.combined_output.contains("out"),
        "combined must contain stdout line: {}",
        result.combined_output
    );
    assert!(
        result.combined_output.contains("err"),
        "combined must contain stderr line: {}",
        result.combined_output
    );
}

#[tokio::test]
async fn spawn_non_zero_exit_preserved() {
    let cmd = sh_command("exit 42", &std::env::temp_dir());
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 42);
}

#[tokio::test]
async fn spawn_timeout_kills_pgid_strategy() {
    let cmd = sh_command("sleep 30 & sleep 30 & wait", &std::env::temp_dir());
    let result = spawn_with_process_group(cmd, 1, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert!(result.timed_out, "sleep should have been killed by timeout");
    assert!(result.stderr.contains("timed out"));
}

#[tokio::test]
async fn spawn_non_utf8_output_survives() {
    // Emit genuine raw bytes via python3 (printf \xNN is inconsistent across
    // sh/bash/coreutils). This is D15's regression test: the lines()-based
    // reader terminated mid-stream on invalid UTF-8; the read_until + lossy
    // conversion preserves the full output.
    let cmd = sh_command(
        r#"python3 -c "import sys; sys.stdout.buffer.write(b'before\xff\xfeafter\n')""#,
        &std::env::temp_dir(),
    );
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    // "before" is before the invalid bytes; "after" is after. The lines()-
    // based reader would have dropped "after" entirely.
    assert!(
        result.stdout.contains("before"),
        "stdout missing 'before': {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains("after"),
        "stdout missing 'after' - D15 regression (lines() reader terminated on invalid UTF-8): {:?}",
        result.stdout
    );
    assert!(
        result.stdout.contains('\u{FFFD}'),
        "expected replacement character for invalid bytes: {:?}",
        result.stdout
    );
}

#[tokio::test]
async fn spawn_large_output_persisted() {
    let dir = tempfile::tempdir().unwrap();
    let id = Uuid::now_v7();
    let cmd = sh_command(
        r#"python3 -c "print('x'*100000)" 2>/dev/null || yes x | head -c 100000"#,
        &std::env::temp_dir(),
    );
    let persist = PersistConfig {
        base: Some(dir.path()),
        invocation_id: Some(id),
    };
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, persist)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(result.truncated, "100_000 bytes must overflow 32_000 cap");
    let persisted = result.persisted_output_path.unwrap();
    assert_eq!(persisted, dir.path().join(format!("{id}.log")));
    assert!(persisted.exists(), "persist file must exist at {persisted:?}");
    let persisted_bytes = std::fs::read(&persisted).unwrap();
    assert!(
        persisted_bytes.len() >= 100_000,
        "persist file must contain the full 100_000-byte output, got {}",
        persisted_bytes.len()
    );
}

#[tokio::test]
async fn spawn_working_dir_applied() {
    let dir = std::env::temp_dir();
    let cmd = sh_command("pwd", &dir);
    let result = spawn_with_process_group(cmd, 10, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let expected = std::fs::canonicalize(&dir).unwrap();
    let actual = std::fs::canonicalize(result.stdout.trim()).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn truncate_inline_with_persist_path_appends_reference() {
    let mut s = "a\n".repeat(MAX_INLINE_OUTPUT); // well over cap
    let p = PathBuf::from("/tmp/foo.log");
    truncate_inline(&mut s, Some(&p));
    assert!(s.contains("/tmp/foo.log"));
    assert!(s.contains("truncated"));
    assert!(s.len() <= MAX_INLINE_OUTPUT + 100);
}

#[test]
fn truncate_inline_without_persist_path_marks_truncated() {
    let mut s = "a\n".repeat(MAX_INLINE_OUTPUT);
    truncate_inline(&mut s, None);
    assert!(s.ends_with("[truncated]"));
}

#[test]
fn truncate_inline_below_cap_is_noop() {
    let mut s = "short\n".to_string();
    truncate_inline(&mut s, None);
    assert_eq!(s, "short\n");
}
