//! Phase 1 acceptance test for the instrumentation sweep.
//!
//! Drives one Implementer iteration that triggers a lifeguard escalation
//! (the same shape as the Stage 9 motivating incident: an LLM repeating
//! the same action until `max_repeat` trips). Captures every span name
//! and recorded field via a custom layer, and asserts the span graph
//! includes the entry points named in the design doc.
//!
//! Failing this test means an `#[instrument]` attribute was removed from
//! one of the agent entry points or its scope-fields shape regressed.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use agents::{Deps, ImplementerConfig, ImplementerError, ToolExecutor, run_implementer};
use context::{InlineContextBuilder, StateSummary};
use domain::Work;
use llm::ScriptedLlm;
use store::Store;
use worktree::Worktree;

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
    fn names(&self) -> Vec<String> {
        self.spans.lock().unwrap().iter().map(|s| s.name.clone()).collect()
    }

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

struct EchoTools;

impl ToolExecutor for EchoTools {
    async fn execute(
        &self,
        tool_name: &str,
        _input: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<String, agents::DispatchError> {
        Ok(format!("echo {tool_name}"))
    }
}

fn init_repo() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn agents_smoke_spans_lifeguard_escalation() {
    let capture = Arc::new(SpanCapture::default());
    let layer = CaptureLayer {
        capture: capture.clone(),
    };
    let subscriber = Registry::default().with(EnvFilter::new("trace")).with(layer);

    let (_dir, repo_path, sha) = init_repo();
    let store = Store::open(&repo_path).await.unwrap();
    let plan = domain::Plan::new("instrumentation smoke".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let work = Work::new(plan.id.clone(), "trigger lifeguard".to_string());
    store.works().create(work.clone()).await.unwrap();

    let worktree_root = repo_path.parent().unwrap().join("instr-wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    let wt = Worktree::create(&repo_path, &worktree_root, work.id.clone(), &sha).unwrap();

    // The LLM emits the same action three times; with max_repeat_action=3
    // the lifeguard escalates on the third occurrence.
    let llm = ScriptedLlm::new();
    let same = r#"[{"type":"run_tool","tool":"bash","input":{"command":"ls"}}]"#.to_string();
    llm.queue_free(Ok(same.clone()));
    llm.queue_free(Ok(same.clone()));
    llm.queue_free(Ok(same));

    let config = ImplementerConfig {
        max_repeat_action: 3,
        max_iterations: 5,
        ..ImplementerConfig::default()
    };

    let deps = Deps {
        llm,
        tools: EchoTools,
        bundles: store,
        context: InlineContextBuilder::new(),
        config,
        tool_schemas: vec![],
        state: StateSummary::default(),
    };

    // `set_default` makes the subscriber thread-local; current_thread
    // tokio runtime keeps every poll on this thread, so async spans are
    // captured.
    let _guard = tracing::subscriber::set_default(subscriber);
    let result = run_implementer(&work, &wt, &deps).await;
    drop(_guard);

    let err = result.expect_err("lifeguard should escalate after max_repeat hits");
    let escalation_msg = match err {
        ImplementerError::EscalationNeeded(s) => s,
        other => panic!("expected EscalationNeeded, got {other:?}"),
    };
    assert!(
        escalation_msg.contains("repeated"),
        "escalation message should name repetition: {escalation_msg}"
    );

    let names = capture.names();

    // The Stage 9 motivating story: the lifeguard's check_action span fires
    // and carries action_kind, action_hash, action_count, max_repeat as
    // diagnostic fields - reading the captured span answers "which action
    // repeated?" without rerunning at debug.
    let check_action = capture.find("check_action").expect("check_action span must be emitted");
    assert!(
        check_action.fields.contains_key("action_kind"),
        "check_action must record action_kind: {:?}",
        check_action.fields
    );
    assert!(
        check_action.fields.contains_key("action_hash"),
        "check_action must record action_hash: {:?}",
        check_action.fields
    );
    assert!(
        check_action.fields.contains_key("action_count"),
        "check_action must record action_count: {:?}",
        check_action.fields
    );
    assert_eq!(
        check_action.fields.get("max_repeat").map(String::as_str),
        Some("3"),
        "check_action must record max_repeat=3: {:?}",
        check_action.fields
    );

    let dispatch_action = capture
        .find("dispatch_action")
        .expect("dispatch_action span must be emitted");
    assert!(
        dispatch_action.fields.contains_key("work_id"),
        "dispatch_action must carry work_id: {:?}",
        dispatch_action.fields
    );
    assert!(
        dispatch_action.fields.contains_key("action_kind"),
        "dispatch_action must carry action_kind: {:?}",
        dispatch_action.fields
    );

    let run_implementer_span = capture
        .find("run_implementer")
        .expect("run_implementer span must be emitted");
    assert_eq!(
        run_implementer_span.fields.get("work_id").map(String::as_str),
        Some(&*work.id.to_string()),
        "run_implementer must carry work_id: {:?}",
        run_implementer_span.fields
    );

    let parse_actions = capture
        .find("parse_actions")
        .expect("parse_actions span must be emitted");
    assert!(
        parse_actions.fields.contains_key("raw_chars"),
        "parse_actions must record raw_chars: {:?}",
        parse_actions.fields
    );

    // Sanity: the spans we expect for Phase 1 all appear at least once.
    for required in ["run_implementer", "parse_actions", "dispatch_action", "check_action"] {
        assert!(
            names.iter().any(|n| n == required),
            "expected span '{required}' in {names:?}"
        );
    }
}
