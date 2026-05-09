//! Phase 1 keystone test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! The harness initializes a thread-local subscriber whose layer
//! composition mirrors production `telemetry::init`, runs a scenario
//! closure, then re-parses the resulting `events.log` JSONL and exposes
//! `assert_event` so later phases can assert presence of named spans and
//! their required fields.
//!
//! Smoke scenarios for Phase 1 (`decompose_smoke`, `daemon_serve_core_smoke`,
//! and `decompose_smoke_isolation`) drive already-visible spans so the
//! harness itself is exercised. Phase 2-onward will extend this file with
//! per-stage scenarios that exercise newly-visible spans.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;
use tempfile::TempDir;
use tracing::info_span;

/// Run `scenario` under a thread-local subscriber whose layer composition
/// mirrors production `telemetry::init`, then return the parsed JSONL from
/// `events.log` in `run_dir`.
///
/// **Sync only.** `set_default` is thread-local; events emitted on a
/// different thread (notably `tokio::spawn`-ed worker threads) will not be
/// captured. Phase-1 scenarios are sync; an async harness will land
/// alongside the first phase that needs it.
fn run_and_capture_events_sync<F: FnOnce()>(run_dir: &Path, scenario: F) -> Vec<Value> {
    {
        let _guard = telemetry::init_for_test(run_dir, "debug").expect("init_for_test");
        scenario();
    }
    // Both guards dropped: subscriber uninstalled, writers flushed.
    let json_path = run_dir.join("events.log");
    let body = fs::read_to_string(&json_path).expect("read events.log");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("parse JSONL line {line:?}: {e}")))
        .collect()
}

/// Assert that `events` contains at least one event whose immediate or
/// ancestor span carries `name`, and that some such event surfaces every
/// required field.
///
/// Fields may live on the event's `fields` object, on the immediate `span`
/// entry, or on any element in the `spans` array — emission patterns vary
/// (`#[instrument]` puts the field on the span; an explicit `info!(field
/// = ...)` puts it under `fields`). The contract is "the field appears on
/// a grep-able event line carrying span X," not "the field is in a
/// specific JSON sub-object."
fn assert_event(events: &[Value], name: &str, required_fields: &[&str]) {
    let matches: Vec<&Value> = events.iter().filter(|ev| event_carries_span(ev, name)).collect();
    assert!(
        !matches.is_empty(),
        "expected at least one event carrying span `{name}`, but found none. \
         Span names observed in {} events: {:?}",
        events.len(),
        observed_span_names(events),
    );
    for field in required_fields {
        let any = matches.iter().any(|ev| event_has_field(ev, name, field));
        assert!(
            any,
            "events carrying span `{name}` do not surface required field `{field}`. \
             Matched event count = {}, sample event = {}",
            matches.len(),
            matches
                .first()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "<none>".to_string()),
        );
    }
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
    // 1. event.fields[field]: the field was emitted via `info!(field = ...)`.
    if event.get("fields").and_then(|f| f.get(field)).is_some() {
        return true;
    }
    // 2. event.span (when its name matches): top-level span fields are
    //    flattened, e.g. { "name":"tool.read", "path":"/x" }.
    if let Some(span) = event.get("span")
        && span.get("name").and_then(|v| v.as_str()) == Some(span_name)
        && span.get(field).is_some()
    {
        return true;
    }
    // 3. ancestor spans[*]: any element naming the span and surfacing the field.
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

// ---------- Smoke scenarios ----------

#[test]
fn decompose_smoke() {
    let td = TempDir::new().unwrap();
    let events = run_and_capture_events_sync(td.path(), || {
        // Mirrors `decomposer::decompose`'s `#[instrument]` field set per
        // `crates/decomposer/CLAUDE.md`. `child_count` and `outcome` are
        // declared as `field::Empty` in production and recorded later;
        // we do the same here.
        let span = info_span!(
            "decompose",
            plan_id = "p-test",
            goal_len = 12,
            child_count = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let _enter = span.enter();
        tracing::info!("decompose: smoke-test event");
    });
    assert_event(&events, "decompose", &["plan_id", "goal_len"]);
}

#[test]
fn daemon_serve_core_smoke() {
    let td = TempDir::new().unwrap();
    let events = run_and_capture_events_sync(td.path(), || {
        let span = info_span!(
            "daemon.serve_core",
            target = "/tmp/test-target",
            session_id = "20260509-120000",
            process_id = "pc-abcdef",
        );
        let _enter = span.enter();
        tracing::info!("daemon.serve_core: smoke-test event");
    });
    assert_event(&events, "daemon.serve_core", &["target", "session_id", "process_id"]);
}

#[test]
fn decompose_smoke_isolation() {
    // Regression guard for thread-local subscriber leakage across tests:
    // each test gets its own tempdir + subscriber install, so a second
    // emission of the same span name must land only in this test's
    // events.log, with no contamination from `decompose_smoke`'s file.
    let td = TempDir::new().unwrap();
    let events = run_and_capture_events_sync(td.path(), || {
        let span = info_span!("decompose", plan_id = "p-isolation", goal_len = 7);
        let _enter = span.enter();
        tracing::info!("decompose: isolation-check event");
    });
    assert_event(&events, "decompose", &["plan_id"]);
    // Sanity: the events.log carries this test's plan_id, not the prior
    // smoke's. Re-parse the events.log directly for the assertion since
    // the harness only checks presence, not exclusivity.
    let plan_ids: BTreeSet<String> = events
        .iter()
        .filter_map(|ev| {
            ev.get("span")
                .and_then(|s| s.get("plan_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    assert_eq!(
        plan_ids,
        BTreeSet::from(["p-isolation".to_string()]),
        "events.log carries only this test's plan_id; thread-local subscriber leaked if other ids appear"
    );
}
