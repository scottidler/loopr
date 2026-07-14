//! Phase 7 acceptance test for the instrumentation sweep.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use serde_json::json;
use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use decomposer::{DecomposerConfig, decompose};
use domain::Plan;
use llm::{ScriptedLlm, ToolCall};

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

fn init_repo() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "x\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    (dir, path)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

#[tokio::test(flavor = "current_thread")]
async fn decomposer_smoke_spans_decompose() {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);

    let (_dir, repo) = init_repo();
    let plan = Plan::new("ship feature X with one Work".to_string());

    let llm = ScriptedLlm::new();
    llm.queue_tool(Ok(ToolCall {
        tool_name: "submit_decomposition".to_string(),
        input: json!({
            "children": [
                {
                    "title": "Implement X",
                    "content": "## Acceptance Criteria\n- it works\n",
                    "acceptance_criteria": ["it works"],
                    "dependencies": [],
                    "files": ["src/x.rs"],
                }
            ]
        }),
    }));

    let _works = decompose(&plan, &repo, &llm, &DecomposerConfig::default())
        .await
        .unwrap();

    let outer = cap.find("decompose").expect("decompose span");
    assert_eq!(
        outer.fields.get("plan_id").map(String::as_str),
        Some(&*plan.id.to_string())
    );
    assert!(outer.fields.contains_key("goal_len"));
    assert_eq!(outer.fields.get("child_count").map(String::as_str), Some("1"));
    assert_eq!(outer.fields.get("outcome").map(String::as_str), Some("ok"));

    cap.find("try_llm_once").expect("try_llm_once span");
    cap.find("collect_workspace_tree").expect("collect_workspace_tree span");
    cap.find("assemble_system").expect("assemble_system span");
    cap.find("assemble_user").expect("assemble_user span");
    // Cycle detection moved to domain::WorkGraph (span `detect_cycle`); it
    // is exercised by domain's tests, not asserted here.
    cap.find("resolve_deps").expect("resolve_deps span");
}
