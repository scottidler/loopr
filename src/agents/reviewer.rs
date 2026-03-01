use async_trait::async_trait;
use eyre::{Result, eyre};
use serde::{Deserialize, Serialize};

use crate::agents::context::ContextBuilder;
use crate::agents::implementer::LlmClient;
use crate::agents::{Agent, AgentContext, AgentType};
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
pub struct ReviewerAgent {
    pub ctx: AgentContext,
    llm: Box<dyn LlmClient>,
    config: AgentRoleConfig,
    bundle_id: String,
}

impl ReviewerAgent {
    pub fn new(ctx: AgentContext, llm: Box<dyn LlmClient>, config: AgentRoleConfig) -> Result<Self> {
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

#[async_trait]
impl Agent for ReviewerAgent {
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
        let assembled = ctx_builder.build(&crate::prompts::store().reviewer);

        let response = self.llm.call(&assembled.system_prompt, &assembled.user_message).await?;
        let review = parse_review_result(&response, &self.ctx.log)?;

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
                    self.ctx
                        .warn(&format!("failed to transition bundle to Reviewed: {:?}", resp.error));
                    return Err(eyre!("failed to transition bundle: {:?}", resp.error));
                }
                self.ctx.info(&format!("approved bundle {}", self.bundle_id));
            }
            ReviewVerdict::RequestChanges => {
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "target_status": "Rejected",
                        "role": "reviewer",
                    }),
                );
                if resp.is_error() {
                    self.ctx
                        .warn(&format!("failed to reject bundle (request_changes): {:?}", resp.error));
                    return Err(eyre!("failed to reject bundle: {:?}", resp.error));
                }
                let _ = self.ctx.bridge.request(
                    "learning.create",
                    serde_json::json!({
                        "content": format!("Review requested changes: {}", review.summary),
                        "scope": "work",
                        "source_id": work_title,
                    }),
                );
                self.ctx.info(&format!(
                    "requested changes on bundle {} (→Rejected + Learning)",
                    self.bundle_id
                ));
            }
            ReviewVerdict::Reject => {
                let resp = self.ctx.bridge.request(
                    "bundle.transition",
                    serde_json::json!({
                        "id": self.bundle_id,
                        "target_status": "Rejected",
                        "role": "reviewer",
                    }),
                );
                if resp.is_error() {
                    self.ctx.warn(&format!("failed to reject bundle: {:?}", resp.error));
                    return Err(eyre!("failed to reject bundle: {:?}", resp.error));
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

    fn agent_type(&self) -> AgentType {
        AgentType::Reviewer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::agent_logger::AgentLogger;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentContext, AgentSession, AgentType};
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
    use async_trait::async_trait;
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
        bundle.status = BundleStatus::Triaged;
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
        AgentLogger::_new_for_test(AgentType::Reviewer, "test-session", file, file_path)
    }

    fn test_reviewer(dir: &Path, stores: Arc<Stores>, bundle_id: &str, llm: Box<dyn LlmClient>) -> ReviewerAgent {
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let mut session = AgentSession::new(AgentType::Reviewer, "test".into());
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
            log: agent_log,
        };
        ReviewerAgent::new(ctx, llm, config).unwrap()
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
        let dir = TestDir::new("loopr-rev-parse1");
        let agent_log = test_agent_logger(&dir);
        let json = r#"{"verdict": "approve", "issues": [], "summary": "All good"}"#;
        let result = parse_review_result(json, &agent_log).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
    }

    #[test]
    fn test_parse_review_result_wrapped_in_code_block() {
        let dir = TestDir::new("loopr-rev-parse2");
        let agent_log = test_agent_logger(&dir);
        let response =
            "Here is my review:\n```json\n{\"verdict\": \"approve\", \"issues\": [], \"summary\": \"LGTM\"}\n```";
        let result = parse_review_result(response, &agent_log).unwrap();
        assert_eq!(result.verdict, ReviewVerdict::Approve);
        assert_eq!(result.summary, "LGTM");
    }

    #[test]
    fn test_parse_review_result_invalid() {
        let dir = TestDir::new("loopr-rev-parse3");
        let agent_log = test_agent_logger(&dir);
        let bad = "This is not JSON at all";
        assert!(parse_review_result(bad, &agent_log).is_err());
    }

    #[test]
    fn test_parse_review_result_wrong_schema() {
        let dir = TestDir::new("loopr-rev-parse4");
        let agent_log = test_agent_logger(&dir);
        let json = r#"{"wrong": "schema"}"#;
        assert!(parse_review_result(json, &agent_log).is_err());
    }

    // --- context builder integration tests ---

    #[test]
    fn test_context_builder_for_reviewer() {
        let dir = TestDir::new("loopr-rev-ctx");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let ctx = ContextBuilder::new(&stores, Role::Reviewer)
            .load_bundle_hierarchy(&bundle_id)
            .unwrap()
            .with_footer("Review this Bundle.".into());
        assert_eq!(ctx.work_title(), Some("Test Work"));

        let assembled = ctx.build(&crate::prompts::store().reviewer);
        assert!(assembled.user_message.contains("Test Plan"));
        assert!(assembled.user_message.contains("Test Spec"));
        assert!(assembled.user_message.contains("Test Phase"));
        assert!(assembled.user_message.contains("Test Work"));
        assert!(assembled.user_message.contains("Bundle Under Review"));
        assert!(assembled.system_prompt.contains("Reviewer agent"));
    }

    #[test]
    fn test_context_builder_missing_bundle() {
        let dir = TestDir::new("loopr-rev-miss");
        let (stores, _) = setup_stores_with_bundle(&dir);

        let result = ContextBuilder::new(&stores, Role::Reviewer).load_bundle_hierarchy("nonexistent");
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("bundle not found"));
    }

    // --- ReviewerAgent tests ---

    #[tokio::test]
    async fn test_run_reviewer_approve() {
        let dir = TestDir::new("loopr-rev-approve");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = Box::new(MockReviewLlm::new(
            r#"{"verdict": "approve", "issues": [], "summary": "LGTM"}"#,
        ));
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());
        assert_eq!(agent.ctx.session.iteration, 1);

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Reviewed);
    }

    #[tokio::test]
    async fn test_run_reviewer_reject() {
        let dir = TestDir::new("loopr-rev-reject");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = Box::new(MockReviewLlm::new(
            r#"{"verdict": "reject", "issues": [{"severity": "error", "file": "src/main.rs", "message": "Wrong approach"}], "summary": "Fundamentally wrong"}"#,
        ));
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Rejected);
    }

    #[tokio::test]
    async fn test_run_reviewer_request_changes() {
        let dir = TestDir::new("loopr-rev-changes");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = Box::new(MockReviewLlm::new(
            r#"{"verdict": "request_changes", "issues": [{"severity": "warning", "file": "src/lib.rs", "message": "Add more tests"}], "summary": "Needs work"}"#,
        ));
        let mut agent = test_reviewer(&dir, stores.clone(), &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

        // #21: request_changes now transitions to Rejected (not Reviewed)
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(&bundle_id).unwrap();
        assert_eq!(bundle.status, BundleStatus::Rejected);
    }

    #[test]
    fn test_reviewer_missing_bundle_id() {
        let dir = TestDir::new("loopr-rev-noid");
        let (stores, _) = setup_stores_with_bundle(&dir);
        let (event_tx, _) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);
        let session = AgentSession::new(AgentType::Reviewer, "test".into());
        // No bundle_id set
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            log: agent_log,
        };
        let llm = Box::new(MockReviewLlm::new("{}"));
        let result = ReviewerAgent::new(ctx, llm, AgentRoleConfig::default_reviewer());
        let err = result.err().expect("expected error for missing bundle_id");
        assert!(err.to_string().contains("missing bundle_id"));
    }

    #[tokio::test]
    async fn test_run_reviewer_llm_failure() {
        let dir = TestDir::new("loopr-rev-fail");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm: Box<dyn LlmClient> = Box::new(FailingReviewLlm);
        let mut agent = test_reviewer(&dir, stores, &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_run_reviewer_bad_response() {
        let dir = TestDir::new("loopr-rev-bad");
        let (stores, bundle_id) = setup_stores_with_bundle(&dir);

        let llm = Box::new(MockReviewLlm::new("Not valid JSON"));
        let mut agent = test_reviewer(&dir, stores, &bundle_id, llm);
        let result = agent.run().await;
        assert!(result.is_err());
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_instructions() {
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().reviewer;
        assert!(prompt.contains("Reviewer agent"));
        assert!(prompt.contains("Correctness"));
        assert!(prompt.contains("Quality"));
        assert!(prompt.contains("Tests"));
        assert!(prompt.contains("verdict"));
        assert!(prompt.contains("approve"));
        assert!(prompt.contains("request_changes"));
        assert!(prompt.contains("reject"));
    }
}
