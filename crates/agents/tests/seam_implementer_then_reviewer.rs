//! Integration seam test: pair a Stage-7 `run_implementer` run with a
//! Stage-8 `run_reviewer` run in one test. The Implementer writes a
//! Bundle; the daemon-role `Bundle::transition(Triaged,
//! Role::Reactor)` drives it to `Triaged`; the Reviewer reads it
//! and transitions. Asserts the full agent-to-agent handoff against a
//! real `store::Store` (TaskStore-backed tempdir).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use tempfile::TempDir;

use agents::{Deps, ImplementerConfig, ReviewerConfig, ReviewerDeps, ToolExecutor, run_implementer, run_reviewer};
use context::{InlineContextBuilder, StateSummary};
use domain::{AcceptanceCriteria, BundleStatus, Role, TargetKind, Verdict, Work};
use llm::ScriptedLlm;
use store::{BundleUpdateSink, Store};
use worktree::Worktree;

struct NoopTools;

impl ToolExecutor for NoopTools {
    async fn execute(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<String, agents::DispatchError> {
        Ok("ok".to_string())
    }
}

// ---------------------------------------------------------------------------
// Git setup
// ---------------------------------------------------------------------------

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
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// Full agent-to-agent handoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn implementer_writes_bundle_then_reviewer_accepts_it() {
    let (_dir, repo_path, sha) = init_repo();
    let store = Store::open(&repo_path).await.unwrap();

    // Set up Plan + Work.
    let plan = domain::Plan::new("integration plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let mut work = Work::new(plan.id.clone(), "ship a noop bundle".to_string());
    work.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["work completes".to_string()]);
    store.works().create(work.clone()).await.unwrap();

    // Worktree for the Implementer.
    let worktree_root = repo_path.parent().unwrap().join("impl-reviewer-wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    let wt = Worktree::create(&repo_path, &worktree_root, work.id.clone(), &sha).unwrap();

    // Finding 1: propose_bundle rejects a propose with no new commits.
    // Seed a dirty in-scope change for the staging step to commit.
    std::fs::write(wt.path().join("feature.rs"), "fn main() {}\n").unwrap();

    // Stage 7: Implementer emits a propose_bundle immediately.
    let impl_llm = ScriptedLlm::new();
    impl_llm.queue_free(Ok(
        r#"[{"type":"propose_bundle","claims":["work is done"]}]"#.to_string()
    ));
    let impl_deps = Deps {
        llm: impl_llm,
        tools: NoopTools,
        bundles: &store,
        context: InlineContextBuilder::new(),
        config: ImplementerConfig::default(),
        tool_schemas: vec![],
        state: StateSummary::default(),
        run_id: None,
    };
    let bundle = run_implementer(&work, &wt, &impl_deps).await.unwrap();
    assert_eq!(bundle.status, BundleStatus::Proposed);

    // Daemon (simulated): triage the Bundle with Role::Reactor,
    // persist via OCC update. This mirrors the Stage 8 wiring capstone.
    let persisted = store.bundles().get(&bundle.id).await.unwrap();
    let expected_updated_at = persisted.updated_at;
    let mut triaged = persisted.clone();
    triaged.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    BundleUpdateSink::update(&&store, triaged, expected_updated_at, Role::Reactor, TargetKind::Normal)
        .await
        .unwrap();

    let triaged_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(triaged_bundle.status, BundleStatus::Triaged);

    // Stage 8: Reviewer accepts.
    let rev_llm = ScriptedLlm::new();
    rev_llm.queue_free(Ok(r#"{"kind":"accept","summary":"work completes"}"#.to_string()));
    let rev_deps = ReviewerDeps {
        llm: rev_llm,
        store: &store,
        context: InlineContextBuilder::new(),
        config: ReviewerConfig::default(),
        target: repo_path.clone(),
        checkout_path: repo_path,
        ephemeral_checkout: false,
        check_runner: std::sync::Arc::new(agents::ProductionCheckRunner::new(
            std::sync::Arc::new(tools::LaneRouter::new(tools::SandboxMode::Off).unwrap()),
            None,
        )),
        path_deny_patterns: Vec::new(),
    };
    let verdict = run_reviewer(&triaged_bundle, &work, &rev_deps).await.unwrap();
    assert!(matches!(verdict, Verdict::Accept { .. }));

    // Real-store final state.
    let final_bundle = store.bundles().get(&bundle.id).await.unwrap();
    assert_eq!(final_bundle.status, BundleStatus::Reviewed);
    assert!(final_bundle.verification.contains("work completes"));

    store.close().await.unwrap();
}
