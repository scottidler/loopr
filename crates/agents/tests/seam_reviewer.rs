//! Seam test: full `run_reviewer` round-trip against a real
//! `store::Store` and a real git repo fixture. Fake LLM returns a
//! single `accept` verdict; git_show + strip_commit_header +
//! build_for_reviewer + OCC-aware persistence all run for real.

#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Mutex;

use tempfile::TempDir;

use agents::{ReviewerConfig, ReviewerDeps, run_reviewer};
use context::InlineContextBuilder;
use domain::{AcceptanceCriteria, Bundle, BundleStatus, Role, Verdict, Work};
use llm::{ChatMessage, LlmClient, LlmError, ToolCall, ToolSchema as LlmToolSchema};
use store::Store;

// ---------------------------------------------------------------------------
// Fake LLM
// ---------------------------------------------------------------------------

struct ScriptedLlm {
    responses: Mutex<Vec<String>>,
}

impl LlmClient for ScriptedLlm {
    #[allow(clippy::manual_async_fn)]
    fn complete_with_tool<'a>(
        &'a self,
        _system: &'a str,
        _user: &'a str,
        _tool: LlmToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + 'a {
        async move { panic!("seam test: complete_with_tool unused") }
    }

    #[allow(clippy::manual_async_fn)]
    fn complete_free<'a>(
        &'a self,
        _system: &'a str,
        _messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<String, LlmError>> + Send + 'a {
        async move {
            let mut q = self.responses.lock().unwrap();
            assert!(!q.is_empty(), "scripted llm exhausted");
            Ok(q.remove(0))
        }
    }
}

// ---------------------------------------------------------------------------
// Git helpers
// ---------------------------------------------------------------------------

fn init_repo_with_commit() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.email", "test@example.com"]);
    git(&path, &["config", "user.name", "Test"]);
    git(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    std::fs::write(path.join("src.rs"), "fn hello() -> u32 { 42 }\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "add src", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// Full seam round-trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_roundtrip_accept_verdict_persists_reviewed_bundle() {
    let (_dir, repo_path, sha) = init_repo_with_commit();

    // Real Store.
    let store = Store::open(&repo_path).await.unwrap();

    // Set up Plan + Work + Bundle via the real store.
    let plan = domain::Plan::new("seam reviewer plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "add hello()".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["hello returns 42".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(
        work.id.clone(),
        "loopr/wk-seam-1".to_string(),
        vec!["added hello".to_string()],
    );
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha.clone());
    // Triage the Bundle: the daemon would do this in wiring (using
    // Role::Coordinator as the identifier, per the Reviewer design
    // doc invariant).
    bundle.transition(BundleStatus::Triaged, Role::Coordinator).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    // Fake LLM returning an accept verdict.
    let llm = ScriptedLlm {
        responses: Mutex::new(vec![
            r#"{"kind":"accept","summary":"AC met: hello returns 42"}"#.to_string(),
        ]),
    };

    let deps = ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path.clone(),
    };

    // Re-fetch the Bundle so the `updated_at` we pass matches what is
    // on disk (the create path may have bumped it minimally; the
    // seam exercises the real OCC snapshot).
    let stored_before = deps.store.bundles().get(&bundle.id).await.unwrap();

    let verdict = run_reviewer(&stored_before, &work, &deps).await.unwrap();
    match &verdict {
        Verdict::Accept { summary } => assert!(summary.contains("42")),
        other => panic!("expected Accept, got {other:?}"),
    }

    // Real store: the persisted Bundle is Reviewed and carries a
    // verification summary.
    let stored_after = deps.store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(stored_after.status, BundleStatus::Reviewed);
    assert!(!stored_after.verification.is_empty());
    assert!(stored_after.verification.contains("AC met"));
    assert!(stored_after.updated_at >= stored_before.updated_at);

    deps.store.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// Change-requested with real diff extraction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn change_requested_verdict_persists_rejected_bundle() {
    let (_dir, repo_path, sha) = init_repo_with_commit();
    let store = Store::open(&repo_path).await.unwrap();

    let plan = domain::Plan::new("plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "add hello()".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["hello returns 99".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(
        work.id.clone(),
        "loopr/wk-seam-2".to_string(),
        vec!["added hello".to_string()],
    );
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Coordinator).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    let llm = ScriptedLlm {
        responses: Mutex::new(vec![
            r#"{"kind":"change_requested","summary":"AC says 99 not 42","reasons":[{"severity":"error","file":"src.rs","line":1,"message":"returns 42 not 99"}]}"#.to_string(),
        ]),
    };

    let deps = ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path,
    };

    let stored_before = deps.store.bundles().get(&bundle.id).await.unwrap();
    let verdict = run_reviewer(&stored_before, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::ChangeRequested { .. }));

    let stored_after = deps.store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(stored_after.status, BundleStatus::Rejected);
    assert!(stored_after.verification.contains("returns 42 not 99"));

    deps.store.close().await.unwrap();
}
