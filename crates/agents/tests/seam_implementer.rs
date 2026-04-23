//! Seam test: full `run_implementer` round-trip against a real
//! `store::Store` and a real git repo fixture. Fake LLM returns a
//! two-step script (RunTool then ProposeBundle); dispatch and
//! Bundle persistence run for real.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Mutex;

use tempfile::TempDir;

use agents::{Deps, ImplementerConfig, ToolExecutor, run_implementer};
use context::{InlineContextBuilder, StateSummary};
use domain::Work;
use llm::ScriptedLlm;
use store::Store;
use worktree::Worktree;

struct RecordedTools {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

impl ToolExecutor for RecordedTools {
    #[allow(clippy::manual_async_fn)]
    fn execute<'a>(
        &'a self,
        tool_name: &'a str,
        input: &'a serde_json::Value,
        _working_dir: &'a Path,
    ) -> impl std::future::Future<Output = Result<String, agents::DispatchError>> + Send + 'a {
        async move {
            self.calls.lock().unwrap().push((tool_name.to_string(), input.clone()));
            Ok(format!("fake {tool_name} output"))
        }
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

#[tokio::test]
async fn full_roundtrip_bundle_persists_in_real_store() {
    let (_dir, repo_path, base_sha) = init_repo();

    // Real Store at .loopr/taskstore/ under the repo.
    let store = Store::open(&repo_path).await.unwrap();
    let plan = domain::Plan::new("seam test plan".to_string());
    store.plans().create(plan.clone()).await.unwrap();
    let work = Work::new(plan.id.clone(), "do the thing".to_string());
    store.works().create(work.clone()).await.unwrap();

    // Real worktree via the worktree crate, using the Work's ID.
    let worktree_root = repo_path.parent().unwrap().join("seam-wts");
    std::fs::create_dir_all(&worktree_root).unwrap();
    let wt = Worktree::create(&repo_path, &worktree_root, work.id.clone(), &base_sha).unwrap();

    // Fake LLM with a 2-step script.
    let llm = ScriptedLlm::new();
    llm.queue_free(Ok(
        r#"[{"type":"run_tool","tool":"bash","input":{"command":"echo hi"}}]"#.to_string(),
    ));
    llm.queue_free(Ok(r#"[{"type":"propose_bundle","claims":["did a thing"]}]"#.to_string()));

    let tools = RecordedTools {
        calls: Mutex::new(Vec::new()),
    };

    let deps = Deps {
        llm,
        tools,
        bundles: store,
        context: InlineContextBuilder::new(),
        config: ImplementerConfig::default(),
        tool_schemas: vec![],
        state: StateSummary::default(),
    };

    let bundle = run_implementer(&work, &wt, &deps).await.unwrap();

    // Bundle correctness.
    assert_eq!(bundle.work_id, work.id, "bundle.work_id must equal work.id");
    assert_eq!(bundle.claims, vec!["did a thing".to_string()]);
    assert!(!bundle.force_proposed);
    assert!(bundle.head_commit.is_some(), "head_commit captured");

    // Tool was invoked.
    {
        let calls = deps.tools.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bash");
        assert_eq!(calls[0].1, serde_json::json!({"command": "echo hi"}));
    }

    // Real store: Bundle is queryable by work_id.
    let stored = deps.bundles.bundles().list_by_work_id(&work.id).await.unwrap();
    assert_eq!(stored.len(), 1, "exactly one bundle for this work");
    assert_eq!(stored[0].id, bundle.id);
    assert_eq!(stored[0].claims, bundle.claims);
    deps.bundles.close().await.unwrap();
}
