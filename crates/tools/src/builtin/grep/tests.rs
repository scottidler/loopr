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
