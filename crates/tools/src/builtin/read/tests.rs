use super::*;

use crate::denylist::BashDenylist;
use crate::router::LaneRouter;
use crate::sandbox::SandboxMode;
use crate::tool::ToolContext;
use std::sync::Arc;

fn ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        working_dir: dir.to_path_buf(),
        router: Arc::new(LaneRouter::new(SandboxMode::Off).expect("router")),
        sandbox: SandboxMode::Off,
        path_deny_patterns: vec![".env".into()],
        bash_denylist: Arc::new(BashDenylist::with_base()),
        persist_base: None,
        invocation_id: None,
    }
}

#[tokio::test]
async fn reads_small_file_numbered() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("hello.txt");
    std::fs::write(&p, "a\nb\nc\n").unwrap();
    let out = execute(
        Input {
            path: p,
            offset: None,
            limit: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.lines_shown, 3);
    assert_eq!(out.lines_total, 3);
    assert!(!out.truncated);
    assert!(out.content.contains("     1\ta"), "numbered: {}", out.content);
    assert!(out.content.contains("     2\tb"));
    assert!(out.content.contains("     3\tc"));
}

#[tokio::test]
async fn honors_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("big.txt");
    let content: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&p, content).unwrap();

    let out = execute(
        Input {
            path: p,
            offset: Some(2),
            limit: Some(3),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.lines_shown, 3);
    assert_eq!(out.lines_total, 10);
    assert!(out.truncated);
    assert!(out.content.contains("line3"));
    assert!(out.content.contains("line4"));
    assert!(out.content.contains("line5"));
    assert!(!out.content.contains("line6"), "limit must cut at 3");
}

#[tokio::test]
async fn default_caps_at_500_lines() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("huge.txt");
    let content: String = (1..=600).map(|i| format!("line{i}\n")).collect();
    std::fs::write(&p, content).unwrap();

    let out = execute(
        Input {
            path: p,
            offset: None,
            limit: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.lines_shown, 500);
    assert_eq!(out.lines_total, 600);
    assert!(out.truncated);
}

#[tokio::test]
async fn rejects_path_matching_deny_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join(".env");
    std::fs::write(&secret, "SECRET=1\n").unwrap();
    let err = execute(
        Input {
            path: secret,
            offset: None,
            limit: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::PathDenied(_)), "err: {err:?}");
}

#[tokio::test]
async fn rejects_path_outside_working_dir() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let victim = other.path().join("victim.txt");
    std::fs::write(&victim, "x").unwrap();

    let mut c = ctx(dir.path());
    c.sandbox = SandboxMode::Required;
    let err = execute(
        Input {
            path: victim,
            offset: None,
            limit: None,
        },
        &c,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::SandboxViolation(_)), "err: {err:?}");
}

#[tokio::test]
async fn missing_file_errors_as_io() {
    let dir = tempfile::tempdir().unwrap();
    let ghost = dir.path().join("ghost.txt");
    let err = execute(
        Input {
            path: ghost,
            offset: None,
            limit: None,
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Io { .. } | Error::SandboxViolation(_)));
}

#[test]
fn error_converts_to_tool_error() {
    let e = Error::SandboxViolation("x".into());
    let t: ToolError = e.into();
    assert!(matches!(t, ToolError::SandboxViolation(_)));
}
