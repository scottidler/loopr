//! Phase 2 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`. Each builtin's
//! `execute()` is dispatched through the real `LaneRouter` under a
//! tempdir-rooted telemetry subscriber; the resulting `events.log` is
//! re-parsed and asserted to contain the documented `tool.*` span with
//! its required scope fields.
//!
//! The harness mirrors `crates/telemetry/tests/events_log_contract.rs`'s
//! shape; helpers are inlined rather than shared via a `pub mod testing`
//! to keep the telemetry crate's surface narrow until a third copy
//! crosses the duplication threshold.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tempfile::TempDir;

use tools::builtin;
use tools::denylist::BashDenylist;
use tools::lane::Lane;
use tools::router::LaneRouter;
use tools::sandbox::SandboxMode;
use tools::tool::ToolContext;

// ---------- Harness ----------

fn ctx(dir: &Path) -> ToolContext {
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

fn read_events(run_dir: &Path) -> Vec<Value> {
    let body = fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("parse JSONL line {line:?}: {e}")))
        .collect()
}

fn event_carries_span(event: &Value, name: &str) -> bool {
    if let Some(span) = event.get("span")
        && span.get("name").and_then(|v| v.as_str()) == Some(name)
    {
        return true;
    }
    if let Some(spans) = event.get("spans").and_then(|v| v.as_array())
        && spans
            .iter()
            .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(name))
    {
        return true;
    }
    false
}

fn event_has_field(event: &Value, span_name: &str, field: &str) -> bool {
    if event.get("fields").and_then(|f| f.get(field)).is_some() {
        return true;
    }
    if let Some(span) = event.get("span")
        && span.get("name").and_then(|v| v.as_str()) == Some(span_name)
        && span.get(field).is_some()
    {
        return true;
    }
    if let Some(spans) = event.get("spans").and_then(|v| v.as_array()) {
        for s in spans {
            if s.get("name").and_then(|v| v.as_str()) == Some(span_name) && s.get(field).is_some() {
                return true;
            }
        }
    }
    false
}

fn observed_span_names(events: &[Value]) -> Vec<String> {
    let mut names = BTreeSet::new();
    for ev in events {
        if let Some(name) = ev.get("span").and_then(|s| s.get("name")).and_then(|n| n.as_str()) {
            names.insert(name.to_string());
        }
        if let Some(spans) = ev.get("spans").and_then(|v| v.as_array()) {
            for s in spans {
                if let Some(name) = s.get("name").and_then(|n| n.as_str()) {
                    names.insert(name.to_string());
                }
            }
        }
    }
    names.into_iter().collect()
}

fn assert_event(events: &[Value], name: &str, required_fields: &[&str]) {
    let matches: Vec<&Value> = events.iter().filter(|ev| event_carries_span(ev, name)).collect();
    assert!(
        !matches.is_empty(),
        "expected at least one event carrying span `{name}`, but found none. \
         Span names observed: {:?}",
        observed_span_names(events),
    );
    for field in required_fields {
        let any = matches.iter().any(|ev| event_has_field(ev, name, field));
        assert!(
            any,
            "events carrying span `{name}` do not surface required field `{field}`. \
             Sample event: {}",
            matches
                .first()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        );
    }
}

// ---------- Per-builtin scenarios ----------

#[tokio::test]
async fn tool_read_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    let target = work_dir.path().join("hello.txt");
    fs::write(&target, "alpha\nbeta\ngamma\n").unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::read::execute(
            builtin::read::Input {
                path: target.clone(),
                offset: None,
                limit: None,
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("read ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "tool.read", &["tool_name", "lane", "path", "working_dir"]);
}

#[tokio::test]
async fn tool_write_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    let target = work_dir.path().join("out.txt");

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::write::execute(
            builtin::write::Input {
                path: target.clone(),
                content: "hello world\n".to_string(),
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("write ok");
    }

    let events = read_events(log_dir.path());
    assert_event(
        &events,
        "tool.write",
        &["tool_name", "lane", "path", "bytes", "working_dir"],
    );
}

#[tokio::test]
async fn tool_edit_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    let target = work_dir.path().join("doc.md");
    fs::write(&target, "before TOKEN after\n").unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::edit::execute(
            builtin::edit::Input {
                path: target.clone(),
                old_string: "TOKEN".to_string(),
                new_string: "REPLACED".to_string(),
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("edit ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "tool.edit", &["tool_name", "lane", "path", "working_dir"]);
}

#[tokio::test]
async fn tool_glob_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    fs::write(work_dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    fs::write(work_dir.path().join("b.rs"), "fn b() {}\n").unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::glob::execute(
            builtin::glob::Input {
                pattern: "*.rs".to_string(),
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("glob ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "tool.glob", &["tool_name", "lane", "pattern", "working_dir"]);
}

#[tokio::test]
async fn tool_grep_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    fs::write(work_dir.path().join("src.rs"), "fn target() {}\nfn other() {}\n").unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::grep::execute(
            builtin::grep::Input {
                pattern: "target".to_string(),
                path: None,
                glob: None,
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("grep ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "tool.grep", &["tool_name", "lane", "pattern", "working_dir"]);
    // The router ran underneath; assert its span was visible too.
    assert_event(&events, "router.spawn", &["lane", "timeout_secs"]);
    assert_event(&events, "spawn.process_group", &["timeout_secs"]);
}

#[tokio::test]
async fn tool_bash_visible() {
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _ = builtin::bash::execute(
            builtin::bash::Input {
                command: "echo hello".to_string(),
                timeout_secs: Some(5),
            },
            &ctx(work_dir.path()),
        )
        .await
        .expect("bash ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "tool.bash", &["tool_name", "command_chars", "working_dir"]);
}

#[tokio::test]
async fn router_spawn_acquires_slot_visible() {
    // Cover the router span on a non-tool path: dispatch a plain command
    // through router.spawn directly and assert the dispatch event is
    // visible. Ensures Phase 2's new "router: dispatched" event is
    // attributed to router.spawn span ancestry.
    let log_dir = TempDir::new().unwrap();
    let work_dir = TempDir::new().unwrap();
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).expect("router"));

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let mut cmd = tokio::process::Command::new("echo");
        cmd.arg("hi");
        cmd.current_dir(work_dir.path());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        let _ = router
            .spawn(
                cmd,
                Lane::Local,
                work_dir.path(),
                Some(5),
                tools::spawn::PersistConfig::default(),
            )
            .await
            .expect("spawn ok");
    }

    let events = read_events(log_dir.path());
    assert_event(&events, "router.spawn", &["lane", "timeout_secs"]);
    assert_event(&events, "spawn.process_group", &["timeout_secs"]);
}
