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
async fn finds_rs_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "").unwrap();
    std::fs::write(dir.path().join("b.rs"), "").unwrap();
    std::fs::write(dir.path().join("c.txt"), "").unwrap();
    let out = execute(Input { pattern: "*.rs".into() }, &ctx(dir.path()))
        .await
        .unwrap();
    let names: Vec<String> = out.paths.iter().map(|p| p.display().to_string()).collect();
    assert_eq!(names.len(), 2, "names: {names:?}");
    assert!(names.iter().any(|n| n.ends_with("a.rs")));
    assert!(names.iter().any(|n| n.ends_with("b.rs")));
}

#[tokio::test]
async fn recursive_doublestar() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/deep/b.rs"), "").unwrap();
    let out = execute(
        Input {
            pattern: "**/*.rs".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.paths.len(), 2, "paths: {:?}", out.paths);
}

#[tokio::test]
async fn doublestar_does_not_descend_dotdirs() {
    // Finding 6: `**/*.rs` must not match files inside `.git` / `.loopr`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/real.rs"), "").unwrap();
    std::fs::write(dir.path().join(".git/objects/leaked.rs"), "").unwrap();
    let out = execute(
        Input {
            pattern: "**/*.rs".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    let names: Vec<String> = out.paths.iter().map(|p| p.display().to_string()).collect();
    assert_eq!(names.len(), 1, "should not descend .git: {names:?}");
    assert!(names[0].ends_with("real.rs"));
}

#[tokio::test]
async fn literal_dotfile_pattern_still_matches() {
    // Finding 6: `require_literal_leading_dot` stops `*` wildcards from
    // descending dotdirs, but an explicitly-named dotfile is still reachable
    // (an agent that genuinely wants `.env` spells the dot).
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "").unwrap();
    let out = execute(Input { pattern: ".env".into() }, &ctx(dir.path()))
        .await
        .unwrap();
    let names: Vec<String> = out.paths.iter().map(|p| p.display().to_string()).collect();
    assert!(names.iter().any(|n| n.ends_with(".env")), "names: {names:?}");
}

#[tokio::test]
async fn invalid_pattern_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let err = execute(
        Input {
            pattern: "[unclosed".into(),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::InvalidPattern(_)), "err: {err:?}");
}

#[tokio::test]
async fn strips_working_dir_prefix() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();
    let out = execute(Input { pattern: "*.rs".into() }, &ctx(dir.path()))
        .await
        .unwrap();
    assert_eq!(out.paths.len(), 1);
    let rel = out.paths[0].display().to_string();
    assert_eq!(rel, "foo.rs", "expected relative path, got: {rel}");
}
