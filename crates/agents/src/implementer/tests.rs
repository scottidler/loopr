#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Mutex;

use tempfile::TempDir;

use context::{InlineContextBuilder, StateSummary};
use domain::{BundleId, PlanId, Work, WorkId};
use llm::{LlmClient, LlmError, Message, ToolCall, ToolSchema as LlmToolSchema, Usage};
use worktree::Worktree;

use super::{BundleSink, BundleSinkError, Deps, ImplementerError, run_implementer};
use crate::config::ImplementerConfig;
use crate::dispatch::{DispatchError, ToolExecutor};

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

struct FakeLlm {
    responses: Mutex<Vec<String>>,
    /// If true, loop the final response forever once the queue is empty.
    repeat_last: bool,
}

impl FakeLlm {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses),
            repeat_last: false,
        }
    }

    fn repeating(response: String) -> Self {
        Self {
            responses: Mutex::new(vec![response]),
            repeat_last: true,
        }
    }
}

impl LlmClient for FakeLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("FakeLlm: complete_with_tool not used in Implementer tests")
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        let mut q = self.responses.lock().unwrap();
        let payload = if q.len() > 1 {
            q.remove(0)
        } else if self.repeat_last {
            q[0].clone()
        } else if q.is_empty() {
            panic!("FakeLlm: response queue exhausted (set repeat_last if you wanted looping)");
        } else {
            q.remove(0)
        };
        Ok((payload, Usage::default()))
    }
}

struct FakeTools;

impl ToolExecutor for FakeTools {
    async fn execute(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<String, DispatchError> {
        Ok("ok".to_string())
    }
}

#[derive(Default)]
struct CollectingSink {
    persisted: Mutex<Vec<domain::Bundle>>,
}

impl BundleSink for CollectingSink {
    async fn persist(&self, bundle: domain::Bundle) -> Result<BundleId, BundleSinkError> {
        let id = bundle.id.clone();
        self.persisted.lock().unwrap().push(bundle);
        Ok(id)
    }
}

fn init_repo() -> (TempDir, PathBuf, String) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().to_path_buf();
    run(&path, &["init", "-q", "-b", "main"]);
    run(&path, &["config", "user.email", "test@example.com"]);
    run(&path, &["config", "user.name", "Test"]);
    run(&path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "initial\n").unwrap();
    run(&path, &["add", "-A"]);
    run(&path, &["commit", "-q", "-m", "initial", "--no-gpg-sign"]);
    let sha = run_capture(&path, &["rev-parse", "HEAD"]);
    (dir, path, sha)
}

fn run(path: &Path, args: &[&str]) {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn run_capture(path: &Path, args: &[&str]) -> String {
    let out = StdCommand::new("git").arg("-C").arg(path).args(args).output().unwrap();
    assert!(out.status.success(), "git {:?} failed", args);
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn fake_worktree(repo_path: &Path, sha: &str, work_id: WorkId) -> Worktree {
    let worktree_root = repo_path.parent().unwrap().join("wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    Worktree::create(repo_path, &worktree_root, work_id, sha).unwrap()
}

fn make_work() -> Work {
    Work::new(PlanId::new(), "ship a feature".to_string())
}

fn make_deps<L: LlmClient>(
    llm: L,
    config: ImplementerConfig,
) -> Deps<L, FakeTools, CollectingSink, InlineContextBuilder> {
    Deps {
        llm,
        tools: FakeTools,
        bundles: CollectingSink::default(),
        context: InlineContextBuilder::new(),
        config,
        tool_schemas: vec![],
        state: StateSummary::default(),
        run_id: None,
    }
}

// ---------------------------------------------------------------------------
// Integration scenarios from the design doc, Phase 4.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_propose_bundle_on_first_call() {
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    // Finding 1: propose_bundle now rejects a propose with no new
    // commits. Make a dirty in-scope change so propose_bundle's staging
    // step commits it and HEAD advances past base.
    std::fs::write(wt.path().join("feature.rs"), "fn main() {}\n").unwrap();

    let llm = FakeLlm::new(vec![
        r#"[{"type":"propose_bundle","claims":["it compiles"]}]"#.to_string(),
    ]);
    let deps = make_deps(llm, ImplementerConfig::default());

    let bundle = run_implementer(&work, &wt, &deps).await.unwrap();
    assert_eq!(bundle.work_id, work.id);
    assert_eq!(bundle.claims, vec!["it compiles".to_string()]);
    assert!(!bundle.force_proposed);
    // Finding 4: base sha persisted so the reviewer can diff base..head.
    assert_eq!(bundle.base_commit.as_deref(), Some(base.as_str()));
    assert!(bundle.head_commit.is_some());
    assert_eq!(deps.bundles.persisted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn force_propose_fires_at_iteration_cap() {
    // LLM emits a parseable run_tool forever but never proposes.
    // Need a bigger max_repeat_action so we don't escalate via the
    // repeat-action path before hitting the iteration cap.
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    // Make a tracked modification so force-propose has something to commit.
    std::fs::write(wt.path().join("README.md"), "modified\n").unwrap();

    let llm = FakeLlm::repeating(r#"[{"type":"run_tool","tool":"echo","input":{"arg":"a"}}]"#.to_string());
    let config = ImplementerConfig {
        max_iterations: 3,
        max_repeat_action: 100,
        ..ImplementerConfig::default()
    };
    let deps = make_deps(llm, config);

    let bundle = run_implementer(&work, &wt, &deps).await.unwrap();
    assert!(bundle.force_proposed, "force_proposed must be true on iteration-cap");
    assert_eq!(bundle.work_id, work.id);
    // A commit should have been made since we had a tracked modification.
    assert!(bundle.head_commit.is_some());
    // Finding 3: a force-proposed bundle must populate paths (the bundle
    // class most deserving scrutiny) so the reviewer's diff filter and
    // the integrator's collision detector see the change.
    assert_eq!(bundle.paths, vec!["README.md".to_string()], "force-propose paths from branch diff");
    // Finding 4: base sha persisted for the reviewer's range diff.
    assert_eq!(bundle.base_commit.as_deref(), Some(base.as_str()));
    assert_eq!(deps.bundles.persisted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn garbage_output_escalates_via_lifeguard() {
    // LLM returns unparseable junk forever. Each iteration burns its
    // requery budget and records a parse failure. Escalation fires
    // after max_parse_failures consecutive iterations.
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    let llm = FakeLlm::repeating("this is not JSON at all".to_string());
    let config = ImplementerConfig {
        max_iterations: 50,
        max_parse_failures: 3,
        max_requeries: 1,
        ..ImplementerConfig::default()
    };
    let deps = make_deps(llm, config);

    let err = run_implementer(&work, &wt, &deps).await.unwrap_err();
    match err {
        ImplementerError::EscalationNeeded(reason) => {
            assert!(
                reason.contains("unparseable") || reason.contains("consecutive"),
                "unexpected escalation reason: {reason}"
            );
        }
        other => panic!("expected EscalationNeeded, got {other:?}"),
    }
}

#[tokio::test]
async fn need_help_escalates_with_partial_commit() {
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    // Modify a tracked file so need_help's partial commit has something to save.
    std::fs::write(wt.path().join("README.md"), "partial\n").unwrap();

    let llm = FakeLlm::new(vec![
        r#"[{"type":"need_help","reason":"blocked on specs"}]"#.to_string(),
    ]);
    let deps = make_deps(llm, ImplementerConfig::default());

    let err = run_implementer(&work, &wt, &deps).await.unwrap_err();
    match err {
        ImplementerError::EscalationNeeded(reason) => assert_eq!(reason, "blocked on specs"),
        other => panic!("expected EscalationNeeded, got {other:?}"),
    }
    // Partial commit must be visible in git log.
    let log = run_capture(wt.path(), &["log", "-1", "--format=%s"]);
    assert!(log.contains("partial"), "expected partial commit: {log}");
}

#[tokio::test]
async fn force_propose_guard_escalates_when_too_many_files() {
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    // First, seed the repo with 5 tracked files so we can modify
    // them later and hit the guard.
    for i in 0..5 {
        std::fs::write(wt.path().join(format!("f{i}.txt")), "v1\n").unwrap();
    }
    run(wt.path(), &["add", "-A"]);
    run(wt.path(), &["commit", "-q", "-m", "seed", "--no-gpg-sign"]);

    // Modify all 5.
    for i in 0..5 {
        std::fs::write(wt.path().join(format!("f{i}.txt")), "v2\n").unwrap();
    }

    let llm = FakeLlm::repeating(r#"[{"type":"run_tool","tool":"echo","input":{"arg":"a"}}]"#.to_string());
    let config = ImplementerConfig {
        max_iterations: 2,
        max_repeat_action: 100,
        max_force_propose_files: 2, // 5 modified > 2 → guard trips
        ..ImplementerConfig::default()
    };
    let deps = make_deps(llm, config);

    let err = run_implementer(&work, &wt, &deps).await.unwrap_err();
    match err {
        ImplementerError::EscalationNeeded(reason) => {
            assert!(
                reason.contains("force-propose guard"),
                "unexpected escalation reason: {reason}"
            );
            assert!(reason.contains("5"), "should mention file count: {reason}");
        }
        other => panic!("expected EscalationNeeded, got {other:?}"),
    }
    // No Bundle persisted - guard trip MUST NOT write zombie records.
    assert!(
        deps.bundles.persisted.lock().unwrap().is_empty(),
        "guard trip must not persist any bundle"
    );
}

#[tokio::test]
async fn done_action_persists_noop_bundle() {
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    let llm = FakeLlm::new(vec![r#"[{"type":"done","message":"nothing to do"}]"#.to_string()]);
    let deps = make_deps(llm, ImplementerConfig::default());

    let bundle = run_implementer(&work, &wt, &deps).await.unwrap();
    assert_eq!(bundle.noop_reason, Some("nothing to do".to_string()));
    assert!(!bundle.force_proposed);
    assert_eq!(deps.bundles.persisted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn parse_failure_then_recovery_resets_counter() {
    // Sequence: fail, parseable-but-not-bundle, fail, bundle.
    // max_parse_failures=2. Without reset the counter would reach 2
    // at iteration 3 and escalate. With reset, iteration 2's success
    // zeroes the counter so iteration 3's failure only brings it to
    // 1, and iteration 4's bundle wins. This PROVES the reset is
    // load-bearing.
    let (_dir, repo, base) = init_repo();
    let work = make_work();
    let wt = fake_worktree(&repo, &base, work.id.clone());

    // Finding 1: give propose_bundle a real change to commit.
    std::fs::write(wt.path().join("feature.rs"), "fn main() {}\n").unwrap();

    let llm = FakeLlm::new(vec![
        "garbage".to_string(),
        r#"[{"type":"run_tool","tool":"t","input":{}}]"#.to_string(),
        "more garbage".to_string(),
        r#"[{"type":"propose_bundle","claims":["done"]}]"#.to_string(),
    ]);
    let config = ImplementerConfig {
        max_iterations: 5,
        max_requeries: 1,
        max_parse_failures: 2,
        ..ImplementerConfig::default()
    };
    let deps = make_deps(llm, config);

    let bundle = run_implementer(&work, &wt, &deps).await.unwrap();
    assert_eq!(bundle.claims, vec!["done".to_string()]);
}
