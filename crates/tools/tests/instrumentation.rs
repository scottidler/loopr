//! Phase 2 acceptance test for the instrumentation sweep.
//!
//! Drives each builtin through `dispatch` and asserts the per-tool span
//! appears with the required `tool_name`, `lane`, and tool-specific keys
//! (`path`, `pattern`, `command_chars`). If an `#[instrument]` is removed
//! from a builtin's `execute()`, this test fails.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use tools::{BashDenylist, LaneRouter, SandboxMode, ToolContext, dispatch};

#[derive(Debug, Default, Clone)]
struct CapturedSpan {
    name: String,
    fields: BTreeMap<String, String>,
}

#[derive(Default)]
struct SpanCapture {
    spans: Mutex<Vec<CapturedSpan>>,
}

impl SpanCapture {
    fn find(&self, name: &str) -> Option<CapturedSpan> {
        self.spans.lock().unwrap().iter().find(|s| s.name == name).cloned()
    }
}

#[derive(Default)]
struct StringFieldVisitor(BTreeMap<String, String>);

impl Visit for StringFieldVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}"));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

struct CaptureLayer {
    capture: Arc<SpanCapture>,
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: Context<'_, S>) {
        let mut visitor = StringFieldVisitor::default();
        attrs.record(&mut visitor);
        self.capture.spans.lock().unwrap().push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.0,
        });
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let span = match ctx.span(id) {
            Some(s) => s,
            None => return,
        };
        let name = span.metadata().name().to_string();
        let mut visitor = StringFieldVisitor::default();
        values.record(&mut visitor);
        let mut spans = self.capture.spans.lock().unwrap();
        if let Some(rec) = spans.iter_mut().rfind(|s| s.name == name) {
            rec.fields.extend(visitor.0);
        }
    }
}

fn ctx(dir: &Path) -> ToolContext {
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

fn capture_subscriber() -> (Arc<SpanCapture>, impl tracing::Subscriber) {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    (cap, sub)
}

#[tokio::test(flavor = "current_thread")]
async fn read_emits_tool_span() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();
    let p = td.path().join("hello.txt");
    std::fs::write(&p, "alpha\nbeta\n").unwrap();

    dispatch("read", json!({ "path": "hello.txt" }), &ctx(td.path()))
        .await
        .unwrap();

    let s = cap.find("tool.read").expect("tool.read span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("read"));
    assert_eq!(s.fields.get("lane").map(String::as_str), Some("local"));
    assert!(s.fields.contains_key("path"), "missing path: {:?}", s.fields);
}

#[tokio::test(flavor = "current_thread")]
async fn write_emits_tool_span() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();

    dispatch(
        "write",
        json!({ "path": "out.txt", "content": "hello" }),
        &ctx(td.path()),
    )
    .await
    .unwrap();

    let s = cap.find("tool.write").expect("tool.write span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("write"));
    assert_eq!(s.fields.get("lane").map(String::as_str), Some("local"));
    assert_eq!(s.fields.get("bytes").map(String::as_str), Some("5"));
}

#[tokio::test(flavor = "current_thread")]
async fn edit_emits_tool_span() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();
    let p = td.path().join("e.txt");
    std::fs::write(&p, "the quick").unwrap();

    dispatch(
        "edit",
        json!({ "path": "e.txt", "old_string": "quick", "new_string": "lazy" }),
        &ctx(td.path()),
    )
    .await
    .unwrap();

    let s = cap.find("tool.edit").expect("tool.edit span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("edit"));
    assert_eq!(s.fields.get("lane").map(String::as_str), Some("local"));
}

#[tokio::test(flavor = "current_thread")]
async fn glob_emits_tool_span() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.txt"), "x").unwrap();

    dispatch("glob", json!({ "pattern": "*.txt" }), &ctx(td.path()))
        .await
        .unwrap();

    let s = cap.find("tool.glob").expect("tool.glob span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("glob"));
    assert_eq!(s.fields.get("lane").map(String::as_str), Some("local"));
    assert_eq!(s.fields.get("pattern").map(String::as_str), Some("*.txt"));
}

#[tokio::test(flavor = "current_thread")]
async fn grep_emits_tool_span() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();
    std::fs::write(td.path().join("a.txt"), "needle\n").unwrap();

    let _ = dispatch("grep", json!({ "pattern": "needle" }), &ctx(td.path())).await;

    let s = cap.find("tool.grep").expect("tool.grep span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("grep"));
    assert_eq!(s.fields.get("lane").map(String::as_str), Some("local"));
    assert_eq!(s.fields.get("pattern").map(String::as_str), Some("needle"));
    let router = cap.find("router.spawn").expect("router.spawn span must fire");
    assert!(
        router.fields.contains_key("lane"),
        "router.spawn must record lane: {:?}",
        router.fields
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bash_emits_tool_span_with_lane_recorded() {
    let (cap, sub) = capture_subscriber();
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);
    let td = TempDir::new().unwrap();

    let _ = dispatch("bash", json!({ "command": "echo hi" }), &ctx(td.path())).await;

    let s = cap.find("tool.bash").expect("tool.bash span must fire");
    assert_eq!(s.fields.get("tool_name").map(String::as_str), Some("bash"));
    assert!(
        s.fields.contains_key("command_chars"),
        "missing command_chars: {:?}",
        s.fields
    );
    assert!(
        s.fields.contains_key("lane"),
        "lane should be recorded post-classification: {:?}",
        s.fields
    );
}
