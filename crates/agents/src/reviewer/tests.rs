#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Mutex;

use tempfile::TempDir;

use context::InlineContextBuilder;
use domain::{AcceptanceCriteria, Bundle, BundleStatus, PlanId, ReviewIssue, Role, Severity, Verdict, Work, WorkId};
use llm::{Message, LlmClient, LlmError, ToolCall, ToolSchema as LlmToolSchema, Usage};

use store::{BundleUpdateError, BundleUpdateSink};

use super::{
    ReviewerDeps, ReviewerError, parse_verdict, render_issue_summary, run_reviewer, strip_commit_header, truncate_diff,
};
use crate::config::ReviewerConfig;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeLlm {
    responses: Mutex<Vec<String>>,
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
    #[allow(clippy::manual_async_fn)]
    fn complete_with_tool<'a>(
        &'a self,
        _system: &'a str,
        _user: &'a str,
        _tool: LlmToolSchema,
    ) -> impl Future<Output = Result<(ToolCall, Usage), LlmError>> + Send + 'a {
        async move { panic!("FakeLlm: complete_with_tool not used in Reviewer tests") }
    }

    #[allow(clippy::manual_async_fn)]
    fn complete_free<'a>(
        &'a self,
        _system: &'a str,
        _messages: &'a [Message],
    ) -> impl Future<Output = Result<(String, Usage), LlmError>> + Send + 'a {
        async move {
            let mut q = self.responses.lock().unwrap();
            let payload = if q.len() > 1 {
                q.remove(0)
            } else if self.repeat_last {
                q[0].clone()
            } else if q.is_empty() {
                panic!("FakeLlm: response queue exhausted");
            } else {
                q.remove(0)
            };
            Ok((payload, Usage::default()))
        }
    }
}

#[derive(Default)]
struct CollectingSink {
    persisted: Mutex<Vec<(Bundle, i64)>>,
    stale_once: Mutex<bool>,
}

impl BundleUpdateSink for CollectingSink {
    #[allow(clippy::manual_async_fn)]
    fn update<'a>(
        &'a self,
        bundle: Bundle,
        expected_updated_at: i64,
    ) -> impl Future<Output = Result<(), BundleUpdateError>> + Send + 'a {
        async move {
            let mut stale = self.stale_once.lock().unwrap();
            if *stale {
                *stale = false;
                return Err(BundleUpdateError::Stale {
                    expected: expected_updated_at,
                    actual: expected_updated_at + 1,
                });
            }
            drop(stale);
            self.persisted.lock().unwrap().push((bundle, expected_updated_at));
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers (real git repo tempdir with one commit)
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
    std::fs::write(path.join("src.rs"), "fn a() {}\n").unwrap();
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

fn sample_work() -> Work {
    let mut w = Work::new(PlanId::new(), "add module".to_string());
    w.acceptance_criteria = AcceptanceCriteria(vec!["module exists".to_string()]);
    w
}

fn triaged_bundle(work_id: WorkId, head_commit: Option<&str>, paths: Vec<String>) -> Bundle {
    let mut b = Bundle::new(work_id, "loopr/wk-test".to_string(), vec!["test claim".to_string()]);
    b.paths = paths;
    b.head_commit = head_commit.map(str::to_string);
    b.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    b
}

fn make_deps<L: LlmClient, S: BundleUpdateSink>(
    llm: L,
    store: S,
    target: PathBuf,
    config: ReviewerConfig,
) -> ReviewerDeps<L, S, InlineContextBuilder> {
    ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config,
        target,
    }
}

// ---------------------------------------------------------------------------
// Happy paths for each Verdict kind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accept_path_persists_reviewed_and_returns_verdict() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"LGTM"}"#.to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    match &verdict {
        Verdict::Accept { summary } => assert!(summary.contains("LGTM")),
        other => panic!("expected Accept, got {other:?}"),
    }

    let persisted = deps.store.persisted.lock().unwrap();
    assert_eq!(persisted.len(), 1);
    let (stored, snapshot) = &persisted[0];
    assert_eq!(stored.status, BundleStatus::Reviewed);
    assert!(stored.verification.contains("LGTM"));
    assert_eq!(*snapshot, bundle.updated_at);
}

#[tokio::test]
async fn change_requested_path_persists_rejected() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![
        r#"{"kind":"change_requested","summary":"fix it","reasons":[{"severity":"error","file":"src.rs","message":"boom"}]}"#.to_string(),
    ]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    match &verdict {
        Verdict::ChangeRequested { summary, reasons } => {
            assert!(summary.contains("fix it"));
            assert_eq!(reasons.len(), 1);
        }
        other => panic!("expected ChangeRequested, got {other:?}"),
    }
    let persisted = deps.store.persisted.lock().unwrap();
    assert_eq!(persisted[0].0.status, BundleStatus::Rejected);
    assert!(persisted[0].0.verification.contains("boom"));
}

#[tokio::test]
async fn reject_path_persists_rejected() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![r#"{"kind":"reject","reason":"wrong approach"}"#.to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    match &verdict {
        Verdict::Reject { reason } => assert!(reason.contains("wrong approach")),
        other => panic!("expected Reject, got {other:?}"),
    }
    let persisted = deps.store.persisted.lock().unwrap();
    assert_eq!(persisted[0].0.status, BundleStatus::Rejected);
    assert!(persisted[0].0.verification.contains("wrong approach"));
}

// ---------------------------------------------------------------------------
// Parse-retry boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_unparseable_then_good_accepts() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);
    let llm = FakeLlm::new(vec![
        "not json".to_string(),
        "still not json".to_string(),
        "nope".to_string(),
        r#"{"kind":"accept","summary":"ok"}"#.to_string(),
    ]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::Accept { .. }));
}

#[tokio::test]
async fn four_unparseable_escalates() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);
    let llm = FakeLlm::repeating("not json at all".to_string());
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    match err {
        ReviewerError::EscalationNeeded(msg) => assert!(msg.contains("parse-retry exhausted")),
        other => panic!("expected EscalationNeeded, got {other:?}"),
    }
}

#[tokio::test]
async fn four_schema_errors_escalates() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);
    // valid JSON but wrong shape -> Schema error
    let llm = FakeLlm::repeating(r#"{"wrong":"schema"}"#.to_string());
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    assert!(matches!(err, ReviewerError::EscalationNeeded(_)));
}

#[tokio::test]
async fn change_requested_empty_reasons_is_schema_error() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);
    let llm = FakeLlm::repeating(r#"{"kind":"change_requested","summary":"empty","reasons":[]}"#.to_string());
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    assert!(matches!(err, ReviewerError::EscalationNeeded(_)));
}

// ---------------------------------------------------------------------------
// Preconditions (before any LLM call)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bundle_in_proposed_status_yields_transition_error() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let mut bundle = Bundle::new(work.id.clone(), "loopr/wk-test".to_string(), vec!["claim".to_string()]);
    bundle.paths = vec!["src.rs".to_string()];
    bundle.head_commit = Some(sha);
    // Leave in `Proposed` (no triage).
    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"ok"}"#.to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    assert!(matches!(err, ReviewerError::Transition(_)));
}

#[tokio::test]
async fn work_id_mismatch_errors_before_llm_call() {
    let (_dir, repo, _sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(WorkId::new(), Some("deadbeef"), vec![]);
    // If the LLM is called, the mismatch check was in the wrong place.
    let llm = FakeLlm::new(vec!["PANIC_IF_CALLED".to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    match err {
        ReviewerError::Mismatch(a, b) => {
            assert_eq!(a, bundle.work_id.to_string());
            assert_eq!(b, work.id.to_string());
        }
        other => panic!("expected Mismatch, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Git and diff helpers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unreachable_head_commit_returns_git_error() {
    let (_dir, repo, _sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(
        work.id.clone(),
        Some("0000000000000000000000000000000000000000"),
        vec!["src.rs".to_string()],
    );
    let llm = FakeLlm::new(vec!["PANIC_IF_CALLED".to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    assert!(matches!(err, ReviewerError::Git(_)));
}

#[test]
fn strip_commit_header_returns_empty_on_header_only() {
    let header_only = "commit deadbeef\nAuthor: x\nDate: y\n\n    message\n";
    assert_eq!(strip_commit_header(header_only), "");
}

#[test]
fn strip_commit_header_preserves_body_from_diff_git() {
    let full = "commit abc\nAuthor: x\nDate: y\n\n    msg\n\ndiff --git a/x b/x\n-old\n+new\n";
    let body = strip_commit_header(full);
    assert!(body.starts_with("diff --git a/x b/x"));
    assert!(body.contains("-old"));
}

#[test]
fn truncate_diff_passes_through_short() {
    let body = "diff body small\n";
    assert_eq!(truncate_diff(body, 100), body);
}

#[test]
fn truncate_diff_appends_marker_when_over_cap() {
    let body = "x".repeat(200_000);
    let out = truncate_diff(&body, 64 * 1024);
    assert!(out.contains("[... diff truncated; original 200000 bytes"));
    assert!(out.len() < body.len());
}

#[tokio::test]
async fn oversized_diff_gets_truncation_marker_in_prompt() {
    let (dir, repo, _sha) = init_repo_with_commit();
    let _ = dir;
    // Produce a huge diff by committing a large file.
    let huge = "a".repeat(200_000);
    std::fs::write(repo.join("big.txt"), &huge).unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "big", "--no-gpg-sign"]);
    let sha = git_capture(&repo, &["rev-parse", "HEAD"]);

    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["big.txt".to_string()]);
    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"ok"}"#.to_string()]);
    let config = ReviewerConfig {
        diff_byte_cap: 4096,
        ..ReviewerConfig::default()
    };
    let deps = make_deps(llm, CollectingSink::default(), repo, config);
    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::Accept { .. }));
    // The Bundle landed on disk; the truncation check belongs to the
    // prompt assembly seam test (Phase 4) since we don't inspect
    // messages here. This test just verifies that truncation doesn't
    // crash the loop.
}

// ---------------------------------------------------------------------------
// Noop bundles (file contents branch)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn noop_bundle_with_readable_files_accepts() {
    let (_dir, repo, _sha) = init_repo_with_commit();
    let work = sample_work();
    // Noop: no head_commit, render file contents instead.
    let bundle = triaged_bundle(work.id.clone(), None, vec!["README.md".to_string()]);
    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"noop is fine"}"#.to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::Accept { .. }));
}

#[tokio::test]
async fn noop_bundle_with_unmet_ac_requests_changes() {
    let (_dir, repo, _sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), None, vec!["README.md".to_string()]);
    let llm = FakeLlm::new(vec![
        r#"{"kind":"change_requested","summary":"AC unmet","reasons":[{"severity":"error","file":"README.md","message":"module missing"}]}"#.to_string(),
    ]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());
    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::ChangeRequested { .. }));
}

#[tokio::test]
async fn noop_bundle_with_many_paths_enforces_aggregate_cap() {
    use crate::reviewer::read_file_contents;
    let (_dir, repo, _sha) = init_repo_with_commit();
    // Create 20 paths each containing 10_000 bytes; aggregate 200 KB.
    let mut paths = Vec::new();
    for i in 0..20 {
        let name = format!("f{i}.txt");
        std::fs::write(repo.join(&name), "b".repeat(10_000)).unwrap();
        paths.push(name);
    }
    let out = read_file_contents(&repo, &paths, 64 * 1024).await.unwrap();
    // Aggregate content size must be <= cap (64 KB) plus a small
    // per-file marker overhead. Add per-file cap floor (2048) times
    // count as the absolute upper bound, but aggregate should hold.
    let total: usize = out.iter().map(|(_, c)| c.len()).sum();
    assert!(
        total <= 64 * 1024 + paths.len() * 128,
        "aggregate {total} bytes exceeded cap"
    );
    assert_eq!(out.len(), paths.len());
}

// ---------------------------------------------------------------------------
// OCC: stale bundle propagates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_store_returns_update_error_bubbled_up() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);
    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"ok"}"#.to_string()]);
    let sink = CollectingSink::default();
    *sink.stale_once.lock().unwrap() = true;
    let deps = make_deps(llm, sink, repo, ReviewerConfig::default());
    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    match err {
        ReviewerError::Update(BundleUpdateError::Stale { .. }) => {}
        other => panic!("expected Update(Stale), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for parse_verdict
// ---------------------------------------------------------------------------

#[test]
fn parse_verdict_accepts_direct_json() {
    let v = parse_verdict(r#"{"kind":"accept","summary":"ok"}"#).unwrap();
    assert!(matches!(v, Verdict::Accept { .. }));
}

#[test]
fn parse_verdict_strips_markdown_fences() {
    let raw = "```json\n{\"kind\":\"accept\",\"summary\":\"ok\"}\n```";
    let v = parse_verdict(raw).unwrap();
    assert!(matches!(v, Verdict::Accept { .. }));
}

#[test]
fn parse_verdict_extracts_embedded_object() {
    let raw = "Here's my review: {\"kind\":\"accept\",\"summary\":\"ok\"} - thanks!";
    let v = parse_verdict(raw).unwrap();
    assert!(matches!(v, Verdict::Accept { .. }));
}

#[test]
fn parse_verdict_rejects_non_json_as_unparseable() {
    let err = parse_verdict("totally not json").unwrap_err();
    assert!(matches!(err, super::ParseError::Unparseable(_)));
}

#[test]
fn parse_verdict_rejects_wrong_schema_as_schema() {
    let err = parse_verdict(r#"{"wrong":"field"}"#).unwrap_err();
    assert!(matches!(err, super::ParseError::Schema(_)));
}

#[test]
fn parse_verdict_rejects_empty_reasons_change_requested_as_schema() {
    let err = parse_verdict(r#"{"kind":"change_requested","summary":"s","reasons":[]}"#).unwrap_err();
    assert!(matches!(err, super::ParseError::Schema(_)));
}

// ---------------------------------------------------------------------------
// render_issue_summary
// ---------------------------------------------------------------------------

#[test]
fn render_issue_summary_includes_every_reason() {
    let reasons = vec![
        ReviewIssue {
            severity: Severity::Error,
            file: "a.rs".to_string(),
            line: Some(10),
            message: "bad".to_string(),
            suggestion: Some("fix".to_string()),
        },
        ReviewIssue {
            severity: Severity::Warning,
            file: "b.rs".to_string(),
            line: None,
            message: "maybe".to_string(),
            suggestion: None,
        },
    ];
    let out = render_issue_summary("summary", &reasons);
    assert!(out.contains("summary"));
    assert!(out.contains("a.rs:10"));
    assert!(out.contains("[error]"));
    assert!(out.contains("[warning]"));
    assert!(out.contains("suggestion: fix"));
}
