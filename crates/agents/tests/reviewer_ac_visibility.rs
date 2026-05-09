//! Phase 5 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`.
//!
//! Drives a real `run_reviewer` happy-path against a tempdir repo +
//! ScriptedLlm under the production telemetry subscriber, then asserts
//! events.log carries one `reviewer.ac` debug event per AC plus a
//! `reviewer: ac roll-up` info event with the four counts.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use serde_json::Value;
use tempfile::TempDir;

use agents::{ReviewerConfig, ReviewerDeps, run_reviewer};
use context::InlineContextBuilder;
use domain::{AcceptanceCriteria, Bundle, BundleStatus, Role, Work};
use llm::ScriptedLlm;
use store::Store;

// ---------- Harness ----------

fn read_events(run_dir: &Path) -> Vec<Value> {
    let body = fs::read_to_string(run_dir.join("events.log")).expect("read events.log");
    body.lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("parse JSONL line {line:?}: {e}")))
        .collect()
}

fn observed_messages(events: &[Value]) -> BTreeSet<String> {
    events
        .iter()
        .filter_map(|ev| {
            ev.get("fields")
                .and_then(|f| f.get("message"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect()
}

// ---------- Git fixture ----------

fn init_repo_with_commit() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    fs::write(
        path.join("src.rs"),
        "fn hello() -> u32 { 42 }\nfn world() -> u32 { 7 }\n",
    )
    .unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "add src", "--no-gpg-sign"]);
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

// ---------- Scenarios ----------

#[tokio::test(flavor = "current_thread")]
async fn reviewer_emits_per_ac_events_on_accept() {
    let log_dir = TempDir::new().unwrap();
    let (_dir, repo_path, sha) = init_repo_with_commit();

    let store = Store::open(&repo_path).await.unwrap();
    let plan = domain::Plan::new("ac-visibility plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work = Work::new(plan.id.clone(), "add hello + world".to_string());
    // Three ACs so the roll-up has all three counted as `verified` on Accept.
    work.acceptance_criteria = AcceptanceCriteria(vec![
        "hello returns 42".to_string(),
        "world returns 7".to_string(),
        "tests pass".to_string(),
    ]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(
        work.id.clone(),
        "loopr/wk-ac-1".to_string(),
        vec!["added hello+world".to_string()],
    );
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(r#"{"kind":"accept","summary":"all ACs met"}"#.to_string()));

    let deps = ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path.clone(),
        path_deny_patterns: Vec::new(),
    };

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _v = run_reviewer(&bundle, &work, &deps).await.expect("review ok");
    }

    let events = read_events(log_dir.path());
    let messages = observed_messages(&events);

    // One event per AC.
    let ac_events: Vec<&Value> = events
        .iter()
        .filter(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str()) == Some("reviewer: ac evaluated")
        })
        .collect();
    assert_eq!(
        ac_events.len(),
        3,
        "expected 3 `reviewer: ac evaluated` events (one per AC), got {}. Messages observed: {:?}",
        ac_events.len(),
        messages,
    );

    // Each AC event must surface criterion + status.
    for ev in &ac_events {
        let fields = ev.get("fields").expect("event has fields");
        assert!(fields.get("criterion").is_some(), "missing criterion: {ev}");
        assert!(fields.get("status").is_some(), "missing status: {ev}");
        assert_eq!(
            fields.get("status").and_then(|v| v.as_str()),
            Some("verified"),
            "Accept verdict -> all ACs verified, got: {ev}"
        );
    }

    // Roll-up event with the four counts.
    let rollup = events
        .iter()
        .find(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str()) == Some("reviewer: ac roll-up")
        })
        .expect("expected `reviewer: ac roll-up` event");
    let f = rollup.get("fields").expect("roll-up fields");
    assert_eq!(f.get("ac_count").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(f.get("ac_verified").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(f.get("ac_failed").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(f.get("ac_skipped").and_then(|v| v.as_u64()), Some(0));
}

#[tokio::test(flavor = "current_thread")]
async fn reviewer_emits_per_ac_events_on_change_requested_with_matching_reason() {
    let log_dir = TempDir::new().unwrap();
    let (_dir, repo_path, sha) = init_repo_with_commit();

    let store = Store::open(&repo_path).await.unwrap();
    let plan = domain::Plan::new("ac-visibility plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();

    let mut work = Work::new(plan.id.clone(), "add hello + world".to_string());
    // Two ACs; the LLM's reason will mention "hello" so AC #1 maps to
    // `failed`. AC #2 won't match any reason -> `not_applicable`.
    work.acceptance_criteria = AcceptanceCriteria(vec![
        "hello returns 42 reliably".to_string(),
        "documentation covers both functions".to_string(),
    ]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(
        work.id.clone(),
        "loopr/wk-ac-2".to_string(),
        vec!["added hello+world".to_string()],
    );
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(r#"{
        "kind":"change_requested",
        "summary":"hello body is wrong",
        "reasons":[
          {"severity":"error","file":"src.rs","line":1,"message":"hello returns 42 but should return 41"}
        ]
    }"#
    .to_string()));

    let deps = ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path.clone(),
        path_deny_patterns: Vec::new(),
    };

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _v = run_reviewer(&bundle, &work, &deps).await.expect("review ok");
    }

    let events = read_events(log_dir.path());

    let rollup = events
        .iter()
        .find(|ev| {
            ev.get("fields").and_then(|f| f.get("message")).and_then(|v| v.as_str()) == Some("reviewer: ac roll-up")
        })
        .expect("expected `reviewer: ac roll-up` event");
    let f = rollup.get("fields").expect("roll-up fields");
    assert_eq!(f.get("ac_count").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(
        f.get("ac_failed").and_then(|v| v.as_u64()),
        Some(1),
        "ac #1 mentions `hello`"
    );
    assert_eq!(
        f.get("ac_skipped").and_then(|v| v.as_u64()),
        Some(1),
        "ac #2 has no matching reason word"
    );
    assert_eq!(f.get("ac_verified").and_then(|v| v.as_u64()), Some(0));
}
