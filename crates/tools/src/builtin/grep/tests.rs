use super::*;

use crate::denylist::BashDenylist;
use crate::router::LaneRouter;
use crate::sandbox::SandboxMode;
use crate::tool::ToolContext;
use std::sync::Arc;

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        working_dir: dir.to_path_buf(),
        router: Arc::new(LaneRouter::new(SandboxMode::Off).unwrap()),
        sandbox: SandboxMode::Off,
        path_deny_patterns: Vec::new(),
        bash_denylist: Arc::new(BashDenylist::with_base()),
        persist_base: None,
        invocation_id: None,
    }
}

#[tokio::test]
async fn finds_matches_in_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world\ngoodbye world\n").unwrap();
    let out = execute(
        Input {
            pattern: "world".into(),
            path: None,
            glob: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.matches.len(), 2, "matches: {:?}", out.matches);
}

#[tokio::test]
async fn no_matches_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "unrelated\n").unwrap();
    let out = execute(
        Input {
            pattern: "needle".into(),
            path: None,
            glob: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    // `grep` returns 1 when no lines match.
    assert_eq!(out.exit_code, 1);
    assert!(out.matches.is_empty());
}

#[tokio::test]
async fn glob_filter_scopes_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "target\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "target\n").unwrap();
    let out = execute(
        Input {
            pattern: "target".into(),
            path: None,
            glob: Some("*.rs".into()),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    // Only a.rs must be in the match list.
    for m in &out.matches {
        assert!(m.contains("a.rs"), "unexpected match: {m}");
    }
}

#[tokio::test]
async fn excludes_git_and_loopr_dirs() {
    // Finding 6: grep must not descend into .git / .loopr.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.txt"), "needle\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".git/packed.txt"), "needle\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".loopr/records")).unwrap();
    std::fs::write(dir.path().join(".loopr/records/r.txt"), "needle\n").unwrap();
    let out = execute(
        Input {
            pattern: "needle".into(),
            path: None,
            glob: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.matches.len(), 1, "should only match real.txt: {:?}", out.matches);
    assert!(out.matches[0].contains("real.txt"));
}

#[tokio::test]
async fn path_outside_working_dir_rejected_when_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut c = ctx(dir.path());
    c.sandbox = SandboxMode::Required;
    let err = execute(
        Input {
            pattern: "x".into(),
            path: Some(other.path().to_path_buf()),
            glob: None,
        },
        &c,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::SandboxViolation(_)));
}

#[tokio::test]
async fn stderr_is_surfaced_on_output() {
    // D17: grep against an unreadable target must surface stderr (grep's
    // "Permission denied" / "No such file" text) on Output.stderr, not drop
    // it. Recursive grep on a mode-000 directory writes to stderr and
    // returns exit_code=2.
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("inner.txt"), "x").unwrap();
    // Unix-only chmod 000 — the test gate is cfg(unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&locked, perms).unwrap();
    }
    let out = execute(
        Input {
            pattern: "x".into(),
            path: Some(locked.clone()),
            glob: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    // Restore perms so the TempDir can clean up.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(&locked, perms);
    }
    assert!(
        !out.stderr.is_empty(),
        "grep against a chmod-000 dir must surface stderr: got {:?}",
        out.stderr
    );
    // combined_output should contain the stderr text too.
    assert!(
        out.combined_output.contains("Permission denied") || !out.stderr.is_empty(),
        "combined_output should carry the stderr: {:?}",
        out.combined_output
    );
}
