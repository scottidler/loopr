use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::agents::context::ContextBuilder;
use crate::agents::implementer::{ChatMessage, LlmClient};
use crate::agents::{Agent, AgentContext, AgentKind};
use crate::config::AgentRoleConfig;
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

/// Parse the LLM response into a ReviewResult.
/// Extracts the first JSON object found in the response text.
pub fn parse_review_result(response: &str, log: &crate::agents::agent_logger::AgentLogger) -> Result<ReviewResult> {
    log.debug(&format!("parse_review_result(response_len={})", response.len()));
    if let Ok(result) = serde_json::from_str::<ReviewResult>(response) {
        return Ok(result);
    }

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

/// The Reviewer agent — single-iteration LLM agent.
pub struct ReviewerAgent<L: LlmClient> {
    pub ctx: AgentContext,
    llm: L,
    config: AgentRoleConfig,
    bundle_id: String,
}

impl<L: LlmClient> ReviewerAgent<L> {
    pub fn new(ctx: AgentContext, llm: L, config: AgentRoleConfig) -> Result<Self> {
        let bundle_id = ctx
            .session
            .bundle_id
            .as_ref()
            .ok_or_else(|| eyre!("reviewer session missing bundle_id"))?
            .clone();
        Ok(Self {
            ctx,
            llm,
            config,
            bundle_id,
        })
    }
}

impl<L: LlmClient + 'static> Agent for ReviewerAgent<L> {
    async fn run(&mut self) -> Result<()> {
        self.ctx.debug(&format!(
            "run(session_id={}, bundle_id={})",
            self.ctx.session.id, self.bundle_id
        ));

        self.ctx.session.iteration = 1;
        self.ctx.info(&format!(
            "Reviewer iteration 1 (max_iterations={})",
            self.config.max_iterations
        ));

        let ctx_builder = ContextBuilder::new(&self.ctx.stores, Role::Reviewer)
            .load_bundle_hierarchy(&self.bundle_id)?
            .with_guidance(&self.ctx.stores.guidance)
            .with_footer(
                "Review this Bundle against the Work requirements and review criteria above. Respond with ONLY valid JSON."
                    .into(),
            );
        let work_title = ctx_builder.work_title().unwrap_or("unknown").to_string();
        let assembled = ctx_builder.build(&crate::prompts::store().reviewer)?;

        let review = {
            let mut messages = vec![ChatMessage::user(&assembled.user_message)];
            let mut requeries = 0u32;

            loop {
                let response = self.llm.call_with_history(&assembled.system_prompt, &messages).await?;
                self.ctx.log.write_iter_file(
                    requeries,
                    Some(&self.bundle_id),
                    &assembled.system_prompt,
                    &assembled.user_message,
                    &response,
                );

                match parse_review_result(&response, &self.ctx.log) {
                    Ok(result) => break result,
                    Err(parse_err) => {
                        requeries += 1;
                        if requeries > self.config.max_requeries {
                            return Err(crate::agents::error::AgentError::ParseExhausted {
                                attempts: self.config.max_requeries,
                            }
                            .into());
                        }
                        self.ctx.warn(&format!(
                            "parse attempt {}/{} failed: {}",
                            requeries, self.config.max_requeries, parse_err
                        ));
                        let truncated: String = response.chars().take(200).collect();
                        messages.push(ChatMessage::assistant(&truncated));
                        messages.push(ChatMessage::user(&format!(
                            "Your response (starting with: {:?}...) could not be parsed \
                             as valid JSON. Error: {}\n\n\
                             Respond with ONLY a valid JSON object matching the schema \
                             in the system prompt. No markdown, no prose.",
                            &truncated[..truncated.len().min(100)],
                            parse_err,
                        )));
                    }
                }
            }
        };

        self.ctx.info(&format!(
            "verdict: {} ({} issues) — {}",
            review.verdict,
            review.issues.len(),
            review.summary
        ));

        let learning_content = format!(
            "Review of bundle {}: verdict={}, summary={}",
            self.bundle_id, review.verdict, review.summary
        );
        let learning_resp = self.ctx.bridge.request(
            "learning.create",
            serde_json::json!({
                "content": learning_content,
                "scope": "work",
                "source_id": work_title,
            }),
        );
        if learning_resp.is_error() {
            self.ctx
                .warn(&format!("failed to create learning: {:?}", learning_resp.error));
        }

        // Create feedback Learning tagged with review:feedback (advisory feedback for all verdicts)
        let feedback_tag = format!("review:{}", review.verdict);
        let learn_resp = self.ctx.bridge.request(
            "learning.create",
            serde_json::json!({
                "content": format!("Review feedback ({}): {}", review.verdict, review.summary),
                "scope": "work",
                "source_id": work_title,
                "resource_tags": [feedback_tag],
            }),
        );
        if learn_resp.is_error() {
            self.ctx.warn(&format!(
                "failed to create review feedback learning for bundle {}",
                self.bundle_id
            ));
        }

        match review.verdict {
            ReviewVerdict::Approve => {
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "target_status": "Reviewed",
                        "role": "reviewer",
                        "verification": format!("Reviewer approved: {}", review.summary),
                    }),
                );
                if resp.is_error() {
                    // Advisory review race: bundle may have already advanced past Triaged
                    // (e.g., Coordinator accepted directly). This is expected - log and continue
                    // since the feedback Learning was already created above.
                    self.ctx.warn(&format!(
                        "bundle {} already advanced past Triaged (advisory review race): {:?}",
                        self.bundle_id, resp.error
                    ));
                } else {
                    self.ctx.info(&format!("approved bundle {}", self.bundle_id));
                }
            }
            ReviewVerdict::RequestChanges => {
                // Store rejection reason before transitioning so the coordinator
                // and next implementer know why the bundle was rejected.
                let upd_resp = self.ctx.bridge.request(
                    "bundle.update",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "verification": format!("Rejected: {}", review.summary),
                    }),
                );
                if upd_resp.is_error() {
                    self.ctx
                        .warn(&format!("failed to update verification on bundle {}", self.bundle_id));
                }
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "target_status": "Rejected",
                        "role": "reviewer",
                    }),
                );
                if resp.is_error() {
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Err(eyre::eyre!("failed to reject bundle {}: {}", self.bundle_id, msg));
                }
                self.ctx.info(&format!(
                    "requested changes on bundle {} (->Rejected + Learning)",
                    self.bundle_id
                ));
            }
            ReviewVerdict::Reject => {
                // Store rejection reason before transitioning so the coordinator
                // and next implementer know why the bundle was rejected.
                let upd_resp = self.ctx.bridge.request(
                    "bundle.update",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "verification": format!("Rejected: {}", review.summary),
                    }),
                );
                if upd_resp.is_error() {
                    self.ctx
                        .warn(&format!("failed to update verification on bundle {}", self.bundle_id));
                }
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "target_status": "Rejected",
                        "role": "reviewer",
                    }),
                );
                if resp.is_error() {
                    let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                    return Err(eyre::eyre!("failed to reject bundle {}: {}", self.bundle_id, msg));
                }
                self.ctx.info(&format!("rejected bundle {}", self.bundle_id));
            }
        }

        let _ = self.ctx.event_tx.send(DaemonEvent::agent_iteration_completed(
            &self.ctx.session.id,
            1,
            &format!("{}: {}", review.verdict, review.summary),
        ));

        Ok(())
    }

    fn agent_type(&self) -> AgentKind {
        AgentKind::Reviewer
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use eyre::eyre;

    use crate::agents::agent_logger::AgentLogger;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentContext, AgentKind, AgentSession};
    use crate::config::{AgentRoleConfig, Config, ProjectConfig};
    use crate::daemon::context::Stores;
    use crate::domain::bundle::{Bundle, BundleStatus};
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::work::Work;
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use crate::worktree::manager::WorktreeManager;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};
    use taskstore::Store;
    use tokio::sync::broadcast;

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

    impl LlmClient for MockReviewLlm {
        fn call<'a>(
            &'a self,
            _system_prompt: &'a str,
            _user_message: &'a str,
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            async move { Ok(self.response.clone()) }
        }
    }

    struct FailingReviewLlm;

    impl LlmClient for FailingReviewLlm {
        fn call<'a>(
            &'a self,
            _system_prompt: &'a str,
            _user_message: &'a str,
        ) -> impl Future<Output = Result<String>> + Send + 'a {
            async move { Err(eyre!("LLM call failed")) }
        }
    }

    /// Mock LLM that returns a sequence of responses, one per call.
    struct SequentialReviewLlm {
        responses: StdMutex<Vec<String>>,
    }

    impl SequentialReviewLlm {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: StdMutex::new(responses.into_iter().map(String::from).collect()),
            }
        }
    }

    impl LlmClient for SequentialReviewLlm {
        fn call<'a>(
            &'a self,
            _system_prompt: &'a str,
            _user_message: &'a str,
        ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
            async move {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(eyre!("no more responses"));
                }
                Ok(responses.remove(0))
            }
        }

        fn call_with_history<'a>(
            &'a self,
            _system_prompt: &'a str,
            _messages: &'a [ChatMessage],
        ) -> impl std::future::Future<Output = Result<String>> + Send + 'a {
            async move {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Err(eyre!("no more responses"));
                }
                Ok(responses.remove(0))
            }
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

        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "criteria".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let spec = Spec::new(plan_id, "Test Spec".into(), "A test spec".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let phase = Phase::new(spec_id, "Test Phase".into(), "A test phase".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let wi = Work::new(phase_id.clone(), "Test Work".into(), "Implement the feature".into());
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut bundle = Bundle::new(
            wi_id,
            Some("tick-001".into()),
            "feature/test".into(),
            vec!["Added test module with basic functionality".into()],
        );
        bundle.force_status(BundleStatus::Triaged);
        bundle.touched_paths = vec!["src/test.rs".into(), "src/main.rs".into()];
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let learning = Learning::new(
            phase_id,
            LearningScope::Phase,
            "Always check for edge cases in parsing".into(),
        );
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        (Arc::new(stores), bundle_id)
    }

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        let file_path = dir.join("test-reviewer.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentKind::Reviewer, "test-session", file, file_path)
    }

    fn test_reviewer<L: LlmClient>(dir: &Path, stores: Arc<Stores>, bundle_id: &str, llm: L) -> ReviewerAgent<L> {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let mut session = AgentSession::new(AgentKind::Reviewer, "test".into());
        session.bundle_id = Some(bundle_id.to_string());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let config = AgentRoleConfig::default_reviewer();
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        ReviewerAgent::new(ctx, llm, config).unwrap()
    }

    // --- ReviewVerdict tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_review_verdict_display() {
        assert_eq!(ReviewVerdict::Approve.to_string(), "approve");
        assert_eq!(ReviewVerdict::RequestChanges.to_string(), "request_changes");
        assert_eq!(ReviewVerdict::Reject.to_string(), "reject");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_review_verdict_serde_roundtrip() {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_review_result_serde_approve() {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_review_result_serde_with_issues() {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_review_result_serde_reject() {
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

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_review_result_direct_json() {
        let dir = TestDir::new("loopr-rev-parse1");
        let agent_log = test_agent_logger(&dir);
        let json = r#"{"verdict": "approve", "issues": [], "summary": "All good"}"#;
        let result = parse_review_result(json, &agent_log).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_review_result_wrapped_in_code_block() {
        let dir = TestDir::new("loopr-rev-parse2");
        let agent_log = test_agent_logger(&dir);
        let response =
            "Here is my review:\n```json\n{\"verdict\": \"approve\", \"issues\": [], \"summary\": \"LGTM\"}\n```";
        let result = parse_review_result(response, &agent_log).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
        assert_eq!(result.summary, "LGTM");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_review_result_invalid() {
        let dir = TestDir::new("loopr-rev-parse3");
        let agent_log = test_agent_logger(&dir);
        let bad = "This is not JSON at all";
        assert!(parse_review_result(bad, &agent_log).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_parse_review_result_wrong_schema() {
        let dir = TestDir::new("loopr-rev-parse4");
        let agent_log = test_agent_logger(&dir);
        let json = r#"{"wrong": "schema"}"#;
        assert!(parse_review_result(json, &agent_log).is_err());
    }

    // --- context builder integration tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_context_builder_for_reviewer() {
        let dir = TestDir::new("loopr-rev-ctx");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let ctx = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap()
            .with_footer("Review this Bundle.".into());
        assert_eq!(ctx.work_title(), Some("Test Work"));

        let assembled = ctx.build(&crate::prompts::store().reviewer).unwrap();
        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test Work"));
        assert!(assembled.user_message.contains("Bundle Under Review"));
        assert!(assembled.system_prompt.contains("Reviewer agent"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_context_builder_missing_bundle() {
        let dir = TestDir::new("loopr-rev-miss");
        let (stores, _) = setup_stores_with_bundle(&dir);

        let result = ContextBuilder::new(&stores, Role::Reviewer).load_bundle_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("bundle not found"));
    }

    // --- ReviewerAgent tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_reviewer_approve() {
        let dir = TestDir::new("loopr-rev-approve");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = MockReviewLlm::new(r#"{"verdict": "approve", "issues": [], "summary": "LGTM"}"#);
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());
        assert_eq!(agent.ctx.session.iteration, 1);

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status(), BundleStatus::Reviewed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_reviewer_reject() {
        let dir = TestDir::new("loopr-rev-reject");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = MockReviewLlm::new(
            r#"{"verdict": "reject", "issues": [{"severity": "error", "file": "src/main.rs", "message": "Wrong approach"}], "summary": "Fundamentally wrong"}"#,
        );
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status(), BundleStatus::Rejected);
        assert!(
            bundle.verification.contains("Rejected: Fundamentally wrong"),
            "verification should contain rejection reason: {}",
            bundle.verification
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_reviewer_request_changes() {
        let dir = TestDir::new("loopr-rev-changes");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = MockReviewLlm::new(
            r#"{"verdict": "request_changes", "issues": [{"severity": "warning", "file": "src/lib.rs", "message": "Add more tests"}], "summary": "Needs work"}"#,
        );
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

        // #21: request_changes now transitions to Rejected (not Reviewed)
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status(), BundleStatus::Rejected);
        assert!(
            bundle.verification.contains("Rejected: Needs work"),
            "verification should contain rejection reason: {}",
            bundle.verification
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reviewer_missing_bundle_id() {
        let dir = TestDir::new("loopr-rev-noid");
        let (stores, _) = setup_stores_with_bundle(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);
        let session = AgentSession::new(AgentKind::Reviewer, "test".into());
        // No bundle_id set
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        let llm = MockReviewLlm::new("{}");
        let result = ReviewerAgent::new(ctx, llm, AgentRoleConfig::default_reviewer());
        let err = result.err().expect("expected error for missing bundle_id");
        assert!(err.to_string().contains("missing bundle_id"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_reviewer_llm_failure() {
        let dir = TestDir::new("loopr-rev-fail");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = FailingReviewLlm;
        let mut agent = test_reviewer(&dir, stores, &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_run_reviewer_bad_response() {
        let dir = TestDir::new("loopr-rev-bad");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        // With retry, bad response must exhaust all retries before failing
        let llm = MockReviewLlm::new("Not valid JSON");
        let mut agent = test_reviewer(&dir, stores, &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.err().unwrap());
        assert!(
            err_msg.contains("exhausted parse retries"),
            "expected parse retry exhaustion, got: {}",
            err_msg
        );
    }

    // --- Parse retry tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reviewer_parse_retry_succeeds_on_second_attempt() {
        let dir = TestDir::new("loopr-rev-retry-ok");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = SequentialReviewLlm::new(vec![
            "Not valid JSON",
            r#"{"verdict": "approve", "issues": [], "summary": "LGTM after retry"}"#,
        ]);
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok(), "retry should succeed: {:?}", result.err());

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status(), BundleStatus::Reviewed);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reviewer_parse_retry_exhaustion() {
        let dir = TestDir::new("loopr-rev-retry-fail");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        // 4 bad responses: 1 initial + 3 retries (max_requeries=3) -> exhaustion
        let llm = SequentialReviewLlm::new(vec!["bad 1", "bad 2", "bad 3", "bad 4"]);
        let mut agent = test_reviewer(&dir, stores, &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.err().unwrap());
        assert!(
            err_msg.contains("exhausted parse retries"),
            "expected exhaustion error, got: {}",
            err_msg
        );
    }

    // --- Advisory review race condition tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reviewer_handles_already_accepted_bundle() {
        // Simulate advisory review race: bundle is already Accepted when Reviewer runs.
        // The Reviewer should create its feedback Learning and complete normally (not error).
        let dir = TestDir::new("loopr-rev-race");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        // Advance bundle to Accepted (simulating Coordinator direct acceptance)
        {
            let mut bundles = stores.bundles.write().unwrap();
            let bundle = bundles.get_mut(&bundle_id).unwrap();
            bundle.force_status(BundleStatus::Accepted);
        }

        let llm = MockReviewLlm::new(r#"{"verdict": "approve", "issues": [], "summary": "LGTM"}"#);
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        // Should succeed (not error) even though bundle.transition fails
        assert!(
            result.is_ok(),
            "Reviewer should handle advisory race gracefully, got: {:?}",
            result.err()
        );

        // Verify feedback Learning was still created
        let learnings = stores.learnings.read().unwrap();
        let has_feedback = learnings
            .values()
            .any(|l| l.content.contains("Review feedback") || l.content.contains("Review of bundle"));
        assert!(
            has_feedback,
            "Feedback Learning should be created even in race condition"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reviewer_creates_feedback_learning_on_approve() {
        // Verify the reviewer creates a review:feedback Learning on all verdicts (not just reject).
        let dir = TestDir::new("loopr-rev-feedback");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = MockReviewLlm::new(r#"{"verdict": "approve", "issues": [], "summary": "Clean code"}"#);
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

        // Check for the feedback Learning (created before the transition)
        let learnings = stores.learnings.read().unwrap();
        let feedback_count = learnings
            .values()
            .filter(|l| l.content.contains("Review feedback (approve)"))
            .count();
        assert!(feedback_count >= 1, "Should create feedback Learning on approve");
    }

    // --- system prompt tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_system_prompt_contains_key_instructions() {
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().reviewer;
        assert!(prompt.contains("Reviewer agent"));
        assert!(prompt.contains("Acceptance Criteria"));
        assert!(prompt.contains("Safety & Security"));
        assert!(prompt.contains("Quality & Style"));
        assert!(prompt.contains("verdict"));
        assert!(prompt.contains("approve"));
        assert!(prompt.contains("request_changes"));
        assert!(prompt.contains("reject"));
    }
}
