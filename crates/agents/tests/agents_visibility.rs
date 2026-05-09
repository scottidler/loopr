//! Phase 4 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! Drives the synthesizable inner agent helpers (lifeguard,
//! parse::parse_actions) under the production telemetry subscriber
//! and asserts events.log carries each documented span with its
//! required fields.
//!
//! `dispatch_action` and `propose_bundle` aren't covered here — they
//! require a real Worktree + ToolExecutor; their visibility is
//! exercised end-to-end by `bin/e2e python-api`.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;

use agents::{AgentAction, Lifeguard, parse_actions};

// ---------- Harness ----------

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

// ---------- Scenarios ----------

#[test]
fn lifeguard_check_action_visible() {
    let log_dir = TempDir::new().unwrap();
    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let mut lg = Lifeguard::new(3, 3);
        let action = AgentAction::Done {
            message: "all done".to_string(),
        };
        let _ = lg.check_action(&action);
    }
    let events = read_events(log_dir.path());
    // The check_action span name is the bare function name (decomposer-style
    // dotted reconciliation tracked in Phase 9).
    assert_event(
        &events,
        "check_action",
        &["action_kind", "action_hash", "action_count", "max_repeat"],
    );
}

#[test]
fn lifeguard_record_parse_failure_visible() {
    let log_dir = TempDir::new().unwrap();
    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let mut lg = Lifeguard::new(3, 3);
        let _ = lg.record_parse_failure();
    }
    let events = read_events(log_dir.path());
    assert_event(
        &events,
        "record_parse_failure",
        &["consecutive_parse_failures", "max_parse_failures"],
    );
}

#[test]
fn parse_actions_visible() {
    let log_dir = TempDir::new().unwrap();
    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let raw = r#"[{"type":"done","message":"all good"}]"#;
        let _ = parse_actions(raw).expect("parse ok");
    }
    let events = read_events(log_dir.path());
    // parse_actions's #[instrument] uses the bare function name as the
    // span name; we assert by that name. raw_chars is declared in the
    // span fields and `action_count` is recorded post-parse.
    assert_event(&events, "parse_actions", &["raw_chars", "action_count"]);
}
