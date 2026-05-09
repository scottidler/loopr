//! Phase 4 contract test from
//! `docs/design/2026-05-09-comprehensive-telemetry.md`. Drives a real
//! `Worktree::create` against a tempdir-rooted git repo under the
//! production telemetry subscriber, then re-parses `events.log` and
//! asserts the documented spans surface their required fields.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

use domain::WorkId;
use worktree::Worktree;

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

// ---------- Fixture ----------

fn init_repo(repo: &Path) -> String {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    fs::write(repo.join("README.md"), "initial\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    git_capture(repo, &["rev-parse", "HEAD"])
}

fn git(path: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------- Scenario ----------

#[test]
fn worktree_create_visible() {
    let log_dir = TempDir::new().unwrap();
    let repo_dir = TempDir::new().unwrap();
    let repo: PathBuf = repo_dir.path().to_path_buf();
    let sha = init_repo(&repo);
    let worktree_root = repo.join(".loopr").join("worktrees");

    let work_id = WorkId::new();

    {
        let _guard = telemetry::init_for_test(log_dir.path(), "debug").expect("init_for_test");
        let _wt = Worktree::create(&repo, &worktree_root, work_id.clone(), &sha).expect("create ok");
    }

    let events = read_events(log_dir.path());
    // The outer worktree.create span must surface its open-time scope
    // fields; the post-allocation `seq` and `branch` are recorded after
    // the seq-loop succeeds.
    assert_event(
        &events,
        "worktree.create",
        &["work_id", "repo_path", "worktree_root", "base_sha", "seq", "branch"],
    );
    // The inner ops.try_create_at_seq + ops.prune spans must also be
    // grep-able now that they emit inline events.
    assert_event(
        &events,
        "worktree.ops.try_create_at_seq",
        &["work_id", "seq", "base_sha"],
    );
}
