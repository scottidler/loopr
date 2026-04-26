//! Seam test: two concurrent `run_reviewer` tasks on the same Bundle
//! must produce exactly one OCC winner. The second writer gets
//! `ReviewerError::Update(BundleUpdateError::Stale { .. })` without
//! clobbering the first winner's verification. This pins the
//! intra-daemon race-defense invariant from the Reviewer design doc.

#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Barrier;

use agents::{ReviewerConfig, ReviewerDeps, ReviewerError, run_reviewer};
use context::InlineContextBuilder;
use domain::{AcceptanceCriteria, Bundle, BundleStatus, Role, Work};
use llm::{ChatMessage, LlmClient, LlmError, ToolCall, ToolSchema as LlmToolSchema, Usage};
use store::{BundleUpdateError, Store};

// Gated fake LLM: both tasks block on a shared barrier after entering
// `complete_free`, then proceed together. This forces both tasks to
// reach the Bundle mutation + store-update sequence concurrently, so
// the OCC Mutex has to pick a winner.
struct GatedLlm {
    barrier: Arc<Barrier>,
    response: String,
}

impl LlmClient for GatedLlm {
    #[allow(clippy::manual_async_fn)]
    fn complete_with_tool<'a>(
        &'a self,
        _system: &'a str,
        _user: &'a str,
        _tool: LlmToolSchema,
    ) -> impl Future<Output = Result<(ToolCall, Usage), LlmError>> + Send + 'a {
        async move { panic!("unused") }
    }

    #[allow(clippy::manual_async_fn)]
    fn complete_free<'a>(
        &'a self,
        _system: &'a str,
        _messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a {
        async move {
            self.barrier.wait().await;
            Ok((self.response.clone(), Usage::default()))
        }
    }
}

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
    std::fs::write(path.join("src.rs"), "fn a() {}\n").unwrap();
    git(&path, &["add", "-A"]);
    git(&path, &["commit", "-q", "-m", "add src", "--no-gpg-sign"]);
    let sha = git_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn git(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {:?}", args);
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_concurrent_reviewers_exactly_one_wins_occ() {
    let (_dir, repo_path, sha) = init_repo_with_commit();
    let store = Arc::new(Store::open(&repo_path).await.unwrap());

    let plan = domain::Plan::new("race plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "do race".to_string());
    work.acceptance_criteria = AcceptanceCriteria(vec!["ok".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    let mut bundle = Bundle::new(work.id.clone(), "loopr/wk-race".to_string(), vec!["claim".to_string()]);
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    bundle.transition(BundleStatus::Triaged, Role::Coordinator).unwrap();
    store.bundles().create(bundle.clone()).await.unwrap();

    let stored = store.bundles().get(&bundle.id).await.unwrap();

    let barrier = Arc::new(Barrier::new(2));

    let store1 = Arc::clone(&store);
    let barrier1 = Arc::clone(&barrier);
    let work1 = work.clone();
    let stored1 = stored.clone();
    let repo1 = repo_path.clone();

    let store2 = Arc::clone(&store);
    let barrier2 = Arc::clone(&barrier);
    let work2 = work.clone();
    let stored2 = stored.clone();
    let repo2 = repo_path.clone();

    let h1 = tokio::spawn(async move {
        let deps = ReviewerDeps {
            llm: GatedLlm {
                barrier: barrier1,
                response: r#"{"kind":"accept","summary":"task A"}"#.to_string(),
            },
            store: Arc::as_ref(&store1),
            context: InlineContextBuilder::new(),
            config: ReviewerConfig::default(),
            target: repo1,
        };
        run_reviewer(&stored1, &work1, &deps).await
    });

    let h2 = tokio::spawn(async move {
        let deps = ReviewerDeps {
            llm: GatedLlm {
                barrier: barrier2,
                response: r#"{"kind":"reject","reason":"task B"}"#.to_string(),
            },
            store: Arc::as_ref(&store2),
            context: InlineContextBuilder::new(),
            config: ReviewerConfig::default(),
            target: repo2,
        };
        run_reviewer(&stored2, &work2, &deps).await
    });

    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();

    let oks = [r1.is_ok(), r2.is_ok()].iter().filter(|b| **b).count();
    let stales = [&r1, &r2]
        .iter()
        .filter(|r| matches!(r, Err(ReviewerError::Update(BundleUpdateError::Stale { .. }))))
        .count();
    assert_eq!(
        oks,
        1,
        "expected exactly one success: r1={:?} r2={:?}",
        r1.as_ref().err(),
        r2.as_ref().err()
    );
    assert_eq!(stales, 1, "expected exactly one Stale");

    // Bundle on disk reflects the winner's outcome.
    let after = store.bundles().get(&bundle.id).await.unwrap();
    assert!(
        after.status == BundleStatus::Reviewed || after.status == BundleStatus::Rejected,
        "Bundle status must be one of the two outcomes, got {:?}",
        after.status
    );
    assert!(!after.verification.is_empty());

    // Take ownership of Store so we can close it cleanly. Both
    // join handles have already resolved above.
    drop(stored);
    let store = Arc::try_unwrap(store).ok().expect("unique Arc");
    store.close().await.unwrap();
}
