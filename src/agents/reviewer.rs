use std::sync::Arc;

use eyre::{Result, eyre};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::agents::AgentSession;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::context::ContextBuilder;
use crate::agents::implementer::LlmClient;
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::role::Role;
use crate::ipc::protocol::DaemonEvent;

/// Verdict from a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    RequestChanges,
    Reject,
}

impl std::fmt::Display for ReviewVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReviewVerdict::Approve => write!(f, "approve"),
            ReviewVerdict::RequestChanges => write!(f, "request_changes"),
            ReviewVerdict::Reject => write!(f, "reject"),
        }
    }
}

/// Severity of a review issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}

/// A single issue found during review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewIssue {
    pub severity: IssueSeverity,
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub message: String,
    #[serde(default)]
    pub suggestion: Option<String>,
}

/// Structured result from the Reviewer LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewResult {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub issues: Vec<ReviewIssue>,
    pub summary: String,
}

const SYSTEM_PROMPT: &str = r#"You are a Reviewer agent in the Loopr development orchestrator. Your role is to review a Bundle (proposed code change) and provide structured feedback.

## Review Criteria

1. **Correctness** — Does the code do what the WorkItem requires?
2. **Quality** — Is the code clean, idiomatic, and maintainable?
3. **Tests** — Are there adequate tests? Do they cover edge cases?
4. **Scope** — Does the change stay within the WorkItem's boundaries?
5. **Safety** — Are there security concerns (OWASP top 10, injection, etc.)?

## Output Format

Respond with ONLY valid JSON matching this schema:

{
  "verdict": "approve" | "request_changes" | "reject",
  "issues": [
    {
      "severity": "error" | "warning" | "info",
      "file": "path/to/file.rs",
      "line": 42,
      "message": "Description of the issue",
      "suggestion": "How to fix it (optional)"
    }
  ],
  "summary": "Overall review summary"
}"#;

/// Parse the LLM response into a ReviewResult.
/// Extracts the first JSON object found in the response text.
pub fn parse_review_result(response: &str) -> Result<ReviewResult> {
    // Try direct parse first
    if let Ok(result) = serde_json::from_str::<ReviewResult>(response) {
        return Ok(result);
    }

    // Try to find a JSON object in the response (may be wrapped in markdown code blocks)
    let trimmed = response.trim();
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        let json_slice = &trimmed[start..=end];
        if let Ok(result) = serde_json::from_str::<ReviewResult>(json_slice) {
            return Ok(result);
        }
    }

    Err(eyre!("failed to parse ReviewResult from LLM response"))
}

/// Run the full reviewer loop. Reviewers are typically single-iteration.
pub async fn run_reviewer(
    llm: &dyn LlmClient,
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &AgentRoleConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> Result<()> {
    let bundle_id = session
        .bundle_id
        .as_ref()
        .ok_or_else(|| eyre!("reviewer session missing bundle_id"))?
        .clone();

    // Reviewer is single-iteration: load context, call LLM, execute verdict.
    session.iteration = 1;
    info!(
        "Reviewer {} iteration 1 (max_iterations={})",
        session.id, config.max_iterations
    );

    let ctx = ContextBuilder::new(stores, Role::Reviewer)
        .load_bundle_hierarchy(&bundle_id)?
        .with_footer("Review this Bundle against the WorkItem requirements and review criteria above. Respond with ONLY valid JSON.".into());
    let work_item_title = ctx.work_item_title().unwrap_or("unknown").to_string();
    let assembled = ctx.build(SYSTEM_PROMPT);

    let response = llm.call(&assembled.system_prompt, &assembled.user_message).await?;
    let review = parse_review_result(&response)?;

    info!(
        "Reviewer {} verdict: {} ({} issues) — {}",
        session.id,
        review.verdict,
        review.issues.len(),
        review.summary
    );

    // Store review as a Learning scoped to the Bundle's work item
    let learning_content = format!(
        "Review of bundle {}: verdict={}, summary={}",
        bundle_id, review.verdict, review.summary
    );
    let learning_resp = bridge.request(
        "learning.create",
        serde_json::json!({
            "content": learning_content,
            "scope": "workitem",
            "source_id": work_item_title,
        }),
    );
    if learning_resp.is_error() {
        warn!(
            "Reviewer {} failed to create learning: {:?}",
            session.id, learning_resp.error
        );
    }

    // Execute verdict action via FSM transition
    match review.verdict {
        ReviewVerdict::Approve | ReviewVerdict::RequestChanges => {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Reviewed",
                    "role": "reviewer",
                }),
            );
            if resp.is_error() {
                warn!(
                    "Reviewer {} failed to transition bundle to Reviewed: {:?}",
                    session.id, resp.error
                );
                return Err(eyre!("failed to transition bundle: {:?}", resp.error));
            }
            info!(
                "Reviewer {} verdict={} on bundle {}",
                session.id, review.verdict, bundle_id
            );
        }
        ReviewVerdict::Reject => {
            let resp = bridge.request(
                "bundle.transition",
                serde_json::json!({
                    "id": bundle_id,
                    "target_status": "Rejected",
                    "role": "reviewer",
                }),
            );
            if resp.is_error() {
                warn!("Reviewer {} failed to reject bundle: {:?}", session.id, resp.error);
                return Err(eyre!("failed to reject bundle: {:?}", resp.error));
            }
            info!("Reviewer {} rejected bundle {}", session.id, bundle_id);
        }
    }

    let _ = event_tx.send(DaemonEvent::agent_iteration_completed(
        &session.id,
        1,
        &format!("{}: {}", review.verdict, review.summary),
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentSession, AgentType};
    use crate::config::{Config, ProjectConfig};
    use crate::domain::bundle::{Bundle, BundleStatus};
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::work_item::WorkItem;
    use crate::worktree::manager::WorktreeManager;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    /// Mock LLM client for review tests.
    struct MockReviewLlm {
        response: String,
    }

    impl MockReviewLlm {
        fn new(response: &str) -> Self {
            Self {
                response: response.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockReviewLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    struct FailingReviewLlm;

    #[async_trait]
    impl LlmClient for FailingReviewLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            Err(eyre!("LLM call failed"))
        }
    }

    fn setup_stores_with_bundle(dir: &Path) -> (Arc<Stores>, String) {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;

        // Populate hierarchy
        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let spec = Spec::new(plan_id, "Test Spec".into(), "A test spec".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let phase = Phase::new(spec_id, "Test Phase".into(), "A test phase".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let wi = WorkItem::new(phase_id.clone(), "Test WorkItem".into(), "Implement the feature".into());
        let wi_id = wi.id.clone();
        stores.work_items.write().unwrap().insert(wi.id.clone(), wi);

        // Create a bundle in Triaged state (ready for review)
        let mut bundle = Bundle::new(
            wi_id,
            Some("tick-001".into()),
            "feature/test".into(),
            "Added test module with basic functionality".into(),
        );
        bundle.status = BundleStatus::Triaged;
        bundle.touched_paths = vec!["src/test.rs".into(), "src/main.rs".into()];
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        // Add a learning
        let learning = Learning::new(
            phase_id,
            LearningScope::Phase,
            "Always check for edge cases in parsing".into(),
        );
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        (Arc::new(stores), bundle_id)
    }

    fn test_bridge(stores: Arc<Stores>, dir: &Path) -> (AgentIpcBridge, broadcast::Sender<DaemonEvent>) {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores, event_tx.clone(), worktree_mgr, Config::default());
        (bridge, event_tx)
    }

    // --- ReviewVerdict tests ---

    #[test]
    fn test_review_verdict_display() {
        assert_eq!(ReviewVerdict::Approve.to_string(), "approve");
        assert_eq!(ReviewVerdict::RequestChanges.to_string(), "request_changes");
        assert_eq!(ReviewVerdict::Reject.to_string(), "reject");
    }

    #[test]
    fn test_review_verdict_serde_roundtrip() {
        for verdict in [
            ReviewVerdict::Approve,
            ReviewVerdict::RequestChanges,
            ReviewVerdict::Reject,
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            let deserialized: ReviewVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(verdict, deserialized);
        }
    }

    // --- ReviewResult tests ---

    #[test]
    fn test_review_result_serde_approve() {
        let json = r#"{
            "verdict": "approve",
            "issues": [],
            "summary": "Looks good"
        }"#;
        let result: ReviewResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
        assert!(result.issues.is_empty());
        assert_eq!(result.summary, "Looks good");
    }

    #[test]
    fn test_review_result_serde_with_issues() {
        let json = r#"{
            "verdict": "request_changes",
            "issues": [
                {
                    "severity": "error",
                    "file": "src/foo.rs",
                    "line": 42,
                    "message": "Missing error handling",
                    "suggestion": "Add a match on the Result"
                },
                {
                    "severity": "warning",
                    "file": "src/bar.rs",
                    "message": "Consider using a more descriptive name"
                }
            ],
            "summary": "Needs some fixes"
        }"#;
        let result: ReviewResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::RequestChanges);
        assert_eq!(result.issues.len(), 2);
        assert_eq!(result.issues[0].severity, IssueSeverity::Error);
        assert_eq!(result.issues[0].line, Some(42));
        assert!(result.issues[1].line.is_none());
        assert!(result.issues[1].suggestion.is_none());
    }

    #[test]
    fn test_review_result_serde_reject() {
        let json = r#"{
            "verdict": "reject",
            "issues": [{"severity": "error", "file": "src/main.rs", "message": "Fundamentally wrong approach"}],
            "summary": "Does not meet requirements"
        }"#;
        let result: ReviewResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Reject);
        assert_eq!(result.issues.len(), 1);
    }

    // --- parse_review_result tests ---

    #[test]
    fn test_parse_review_result_direct_json() {
        let json = r#"{"verdict": "approve", "issues": [], "summary": "All good"}"#;
        let result = parse_review_result(json).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn test_parse_review_result_wrapped_in_code_block() {
        let response =
            "Here is my review:\n```json\n{\"verdict\": \"approve\", \"issues\": [], \"summary\": \"LGTM\"}\n```";
        let result = parse_review_result(response).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
        assert_eq!(result.summary, "LGTM");
    }

    #[test]
    fn test_parse_review_result_invalid() {
        let bad = "This is not JSON at all";
        assert!(parse_review_result(bad).is_err());
    }

    #[test]
    fn test_parse_review_result_wrong_schema() {
        let json = r#"{"wrong": "schema"}"#;
        assert!(parse_review_result(json).is_err());
    }

    // --- context builder integration tests ---

    #[test]
    fn test_context_builder_for_reviewer() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-ctx-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let ctx = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap()
            .with_footer("Review this Bundle.".into());
        assert_eq!(ctx.work_item_title(), Some("Test WorkItem"));

        let assembled = ctx.build(SYSTEM_PROMPT);
        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test WorkItem"));
        assert!(assembled.user_message.contains("Bundle Under Review"));
        assert!(assembled.system_prompt.contains("Reviewer agent"));
    }

    #[test]
    fn test_context_builder_missing_bundle() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-miss-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _) = setup_stores_with_bundle(&dir);

        let result = ContextBuilder::new(&stores, Role::Reviewer).load_bundle_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("bundle not found"));
    }

    // --- run_reviewer tests ---

    #[tokio::test]
    async fn test_run_reviewer_approve() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-approve-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();

        let llm = MockReviewLlm::new(r#"{"verdict": "approve", "issues": [], "summary": "LGTM"}"#);

        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id.clone());

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_ok(), "run_reviewer failed: {:?}", result.err());
        assert_eq!(session.iteration, 1);

        // Bundle should have been transitioned to Reviewed
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Reviewed);
    }

    #[tokio::test]
    async fn test_run_reviewer_reject() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-reject-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();

        let llm = MockReviewLlm::new(
            r#"{"verdict": "reject", "issues": [{"severity": "error", "file": "src/main.rs", "message": "Wrong approach"}], "summary": "Fundamentally wrong"}"#,
        );

        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id.clone());

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_ok());

        // Bundle should have been transitioned to Rejected
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Rejected);
    }

    #[tokio::test]
    async fn test_run_reviewer_request_changes() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-changes-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();

        let llm = MockReviewLlm::new(
            r#"{"verdict": "request_changes", "issues": [{"severity": "warning", "file": "src/lib.rs", "message": "Add more tests"}], "summary": "Needs work"}"#,
        );

        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id.clone());

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_ok());

        // Bundle should have been transitioned to Reviewed (request_changes still transitions)
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Reviewed);
    }

    #[tokio::test]
    async fn test_run_reviewer_missing_bundle_id() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-noid-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, _) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();
        let llm = MockReviewLlm::new("{}");

        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        // No bundle_id set

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing bundle_id"));
    }

    #[tokio::test]
    async fn test_run_reviewer_llm_failure() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-fail-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();

        let llm = FailingReviewLlm;
        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id);

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_reviewer_bad_response() {
        let dir = std::env::temp_dir().join(format!("loopr-rev-bad-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);
        let (bridge, event_tx) = test_bridge(stores.clone(), &dir);
        let config = AgentRoleConfig::default_reviewer();

        let llm = MockReviewLlm::new("Not valid JSON");
        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id);

        let result = run_reviewer(&llm, &mut session, &stores, &bridge, &config, &event_tx).await;
        assert!(result.is_err());
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_instructions() {
        assert!(SYSTEM_PROMPT.contains("Reviewer agent"));
        assert!(SYSTEM_PROMPT.contains("Correctness"));
        assert!(SYSTEM_PROMPT.contains("Quality"));
        assert!(SYSTEM_PROMPT.contains("Tests"));
        assert!(SYSTEM_PROMPT.contains("verdict"));
        assert!(SYSTEM_PROMPT.contains("approve"));
        assert!(SYSTEM_PROMPT.contains("request_changes"));
        assert!(SYSTEM_PROMPT.contains("reject"));
    }
}
