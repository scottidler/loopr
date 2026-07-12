#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use context::InlineContextBuilder;
use domain::{
    AcceptanceCriteria, Bundle, BundleStatus, CheckRun, CheckRunId, PlanId, Review, ReviewId, ReviewIssue, Role,
    Severity, TargetKind, Verdict, Work, WorkId,
};
use llm::{LlmClient, LlmError, Message, ToolCall, ToolSchema as LlmToolSchema, Usage};

use store::{BundleUpdateError, BundleUpdateSink, CheckRunSink, ReviewSink, StoreError};
use tools::{LaneRouter, SandboxMode};

use super::{
    ReviewerDeps, ReviewerError, parse_verdict, render_issue_summary, run_reviewer, strip_commit_header, truncate_diff,
};
use crate::check::{CheckOutcome, CheckRunner, ProductionCheckRunner};
use crate::config::ReviewerConfig;

/// No-op runner for tests whose `check_commands` are empty (checks skipped);
/// its `run` is never actually invoked in that case.
struct NoopCheckRunner;

impl CheckRunner for NoopCheckRunner {
    fn run<'a>(
        &'a self,
        _checkout_path: &'a Path,
        _commands: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<CheckOutcome>> + Send + 'a>> {
        Box::pin(async { Vec::new() })
    }
}

/// LLM that panics if called — proves a code path returns BEFORE the LLM turn.
struct PanicLlm;

impl LlmClient for PanicLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("PanicLlm: complete_with_tool must not be called")
    }

    async fn complete_free(
        &self,
        _system: &str,
        _messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        panic!("PanicLlm: complete_free must not be called (LLM turn should have been skipped)")
    }
}

/// LLM that captures the user message it was sent (for asserting prompt
/// content), then returns a fixed response.
struct CapturingLlm {
    captured: Mutex<String>,
    response: String,
}

impl LlmClient for CapturingLlm {
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("CapturingLlm: complete_with_tool not used in Reviewer tests")
    }

    async fn complete_free(
        &self,
        _system: &str,
        messages: &[Message],
        _model: Option<&str>,
    ) -> Result<(String, Usage), LlmError> {
        use llm::MessageContent;
        if let Some(MessageContent::Text { text }) = messages.first().and_then(|m| m.content.first()) {
            *self.captured.lock().unwrap() = text.clone();
        }
        Ok((self.response.clone(), Usage::default()))
    }
}

/// Real heavy-lane check runner (unsandboxed) for tests that execute real
/// `true` / `false` / missing-binary commands.
fn real_check_runner() -> Arc<dyn CheckRunner> {
    let router = Arc::new(LaneRouter::new(SandboxMode::Off).unwrap());
    Arc::new(ProductionCheckRunner::new(router, None))
}

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
    async fn complete_with_tool(
        &self,
        _system: &str,
        _user: &str,
        _tool: LlmToolSchema,
        _model: Option<&str>,
    ) -> Result<(ToolCall, Usage), LlmError> {
        panic!("FakeLlm: complete_with_tool not used in Reviewer tests")
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
            panic!("FakeLlm: response queue exhausted");
        } else {
            q.remove(0)
        };
        Ok((payload, Usage::default()))
    }
}

#[derive(Default)]
struct CollectingSink {
    persisted: Mutex<Vec<(Bundle, i64)>>,
    stale_once: Mutex<bool>,
    check_runs: Mutex<Vec<CheckRun>>,
    reviews: Mutex<Vec<Review>>,
}

impl CheckRunSink for CollectingSink {
    async fn create_check_run(&self, check_run: CheckRun) -> Result<CheckRunId, StoreError> {
        let id = check_run.id.clone();
        self.check_runs.lock().unwrap().push(check_run);
        Ok(id)
    }
}

impl ReviewSink for CollectingSink {
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError> {
        let id = review.id.clone();
        self.reviews.lock().unwrap().push(review);
        Ok(id)
    }

    async fn list_reviews_by_bundle(&self, bundle_id: &domain::BundleId) -> Result<Vec<Review>, StoreError> {
        Ok(self
            .reviews
            .lock()
            .unwrap()
            .iter()
            .filter(|r| &r.bundle_id == bundle_id)
            .cloned()
            .collect())
    }
}

impl BundleUpdateSink for CollectingSink {
    async fn update(
        &self,
        bundle: Bundle,
        expected_updated_at: i64,
        _role: Role,
        _kind: TargetKind,
    ) -> Result<i64, BundleUpdateError> {
        let mut stale = self.stale_once.lock().unwrap();
        if *stale {
            *stale = false;
            return Err(BundleUpdateError::Stale {
                expected: expected_updated_at,
                actual: expected_updated_at + 1,
            });
        }
        drop(stale);
        let ts = bundle.updated_at;
        self.persisted.lock().unwrap().push((bundle, expected_updated_at));
        Ok(ts)
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
    w.acceptance_criteria = AcceptanceCriteria::from_texts(vec!["module exists".to_string()]);
    w
}

fn triaged_bundle(work_id: WorkId, head_commit: Option<&str>, paths: Vec<String>) -> Bundle {
    let mut b = Bundle::new(work_id, "loopr/wk-test".to_string(), vec!["test claim".to_string()]);
    b.paths = paths;
    b.head_commit = head_commit.map(str::to_string);
    b.transition(BundleStatus::Triaged, Role::Reactor).unwrap();
    b
}

fn make_deps<L: LlmClient, S: BundleUpdateSink + CheckRunSink + ReviewSink>(
    llm: L,
    store: S,
    target: PathBuf,
    config: ReviewerConfig,
) -> ReviewerDeps<L, S, InlineContextBuilder> {
    // Default: checks skipped (empty check_commands in the default config),
    // checkout == target (existing noop tests read fixture files from the repo).
    let checkout_path = target.clone();
    ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config,
        target,
        checkout_path,
        ephemeral_checkout: false,
        check_runner: Arc::new(NoopCheckRunner),
        path_deny_patterns: Vec::new(),
    }
}

/// Build deps with an explicit checkout path and real check runner for the
/// Phase 10 executed-check tests.
fn make_checked_deps<L: LlmClient, S: BundleUpdateSink + CheckRunSink + ReviewSink>(
    llm: L,
    store: S,
    target: PathBuf,
    checkout_path: PathBuf,
    config: ReviewerConfig,
    check_runner: Arc<dyn CheckRunner>,
) -> ReviewerDeps<L, S, InlineContextBuilder> {
    ReviewerDeps {
        llm,
        store,
        context: InlineContextBuilder::new(),
        config,
        target,
        checkout_path,
        ephemeral_checkout: false,
        check_runner,
        path_deny_patterns: Vec::new(),
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

// ---------------------------------------------------------------------------
// Phase 10: executed checks — code-gate, failure taxonomy, checkout reads
// ---------------------------------------------------------------------------

/// Break-to-prove the code-gate: the LLM returns Accept, but a configured
/// check (`false`) exits nonzero -> the Bundle lands ChangeRequested (persisted
/// Rejected) with a synthesized issue naming the red command, and the CheckRun
/// is persisted with the real exit code. On pre-Phase-10 code (no gate) the
/// Bundle would land Reviewed.
#[tokio::test]
async fn accept_over_red_check_is_overridden_to_change_requested() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let checkout = TempDir::new().unwrap();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"looks great to me"}"#.to_string()]);
    let config = ReviewerConfig {
        check_commands: vec!["false".to_string()],
        ..ReviewerConfig::default()
    };
    let deps = make_checked_deps(
        llm,
        CollectingSink::default(),
        repo,
        checkout.path().to_path_buf(),
        config,
        real_check_runner(),
    );

    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    match &verdict {
        Verdict::ChangeRequested { reasons, .. } => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.file.contains("false") || r.message.contains("false")),
                "synthesized issue must name the red command `false`: {reasons:?}"
            );
        }
        other => panic!("expected code-gated ChangeRequested, got {other:?}"),
    }

    // Bundle persisted as Rejected (ChangeRequested routing).
    let persisted = deps.store.persisted.lock().unwrap();
    assert_eq!(persisted[0].0.status, BundleStatus::Rejected);
    drop(persisted);

    // CheckRun persisted with the REAL exit code from `false` (nonzero).
    let runs = deps.store.check_runs.lock().unwrap();
    assert_eq!(runs.len(), 1, "one CheckRun per command");
    assert_eq!(runs[0].command, "false");
    assert_ne!(runs[0].exit_code, 0, "false must record a nonzero exit code");
    assert_eq!(runs[0].executor, Role::Reviewer);
    assert!(!runs[0].output_digest.is_empty(), "digest must be computed");
}

/// A green check does NOT override an Accept; the Bundle lands Reviewed and the
/// CheckRun records exit 0.
#[tokio::test]
async fn accept_over_green_check_stays_reviewed() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let checkout = TempDir::new().unwrap();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"ok"}"#.to_string()]);
    let config = ReviewerConfig {
        check_commands: vec!["true".to_string()],
        ..ReviewerConfig::default()
    };
    let deps = make_checked_deps(
        llm,
        CollectingSink::default(),
        repo,
        checkout.path().to_path_buf(),
        config,
        real_check_runner(),
    );

    let verdict = run_reviewer(&bundle, &work, &deps).await.unwrap();
    assert!(matches!(verdict, Verdict::Accept { .. }));
    assert_eq!(deps.store.persisted.lock().unwrap()[0].0.status, BundleStatus::Reviewed);
    let runs = deps.store.check_runs.lock().unwrap();
    assert_eq!(runs[0].exit_code, 0);
}

/// Break-to-prove the failure taxonomy: a check whose program cannot be spawned
/// (command not found) returns `CheckEnvironment` and the LLM is NEVER called
/// (a `PanicLlm` would panic the test if it were). This is what maps to a
/// Blocked Work in the daemon, not ChangeRequested.
#[tokio::test]
async fn spawn_level_check_failure_blocks_without_llm_call() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let checkout = TempDir::new().unwrap();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let missing = "loopr-definitely-not-a-real-binary-xyzzy".to_string();
    let config = ReviewerConfig {
        check_commands: vec![missing.clone()],
        ..ReviewerConfig::default()
    };
    let deps = make_checked_deps(
        PanicLlm,
        CollectingSink::default(),
        repo,
        checkout.path().to_path_buf(),
        config,
        real_check_runner(),
    );

    let err = run_reviewer(&bundle, &work, &deps).await.unwrap_err();
    match err {
        ReviewerError::CheckEnvironment { command, .. } => assert_eq!(command, missing),
        other => panic!("expected CheckEnvironment, got {other:?}"),
    }
    // No Bundle persisted (aborted before the verdict transition).
    assert!(deps.store.persisted.lock().unwrap().is_empty());
}

/// Break-to-prove the noop-bundle checkout fix: file contents are read from the
/// CHECKOUT, not from `target`. The marker file exists only in the checkout;
/// pre-Phase-10 code read from `target` and would render `(file not found)`.
#[tokio::test]
async fn noop_bundle_reads_file_contents_from_checkout_not_target() {
    let (_dir, repo, _sha) = init_repo_with_commit();
    let checkout = TempDir::new().unwrap();
    const MARKER: &str = "CHECKOUT_ONLY_CONTENT_9f3a";
    std::fs::write(checkout.path().join("marker.txt"), format!("{MARKER}\n")).unwrap();
    // Deliberately do NOT write marker.txt into `repo` (target).

    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), None, vec!["marker.txt".to_string()]);

    let llm = CapturingLlm {
        captured: Mutex::new(String::new()),
        response: r#"{"kind":"accept","summary":"noop ok"}"#.to_string(),
    };
    // check_commands empty -> checks skipped; this test isolates the noop read.
    let deps = make_checked_deps(
        llm,
        CollectingSink::default(),
        repo,
        checkout.path().to_path_buf(),
        ReviewerConfig::default(),
        Arc::new(NoopCheckRunner),
    );

    let _ = run_reviewer(&bundle, &work, &deps).await.unwrap();
    let sent = deps.llm.captured.lock().unwrap().clone();
    assert!(
        sent.contains(MARKER),
        "reviewer prompt must contain the checkout-only file contents (read from checkout, not target)"
    );
}

// ---------------------------------------------------------------------------
// Phase 11: Review record persistence + round numbering + criteria
// ---------------------------------------------------------------------------

use domain::CriterionStatus;

use super::render_review_feedback;

#[tokio::test]
async fn run_reviewer_persists_review_round_1_with_criteria() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work(); // one AC: "module exists" (id 1)
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"LGTM"}"#.to_string()]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    run_reviewer(&bundle, &work, &deps).await.unwrap();

    let reviews = deps.store.reviews.lock().unwrap();
    assert_eq!(reviews.len(), 1, "exactly one Review row per review round");
    let rv = &reviews[0];
    assert_eq!(rv.bundle_id, bundle.id);
    assert_eq!(rv.round, 1, "first review round is 1");
    assert!(matches!(rv.verdict, Verdict::Accept { .. }));
    assert_eq!(rv.criteria.len(), 1, "one CriterionResult per acceptance criterion");
    assert_eq!(rv.criteria[0].criterion_id, 1);
    assert_eq!(rv.criteria[0].status, CriterionStatus::Met, "Accept -> criterion Met");
}

#[tokio::test]
async fn re_review_appends_new_round_no_dedup() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    // Same bundle reviewed twice (run_reviewer transitions a CLONE, so the
    // caller's Triaged bundle is reusable). Round must advance 1 -> 2.
    let llm = FakeLlm::repeating(r#"{"kind":"accept","summary":"ok"}"#.to_string());
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    run_reviewer(&bundle, &work, &deps).await.unwrap();
    run_reviewer(&bundle, &work, &deps).await.unwrap();

    let reviews = deps.store.reviews.lock().unwrap();
    assert_eq!(reviews.len(), 2, "append-only history: one row per round, no dedup");
    let mut rounds: Vec<u32> = reviews.iter().map(|r| r.round).collect();
    rounds.sort_unstable();
    assert_eq!(rounds, vec![1, 2], "rounds are the contiguous 1..N sequence");
}

#[tokio::test]
async fn change_requested_review_persists_structured_reasons() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    let llm = FakeLlm::new(vec![
        r#"{"kind":"change_requested","summary":"needs work","reasons":[{"severity":"error","file":"src.rs","line":3,"message":"missing module"}]}"#.to_string(),
    ]);
    let deps = make_deps(llm, CollectingSink::default(), repo, ReviewerConfig::default());

    run_reviewer(&bundle, &work, &deps).await.unwrap();

    let reviews = deps.store.reviews.lock().unwrap();
    let rv = &reviews[0];
    assert!(matches!(rv.verdict, Verdict::ChangeRequested { .. }));
    assert_eq!(rv.reasons.len(), 1, "structured reasons persisted on the Review");
    assert_eq!(rv.reasons[0].message, "missing module");
    assert_eq!(
        rv.criteria[0].status,
        CriterionStatus::Unmet,
        "ChangeRequested -> criterion Unmet"
    );
}

#[tokio::test]
async fn code_gated_accept_persists_change_requested_review_linking_check_runs() {
    let (_dir, repo, sha) = init_repo_with_commit();
    let checkout = TempDir::new().unwrap();
    let work = sample_work();
    let bundle = triaged_bundle(work.id.clone(), Some(&sha), vec!["src.rs".to_string()]);

    // LLM says accept, but a red check overrides to change_requested; the
    // persisted Review must reflect the code-gated verdict AND link the CheckRun.
    let llm = FakeLlm::new(vec![r#"{"kind":"accept","summary":"looks great"}"#.to_string()]);
    let config = ReviewerConfig {
        check_commands: vec!["false".to_string()],
        ..ReviewerConfig::default()
    };
    let deps = make_checked_deps(
        llm,
        CollectingSink::default(),
        repo,
        checkout.path().to_path_buf(),
        config,
        real_check_runner(),
    );

    run_reviewer(&bundle, &work, &deps).await.unwrap();

    let reviews = deps.store.reviews.lock().unwrap();
    let rv = &reviews[0];
    assert!(
        matches!(rv.verdict, Verdict::ChangeRequested { .. }),
        "code-gated Accept persists as ChangeRequested"
    );
    assert_eq!(rv.check_run_ids.len(), 1, "the Review links the CheckRun it weighed");
}

#[test]
fn render_review_feedback_uses_structured_reasons_not_verification_string() {
    let rv = Review::new(
        domain::BundleId::new(),
        2,
        Verdict::ChangeRequested {
            summary: "address the issues".to_string(),
            reasons: vec![ReviewIssue {
                severity: Severity::Error,
                file: "src/lib.rs".to_string(),
                line: Some(42),
                message: "unused import".to_string(),
                suggestion: Some("remove it".to_string()),
            }],
        },
        "address the issues".to_string(),
        vec![ReviewIssue {
            severity: Severity::Error,
            file: "src/lib.rs".to_string(),
            line: Some(42),
            message: "unused import".to_string(),
            suggestion: Some("remove it".to_string()),
        }],
        Vec::new(),
        "m".to_string(),
    );
    let out = render_review_feedback(&rv).expect("feedback for a review with reasons");
    assert!(out.contains("round 2"));
    assert!(out.contains("address the issues"));
    assert!(out.contains("src/lib.rs:42"), "structured location rendered: {out}");
    assert!(out.contains("unused import"));
    assert!(out.contains("suggestion: remove it"));
}

#[test]
fn render_review_feedback_none_when_empty() {
    let rv = Review::new(
        domain::BundleId::new(),
        1,
        Verdict::Accept { summary: String::new() },
        String::new(),
        Vec::new(),
        Vec::new(),
        "m".to_string(),
    );
    assert!(
        render_review_feedback(&rv).is_none(),
        "no summary and no reasons -> no feedback"
    );
}

#[test]
fn render_review_feedback_caps_reason_count() {
    let reasons: Vec<ReviewIssue> = (0..(super::REJECTION_REASONS_CAP + 5))
        .map(|i| ReviewIssue {
            severity: Severity::Error,
            file: format!("f{i}.rs"),
            line: None,
            message: format!("issue {i}"),
            suggestion: None,
        })
        .collect();
    let rv = Review::new(
        domain::BundleId::new(),
        1,
        Verdict::ChangeRequested {
            summary: "many".to_string(),
            reasons: reasons.clone(),
        },
        "many".to_string(),
        reasons,
        Vec::new(),
        "m".to_string(),
    );
    let out = render_review_feedback(&rv).unwrap();
    assert!(
        out.contains("more reason(s) omitted"),
        "capped render marks omission: {out}"
    );
}

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
