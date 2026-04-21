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
        path_deny_patterns: vec![".env".into()],
        bash_denylist: Arc::new(BashDenylist::with_base()),
        persist_base: None,
        invocation_id: None,
    }
}

#[tokio::test]
async fn writes_and_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nested/deep/hello.txt");
    let out = execute(
        Input {
            path: p.clone(),
            content: "hi there".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.bytes_written, 8);
    assert!(p.exists(), "file must exist after write");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi there");
}

#[tokio::test]
async fn overwrites_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.txt");
    std::fs::write(&p, "before").unwrap();
    let _ = execute(
        Input {
            path: p.clone(),
            content: "after".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "after");
}

#[tokio::test]
async fn rejects_deny_pattern_path() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(".env");
    let err = execute(
        Input {
            path: p,
            content: "SECRET=1".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::PathDenied(_)));
}

#[tokio::test]
async fn rejects_escape_when_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let victim = other.path().join("victim.txt");
    let mut c = ctx(dir.path());
    c.sandbox = SandboxMode::Required;
    let err = execute(
        Input {
            path: victim,
            content: "evil".into(),
        },
        &c,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::SandboxViolation(_)));
}
