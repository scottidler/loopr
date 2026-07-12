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
async fn spawn_backgrounded_pipe_holder_does_not_hang_drain() {
    // Finding 7: the foreground command exits immediately (echo) but a
    // backgrounded `sleep` inherits and holds the stdout write end open.
    // Without the bounded drain, the reader's read_until would block on the
    // open pipe for the full 20s. The bound (DRAIN_TIMEOUT_SECS) must kill
    // the group and return well before that.
    let cmd = sh_command("sleep 20 & echo done", &std::env::temp_dir());
    let started = Instant::now();
    let result = spawn_with_process_group(cmd, 30, KillStrategy::Pgid, PersistConfig::default())
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "drain hung on backgrounded pipe holder: {elapsed:?}"
    );
    assert!(
        result.stdout.contains("done"),
        "foreground output must still be captured: {:?}",
        result.stdout
    );
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

#[test]
fn truncate_inline_multibyte_overflow_does_not_panic() {
    // Phase 1 remediation: a >32 KB string of multibyte chars whose
    // byte at MAX_INLINE_OUTPUT lands mid-codepoint must not panic
    // `String::truncate`. '€' is 3 bytes; 32_000 % 3 != 0 guarantees
    // the cap falls mid-char.
    let mut s = "€".repeat(MAX_INLINE_OUTPUT); // 3 * 32_000 bytes
    truncate_inline(&mut s, None);
    assert!(s.len() <= MAX_INLINE_OUTPUT + 100);
    assert!(s.ends_with("[truncated]"));
    // The truncated prefix must remain valid UTF-8 (no broken codepoint).
    assert!(std::str::from_utf8(s.as_bytes()).is_ok());
}

/// `kill(pid, 0)`: `0` => the process (or a zombie) still exists; `-1` with
/// `ESRCH` => it is gone. Used to assert the reaped subprocess tree is dead.
fn pid_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_reaps_subprocess_tree_no_surviving_pid() {
    // Panel must-fix #5: aborting a spawn future mid-tool-call must reap the
    // ENTIRE subprocess tree, not just the direct child. Break-to-prove:
    // without `kill_on_drop(true)` + the process-group reaper, the dropped
    // tokio `Child` is merely detached and the grandchild `sleep` survives
    // the abort, leaving its pid alive here.
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let pidfile_str = pidfile.display().to_string();

    // `sleep 300 &` is a GRANDCHILD (child of the spawned sh), living in the
    // sh's `setsid()` process group; its pid is recorded to the pidfile. The
    // foreground `wait` parks the spawn future on `child.wait().await` so the
    // abort lands mid-flight, exercising the drop-path reaper.
    let script = format!("sleep 300 & echo $! > '{pidfile_str}'; wait");
    let cmd = sh_command(&script, dir.path());

    let handle = tokio::spawn(async move {
        let _ = spawn_with_process_group(cmd, 60, KillStrategy::Pgid, PersistConfig::default()).await;
    });

    // Wait for the grandchild to record its pid (bounded so a hang fails fast).
    let mut pid = None;
    for _ in 0..200 {
        if let Ok(s) = std::fs::read_to_string(&pidfile)
            && let Ok(p) = s.trim().parse::<i32>()
        {
            pid = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = pid.expect("grandchild never recorded its pid");
    assert!(pid_alive(pid), "grandchild should be alive before abort");

    // Abort the task while it is parked on `child.wait()`; the reaper must
    // SIGKILL the whole process group (leader sh + grandchild sleep).
    handle.abort();
    let _ = handle.await;

    // Poll for the pid to vanish (SIGKILL delivery + init reaping the orphan).
    let mut gone = false;
    for _ in 0..250 {
        if !pid_alive(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        gone,
        "subprocess tree survived abort: grandchild pid {pid} still alive \
         (kill_on_drop + process-group reap failed)"
    );
}
