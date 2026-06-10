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
async fn replaces_unique_match() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.rs");
    std::fs::write(&p, "fn one() {}\nfn two() {}\n").unwrap();
    let out = execute(
        Input {
            path: p.clone(),
            old_string: "fn one()".into(),
            new_string: "fn uno()".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.replacements, 1);
    let after = std::fs::read_to_string(&p).unwrap();
    assert!(after.contains("fn uno()"));
    assert!(!after.contains("fn one()"));
    assert!(after.contains("fn two()"));
}

#[tokio::test]
async fn errors_on_zero_matches() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.rs");
    std::fs::write(&p, "fn one() {}\n").unwrap();
    let err = execute(
        Input {
            path: p,
            old_string: "fn missing()".into(),
            new_string: "fn x()".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::NoMatch(_)), "err: {err:?}");
}

#[tokio::test]
async fn errors_on_multiple_matches() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.rs");
    std::fs::write(&p, "x\nx\nx\n").unwrap();
    let err = execute(
        Input {
            path: p,
            old_string: "x".into(),
            new_string: "y".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::MultipleMatches { count: 3, .. }), "err: {err:?}");
}

#[tokio::test]
async fn rejects_non_utf8_file() {
    // Finding 5: a non-UTF8 file must be rejected, not lossily round-tripped.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("blob.bin");
    std::fs::write(&p, [0xff, 0xfe, 0x00, 0x80]).unwrap();
    let err = execute(
        Input {
            path: p,
            old_string: "x".into(),
            new_string: "y".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::NonUtf8(_)), "err: {err:?}");
}

#[tokio::test]
async fn no_temp_files_left_after_edit() {
    // Finding 5: temp-then-rename must clean up after itself.
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("f.rs");
    std::fs::write(&p, "fn one() {}\n").unwrap();
    execute(
        Input {
            path: p.clone(),
            old_string: "one".into(),
            new_string: "uno".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    let entries: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["f.rs".to_string()], "stray temp file: {entries:?}");
    assert!(std::fs::read_to_string(&p).unwrap().contains("fn uno()"));
}

#[tokio::test]
async fn rejects_escape_when_sandboxed() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let victim = other.path().join("victim.txt");
    std::fs::write(&victim, "x").unwrap();
    let mut c = ctx(dir.path());
    c.sandbox = SandboxMode::Required;
    let err = execute(
        Input {
            path: victim,
            old_string: "x".into(),
            new_string: "y".into(),
        },
        &c,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::SandboxViolation(_)));
}
