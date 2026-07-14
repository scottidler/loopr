//! Phase 4 acceptance test for the instrumentation sweep.
//!
//! Drives one happy-path `integrate` call against a real git tempdir
//! and asserts the `integrator.integrate` span fired with all required
//! scope fields, plus the per-phase markers.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;
use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, TargetKind, Work};
use integrator::{IntegratorConfig, IntegratorDeps, integrate};
use store::Store;

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

fn init_repo_with_integration_branch(plan: &Plan) -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join(".gitignore"), ".loopr/\n").unwrap();
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    let integ = format!("loopr/plan-{}", plan.id);
    git(&path, &["branch", &integ]);
    (dir, path, sha)
}

fn create_bundle_branch(repo: &Path, plan: &Plan, branch: &str, file: &str, contents: &str) -> String {
    let integ = format!("loopr/plan-{}", plan.id);
    git(repo, &["checkout", "-q", "-b", branch, &integ]);
    std::fs::write(repo.join(file), contents).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "bundle", "--no-gpg-sign"]);
    let sha = git_capture(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    sha
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

async fn persist_accepted_bundle(store: &Store, work: &Work, branch: &str, head: &str, paths: Vec<String>) -> Bundle {
    let mut b = Bundle::new(work.id.clone(), branch.to_string(), vec!["claim".to_string()]);
    b.head_commit = Some(head.to_string());
    b.paths = paths;
    store.bundles().create(b.clone()).await.unwrap();

    for target in [BundleStatus::Triaged, BundleStatus::Reviewed, BundleStatus::Accepted] {
        let role = match target {
            BundleStatus::Reviewed => Role::Reviewer,
            _ => Role::Reactor,
        };
        let current = store.bundles().get(&b.id).await.unwrap();
        let mut next = current.clone();
        next.transition(target, role).unwrap();
        store
            .bundles()
            .update(next, current.updated_at, role, TargetKind::Normal)
            .await
            .unwrap();
    }
    store.bundles().get(&b.id).await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn integrator_smoke_spans_happy_path() {
    let cap = Arc::new(SpanCapture::default());
    let layer = CaptureLayer { capture: cap.clone() };
    let sub = Registry::default().with(EnvFilter::new("trace")).with(layer);
    telemetry::ensure_global_interested_default();
    let _g = tracing::subscriber::set_default(sub);

    let plan = Plan::new("ship".to_string());
    let (_dir, repo, _) = init_repo_with_integration_branch(&plan);
    let store = Store::open(&repo).await.unwrap();
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "ship feature X".to_string());
    work.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["X works".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let branch = format!("loopr/wk-{}", work.id);
    let head = create_bundle_branch(&repo, &plan, &branch, "feat.rs", "fn feat() {}\n");
    let bundle = persist_accepted_bundle(&store, &work, &branch, &head, vec!["feat.rs".to_string()]).await;

    let deps = IntegratorDeps {
        bundle_sink: &store,
        works: &store,
        ticks: &store,
        config: IntegratorConfig::default(),
        target: repo.clone(),
        git_lock: Arc::new(AsyncMutex::new(())),
    };

    let _tick = integrate(std::slice::from_ref(&bundle), &plan, &deps).await.unwrap();
    store.close().await.unwrap();

    let s = cap
        .find("integrator.integrate")
        .expect("integrator.integrate span must fire");
    assert_eq!(s.fields.get("plan_id").map(String::as_str), Some(&*plan.id.to_string()));
    assert_eq!(s.fields.get("bundle_count").map(String::as_str), Some("1"));
    assert!(
        s.fields.contains_key("integration_branch"),
        "missing integration_branch: {:?}",
        s.fields
    );
    assert_eq!(
        s.fields.get("integration_branch").map(String::as_str),
        Some(format!("loopr/plan-{}", plan.id).as_str())
    );
    // The phase field walks preflight -> git_sequence -> commit; the last value
    // wins, so the closing span carries `commit`.
    assert_eq!(s.fields.get("phase").map(String::as_str), Some("commit"));

    // Inner spans fire too.
    cap.find("integrator.preflight_plan_consistency")
        .expect("preflight_plan_consistency span");
    let trans = cap
        .find("integrator.transition_bundle")
        .expect("transition_bundle span");
    assert!(trans.fields.contains_key("bundle_id"));
    assert!(trans.fields.contains_key("target_status"));

    // Git wrapper spans for the merge sequence.
    cap.find("integrator.git.checkout")
        .expect("integrator.git.checkout span");
    cap.find("integrator.git.merge_no_ff")
        .expect("integrator.git.merge_no_ff span");
}
