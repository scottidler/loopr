//! Phase 5 acceptance test for the instrumentation sweep.
//!
//! Creates and cleans up a worktree against a real git tempdir;
//! asserts the `worktree.create` and `worktree.cleanup` spans appear
//! with the required scope fields (work_id, branch, worktree_path).

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as StdCommand;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use domain::WorkId;
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

fn init_repo() -> (TempDir, std::path::PathBuf, String) {
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

#[test]
fn worktree_smoke_spans_create_then_cleanup() {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    let _g = tracing::subscriber::set_default(sub);

    let (_dir, repo, sha) = init_repo();
    let worktree_root = repo.parent().unwrap().join("instr-wt");
    std::fs::create_dir_all(&worktree_root).unwrap();
    let work_id = WorkId::from_str("wk-instr1").unwrap();

    let wt = Worktree::create(&repo, &worktree_root, work_id.clone(), &sha).unwrap();
    let branch_owned = wt.branch().to_string();
    wt.cleanup().unwrap();

    let create = cap.find("worktree.create").expect("worktree.create span");
    assert_eq!(
        create.fields.get("work_id").map(String::as_str),
        Some(&*work_id.to_string())
    );
    assert!(create.fields.contains_key("base_sha"), "base_sha: {:?}", create.fields);
    assert!(
        create.fields.contains_key("seq"),
        "seq filled post-creation: {:?}",
        create.fields
    );
    assert!(
        create.fields.contains_key("branch"),
        "branch filled post-creation: {:?}",
        create.fields
    );

    let cleanup = cap.find("worktree.cleanup").expect("worktree.cleanup span");
    assert_eq!(
        cleanup.fields.get("work_id").map(String::as_str),
        Some(&*work_id.to_string())
    );
    assert_eq!(
        cleanup.fields.get("branch").map(String::as_str),
        Some(branch_owned.as_str())
    );
    assert!(cleanup.fields.contains_key("worktree_path"));

    cap.find("worktree.ops.try_create_at_seq")
        .expect("ops.try_create_at_seq span");
    cap.find("worktree.ops.remove_worktree")
        .expect("ops.remove_worktree span");
}
