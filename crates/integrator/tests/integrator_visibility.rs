//! Phase 3 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! Drives a happy-path `integrate` call under the real telemetry
//! subscriber composition, then re-parses `events.log` and asserts
//! every documented `integrator.*` span surfaced its required fields.
//!
//! Process lesson (per the design doc's Phase 1 narrative): the
//! existing `tests/instrumentation.rs` uses an in-memory capture layer,
//! so an `#[instrument]` whose body never logs passes that test but
//! emits zero `events.log` lines in production. This test closes that
//! gap by writing to a tempdir's `events.log` through the real
//! `telemetry::init_for_test` subscriber.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::Mutex as AsyncMutex;

use domain::{AcceptanceCriteria, Bundle, BundleStatus, Plan, Role, Work};
use integrator::{IntegratorConfig, IntegratorDeps, integrate};
use store::Store;

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

// ---------- Git fixture (mirrors tests/instrumentation.rs) ----------

fn init_repo_with_integration_branch(plan: &Plan) -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    fs::write(path.join(".gitignore"), ".loopr/\n").unwrap();
    fs::write(path.join("README.md"), "initial\n").unwrap();
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
    fs::write(repo.join(file), contents).unwrap();
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
        store.bundles().update(next, current.updated_at).await.unwrap();
    }
    store.bundles().get(&b.id).await.unwrap()
}

// ---------- Scenario ----------

#[tokio::test(flavor = "current_thread")]
async fn integrator_happy_path_phases_visible() {
    let log_dir = TempDir::new().unwrap();
    let plan = Plan::new("ship".to_string());
    let (_dir, repo, _) = init_repo_with_integration_branch(&plan);
    let store = Store::open(&repo).await.unwrap();
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "ship feature X".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["X works".to_string()]);
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

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _tick = integrate(std::slice::from_ref(&bundle), &plan, &deps)
            .await
            .expect("integrate ok");
    }
    store.close().await.unwrap();

    let events = read_events(log_dir.path());

    // The outer integrator.integrate span must surface its scope fields,
    // and the per-phase `integrator: phase begin` events must be visible
    // (one per phase). Without these, a stalled integration's last
    // visible phase is unknowable from events.log.
    assert_event(
        &events,
        "integrator.integrate",
        &["plan_id", "bundle_count", "target", "integration_branch"],
    );

    // Phase markers: we asserted that at least one event under
    // integrator.integrate carries `phase`. Sanity-check the per-phase
    // emission count by collecting each `phase` value seen on the new
    // info events.
    let phase_values: BTreeSet<String> = events
        .iter()
        .filter(|ev| event_carries_span(ev, "integrator.integrate"))
        .filter(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str()) == Some("integrator: phase begin")
        })
        .filter_map(|ev| {
            ev.get("fields")
                .and_then(|f| f.get("phase"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    // `validation` phase fires only when validation_commands is non-empty;
    // happy-path uses default config (empty), so we expect the other 3.
    for phase in ["preflight", "git_sequence", "commit"] {
        assert!(
            phase_values.contains(phase),
            "expected `integrator: phase begin` event with phase=`{phase}`. Observed phase events: {phase_values:?}",
        );
    }

    // Bundle transition events on the success path.
    assert_event(&events, "integrator.transition_bundle", &["bundle_id", "target_status"]);

    // Git helpers fired during the merge sequence.
    assert_event(&events, "integrator.git.checkout", &["branch"]);
    assert_event(&events, "integrator.git.merge_no_ff", &["branch"]);
}
