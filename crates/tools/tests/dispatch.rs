//! Phase 5 seam tests: drive the tools crate through its public
//! `dispatch(name, json_input, ctx) -> json_output` entrypoint, the way the
//! agent crate will. Catches serialization regressions that per-builtin unit
//! tests miss (they call `execute` with typed `Input` directly).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

use tools::{
    BashDenylist, DenyEntryConfig, LaneRouter, RouterInitError, SandboxMode, ToolContext, ToolError, ToolsConfig,
    all_schemas, dispatch, schema_for,
};

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

fn ctx_with_persist(dir: &std::path::Path, persist: PathBuf, id: Uuid) -> ToolContext {
    let mut c = ctx(dir);
    c.persist_base = Some(persist);
    c.invocation_id = Some(id);
    c
}

#[tokio::test]
async fn read_round_trips_through_dispatch() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("hello.txt");
    std::fs::write(&p, "one\ntwo\nthree\n").unwrap();

    let out = dispatch("read", json!({ "path": p.display().to_string() }), &ctx(td.path()))
        .await
        .unwrap();

    assert_eq!(out["lines_shown"], 3);
    assert_eq!(out["lines_total"], 3);
    assert_eq!(out["truncated"], false);
    let content = out["content"].as_str().unwrap();
    assert!(content.contains("     1\tone"), "content: {content}");
}

#[tokio::test]
async fn write_then_read_through_dispatch() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("written.txt");

    let write_out = dispatch(
        "write",
        json!({ "path": p.display().to_string(), "content": "payload\n" }),
        &ctx(td.path()),
    )
    .await
    .unwrap();
    assert_eq!(write_out["bytes_written"], 8);

    let read_out = dispatch("read", json!({ "path": p.display().to_string() }), &ctx(td.path()))
        .await
        .unwrap();
    assert!(read_out["content"].as_str().unwrap().contains("payload"));
}

#[tokio::test]
async fn edit_then_read_through_dispatch() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("f.rs");
    std::fs::write(&p, "fn alpha() {}\nfn beta() {}\n").unwrap();

    let edit_out = dispatch(
        "edit",
        json!({
            "path": p.display().to_string(),
            "old_string": "fn alpha()",
            "new_string": "fn gamma()",
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap();
    assert_eq!(edit_out["replacements"], 1);

    let after = std::fs::read_to_string(&p).unwrap();
    assert!(after.contains("fn gamma()"));
    assert!(!after.contains("fn alpha()"));
}

#[tokio::test]
async fn bash_runs_through_dispatch() {
    let td = TempDir::new().unwrap();
    let out = dispatch(
        "bash",
        json!({ "command": "echo hi-bash", "timeout_secs": 5 }),
        &ctx(td.path()),
    )
    .await
    .unwrap();
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"].as_str().unwrap().trim(), "hi-bash");
    assert_eq!(out["timed_out"], false);
}

#[tokio::test]
async fn grep_through_dispatch() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.txt"), "needle\nhay\n").unwrap();

    let out = dispatch("grep", json!({ "pattern": "needle" }), &ctx(td.path()))
        .await
        .unwrap();
    assert_eq!(out["exit_code"], 0);
    let matches = out["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
}

#[tokio::test]
async fn glob_through_dispatch() {
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.rs"), "").unwrap();
    std::fs::write(td.path().join("b.rs"), "").unwrap();
    std::fs::write(td.path().join("c.txt"), "").unwrap();

    let out = dispatch("glob", json!({ "pattern": "*.rs" }), &ctx(td.path()))
        .await
        .unwrap();
    let paths = out["paths"].as_array().unwrap();
    assert_eq!(paths.len(), 2);
}

#[tokio::test]
async fn unknown_tool_is_unknown_tool_error() {
    let td = TempDir::new().unwrap();
    let err = dispatch("nonexistent", json!({}), &ctx(td.path())).await.unwrap_err();
    match err {
        ToolError::UnknownTool(name) => assert_eq!(name, "nonexistent"),
        other => panic!("expected UnknownTool, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_input_is_invalid_input_error() {
    let td = TempDir::new().unwrap();
    // `read` requires a `path`; omit it.
    let err = dispatch("read", json!({}), &ctx(td.path())).await.unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_read_input() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("f.txt");
    std::fs::write(&p, "x").unwrap();
    let err = dispatch(
        "read",
        json!({
            "path": p.display().to_string(),
            "unexpected_field": 42,
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_write_input() {
    let td = TempDir::new().unwrap();
    let err = dispatch(
        "write",
        json!({
            "path": td.path().join("x.txt").display().to_string(),
            "content": "hi",
            "bogus": true,
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_edit_input() {
    let td = TempDir::new().unwrap();
    let p = td.path().join("x.txt");
    std::fs::write(&p, "a").unwrap();
    let err = dispatch(
        "edit",
        json!({
            "path": p.display().to_string(),
            "old_string": "a",
            "new_string": "b",
            "rogue-field": 1,
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_bash_input() {
    let td = TempDir::new().unwrap();
    let err = dispatch(
        "bash",
        json!({
            "command": "echo hi",
            "timeout_secs": 5,
            "background": true,
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_grep_input() {
    let td = TempDir::new().unwrap();
    let err = dispatch(
        "grep",
        json!({
            "pattern": "x",
            "unknown": 1,
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn deny_unknown_fields_on_glob_input() {
    let td = TempDir::new().unwrap();
    let err = dispatch(
        "glob",
        json!({
            "pattern": "*.rs",
            "mystery": "none",
        }),
        &ctx(td.path()),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "err: {err:?}");
}

#[tokio::test]
async fn denylist_blocks_bash_through_dispatch() {
    let td = TempDir::new().unwrap();
    let err = dispatch("bash", json!({ "command": "rm -rf /" }), &ctx(td.path()))
        .await
        .unwrap_err();
    match err {
        ToolError::BashDenied { reason } => assert_eq!(reason, "deletes root filesystem"),
        other => panic!("expected BashDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn target_extension_denylist_fires_through_dispatch() {
    let td = TempDir::new().unwrap();
    let mut bash_denylist = BashDenylist::with_base();
    let cfg = ToolsConfig {
        sandbox: SandboxMode::Off,
        path_deny_patterns: Vec::new(),
        lane_overrides: Default::default(),
        bash_denylist_extend: vec![DenyEntryConfig {
            tokens: vec!["./deploy.sh".into()],
            reason: "deploys are a human action".into(),
        }],
    };
    bash_denylist.extend_from(&cfg);

    let ctx = ToolContext {
        working_dir: td.path().to_path_buf(),
        router: Arc::new(LaneRouter::new(SandboxMode::Off).unwrap()),
        sandbox: SandboxMode::Off,
        path_deny_patterns: Vec::new(),
        bash_denylist: Arc::new(bash_denylist),
        persist_base: None,
        invocation_id: None,
    };

    let err = dispatch("bash", json!({ "command": "./deploy.sh --prod" }), &ctx)
        .await
        .unwrap_err();
    match err {
        ToolError::BashDenied { reason } => assert_eq!(reason, "deploys are a human action"),
        other => panic!("expected BashDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn long_output_truncates_and_persists_via_dispatch() {
    let td = TempDir::new().unwrap();
    let persist = td.path().join("persist");
    std::fs::create_dir_all(&persist).unwrap();
    let id = Uuid::now_v7();

    let out = dispatch(
        "bash",
        json!({
            "command": r#"python3 -c "print('x'*100000)" 2>/dev/null || yes x | head -c 100000"#,
            "timeout_secs": 10,
        }),
        &ctx_with_persist(td.path(), persist.clone(), id),
    )
    .await
    .unwrap();

    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["truncated"], true);
    let persisted = out["persisted_output_path"].as_str().unwrap();
    let expected_suffix = format!("{id}.log");
    assert!(
        persisted.ends_with(&expected_suffix),
        "persisted path must end with {expected_suffix}, got {persisted}"
    );
    let full = std::fs::read(persisted).unwrap();
    assert!(
        full.len() >= 100_000,
        "persist file should contain >=100k bytes, got {}",
        full.len()
    );
}

#[test]
fn all_schemas_has_exactly_six_with_distinct_names() {
    let schemas = all_schemas();
    assert_eq!(schemas.len(), 6);
    let mut names: Vec<&str> = schemas.iter().map(|s| s.name).collect();
    names.sort();
    assert_eq!(names, vec!["bash", "edit", "glob", "grep", "read", "write"]);
}

#[test]
fn every_schema_has_valid_json_schema_shape() {
    for s in all_schemas() {
        let schema = &s.input_schema;
        assert!(schema.is_object(), "{} schema is not an object", s.name);
        let obj = schema.as_object().unwrap();
        assert!(
            obj.contains_key("properties")
                || obj.contains_key("$defs")
                || obj.contains_key("definitions")
                || obj.contains_key("type"),
            "{}: schema lacks structural keys (got: {:?})",
            s.name,
            obj.keys().collect::<Vec<_>>()
        );
        assert!(!s.description.is_empty(), "{}: empty description", s.name);
    }
}

#[test]
fn schema_for_round_trips_with_all_schemas() {
    let schemas = all_schemas();
    for s in &schemas {
        let fetched = schema_for(s.name).unwrap();
        assert_eq!(fetched.name, s.name);
        assert_eq!(fetched.description, s.description);
    }
    assert!(schema_for("nope").is_none());
}

#[test]
fn sandbox_required_without_bwrap_surfaces_router_init_error() {
    // Runs meaningfully only on hosts without functional bwrap.
    if !tools::detect_bwrap_functional() {
        match LaneRouter::new(SandboxMode::Required) {
            Err(RouterInitError::BwrapRequired) => {}
            Ok(_) => panic!("expected BwrapRequired error on bwrap-less host"),
        }
    }
}
