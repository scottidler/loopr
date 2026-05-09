//! Phase 7 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! Asserts the new `director.accept_bundle` span fires when the
//! Director routes an AcceptBundle action through the spawner. The
//! function is exposed as `agents::director_accept_bundle` so the
//! contract test can drive it directly without spinning up a full
//! `run_director` loop.
//!
//! The `restart_reason` field on `director.run` is tested via the
//! existing director-restart unit tests in
//! `crates/agents/src/director/tests.rs`; this file covers the new
//! per-Bundle span specifically.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;
use tempfile::TempDir;

use agents::{WorkSpawner, director_accept_bundle};
use domain::{BundleId, PlanId, WorkId};

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
        "expected at least one event carrying span `{name}`. Span names observed: {:?}",
        observed_span_names(events),
    );
    for field in required_fields {
        let any = matches.iter().any(|ev| event_has_field(ev, name, field));
        assert!(
            any,
            "events carrying span `{name}` do not surface required field `{field}`."
        );
    }
}

// ---------- Minimal WorkSpawner stub ----------

#[derive(Default)]
struct RecordingSpawner {
    accept_bundle_calls: Mutex<Vec<BundleId>>,
}

impl WorkSpawner for RecordingSpawner {
    fn accept_bundle(&self, bundle_id: BundleId) {
        self.accept_bundle_calls.lock().unwrap().push(bundle_id);
    }
    fn override_work(&self, _work_id: WorkId, _target_status: domain::WorkStatus, _reason: String) {}
    fn assign_work(&self, _work_id: WorkId) {}
}

// ---------- Scenario ----------

#[test]
fn director_accept_bundle_emits_routing_event() {
    let log_dir = TempDir::new().unwrap();
    let plan_id = PlanId::new();
    let bundle_id: BundleId = "bd-test-7".parse().expect("parse bundle id");
    let spawner = RecordingSpawner::default();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        director_accept_bundle(&plan_id, &bundle_id, &spawner);
    }

    // The spawner was called exactly once with the right bundle id.
    let calls = spawner.accept_bundle_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0], bundle_id);

    // The span fires and surfaces both ids.
    let events = read_events(log_dir.path());
    assert_event(&events, "director.accept_bundle", &["plan_id", "bundle_id"]);
}
