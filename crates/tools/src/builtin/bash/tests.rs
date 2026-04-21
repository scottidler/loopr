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
async fn echo_produces_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let out = execute(
        Input {
            command: "echo hello".into(),
            timeout_secs: Some(5),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert_eq!(out.stdout.trim(), "hello");
    assert!(!out.timed_out);
}

#[tokio::test]
async fn exit_code_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let out = execute(
        Input {
            command: "exit 7".into(),
            timeout_secs: Some(5),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 7);
}

#[tokio::test]
async fn denylist_blocks_rm_rf() {
    let dir = tempfile::tempdir().unwrap();
    let err = execute(
        Input {
            command: "rm -rf /".into(),
            timeout_secs: Some(5),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    match err {
        ToolError::BashDenied { reason } => assert_eq!(reason, "deletes root filesystem"),
        other => panic!("expected BashDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn denylist_blocks_pipe_to_sh() {
    let dir = tempfile::tempdir().unwrap();
    let err = execute(
        Input {
            command: "echo foo | sh".into(),
            timeout_secs: Some(5),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::BashDenied { .. }));
}

#[tokio::test]
async fn cwd_changes_within_one_call_work() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = execute(
        Input {
            command: format!("cd {} && pwd", sub.display()),
            timeout_secs: Some(5),
        },
        &ctx(dir.path()),
    )
    .await
    .unwrap();
    assert_eq!(out.exit_code, 0);
    // The `pwd` sees the sub directory (possibly canonicalized).
    let expected = sub.canonicalize().unwrap();
    let actual = std::path::PathBuf::from(out.stdout.trim()).canonicalize().unwrap();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn cwd_does_not_persist_across_calls() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let c = ctx(dir.path());
    // First call: cd into sub.
    let _ = execute(
        Input {
            command: format!("cd {}", sub.display()),
            timeout_secs: Some(5),
        },
        &c,
    )
    .await
    .unwrap();
    // Second call: pwd. Should be back at working_dir.
    let out = execute(
        Input {
            command: "pwd".into(),
            timeout_secs: Some(5),
        },
        &c,
    )
    .await
    .unwrap();
    let expected = dir.path().canonicalize().unwrap();
    let actual = std::path::PathBuf::from(out.stdout.trim()).canonicalize().unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn classify_bash_command_routes_cargo_heavy() {
    assert_eq!(classify_bash_command("cargo build"), Lane::Heavy);
    assert_eq!(classify_bash_command("cargo test --release"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_env_prefix_cargo_heavy() {
    // RUST_LOG=debug cargo build -- env prefix must be skipped.
    assert_eq!(classify_bash_command("RUST_LOG=debug cargo build"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_cd_then_cargo_heavy() {
    // cd x && cargo build -- AST walk catches cargo anywhere in the list.
    assert_eq!(classify_bash_command("cd /tmp && cargo build"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_cargo_subcommand_prefix_heavy() {
    // cargo-nextest installed as a symlinked binary, invoked as a word.
    assert_eq!(classify_bash_command("cargo-nextest run"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_npm_heavy() {
    assert_eq!(classify_bash_command("npm install"), Lane::Heavy);
    assert_eq!(classify_bash_command("npx prettier ."), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_pytest_heavy() {
    assert_eq!(classify_bash_command("pytest tests/"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_otto_heavy() {
    assert_eq!(classify_bash_command("otto ci"), Lane::Heavy);
}

#[test]
fn classify_bash_command_routes_pwd_net() {
    assert_eq!(classify_bash_command("pwd"), Lane::Net);
    assert_eq!(classify_bash_command("ls -la"), Lane::Net);
    assert_eq!(classify_bash_command("echo hi"), Lane::Net);
}

#[test]
fn classify_bash_command_routes_path_prefix_to_base() {
    // ./path/to/cargo should still route heavy via normalized head.
    assert_eq!(classify_bash_command("./cargo build"), Lane::Heavy);
    assert_eq!(classify_bash_command("/usr/local/bin/cargo build"), Lane::Heavy);
}

#[test]
fn classify_bash_command_unparseable_defaults_net() {
    // Unparseable fragment defaults to Net (subprocess will fail on parse).
    assert_eq!(classify_bash_command("$(("), Lane::Net);
}

#[test]
fn classify_bash_command_empty_is_net() {
    assert_eq!(classify_bash_command(""), Lane::Net);
}
