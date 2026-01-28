//! Layer 3: LLM-as-judge validation.
//!
//! For subjective criteria that can't be tested programmatically, we use
//! a separate LLM call to evaluate the output. The judge gives a binary
//! PASS/FAIL decision with actionable feedback.
//!
//! ## Use Cases
//!
//! - Documentation quality: "Is this clear and complete?"
//! - API design: "Is this ergonomic?"
//! - Error messages: "Are these helpful?"
//! - Code organization: "Is this well-structured?"
//!
//! ## Key Principles
//!
//! 1. **Binary decisions only** - No scores, no "mostly good". PASS or FAIL.
//! 2. **Actionable feedback** - On FAIL, explain what needs to change.
//! 3. **Specific criteria** - Don't ask "is this good?" - define what "good" means.

use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

use super::feedback::{FailureCategory, FailureDetail};
use crate::llm::{CompletionRequest, LlmClient, LlmError, Message, MessageContent, Role};

/// Errors from LLM judge operations.
#[derive(Debug, Error)]
pub enum JudgeError {
    #[error("LLM error: {0}")]
    Llm(#[from] LlmError),

    #[error("Judge response parsing failed: {0}")]
    ParseFailed(String),

    #[error("Judge timed out after {0:?}")]
    Timeout(Duration),
}

/// Criteria for the judge to evaluate.
#[derive(Debug, Clone)]
pub struct JudgeCriteria {
    /// Name of what's being evaluated (e.g., "Plan", "Documentation").
    pub subject: String,

    /// Specific questions the judge should answer.
    pub questions: Vec<String>,

    /// Optional examples of what passes/fails.
    pub examples: Option<JudgeExamples>,
}

impl JudgeCriteria {
    /// Create new criteria with the given subject.
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            questions: Vec::new(),
            examples: None,
        }
    }

    /// Add a question for the judge to evaluate.
    pub fn with_question(mut self, question: impl Into<String>) -> Self {
        self.questions.push(question.into());
        self
    }

    /// Add multiple questions.
    pub fn with_questions(mut self, questions: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.questions.extend(questions.into_iter().map(|q| q.into()));
        self
    }

    /// Add examples of pass/fail cases.
    pub fn with_examples(mut self, examples: JudgeExamples) -> Self {
        self.examples = Some(examples);
        self
    }

    /// Build the judge prompt.
    pub fn build_prompt(&self, content: &str) -> String {
        let mut prompt = String::new();

        prompt.push_str(&format!("You are a code reviewer evaluating a {}.\n\n", self.subject));

        prompt.push_str("## Evaluation Criteria\n\n");
        for (i, question) in self.questions.iter().enumerate() {
            prompt.push_str(&format!("{}. {}\n", i + 1, question));
        }
        prompt.push('\n');

        if let Some(examples) = &self.examples {
            prompt.push_str("## Examples\n\n");
            if let Some(pass) = &examples.pass_example {
                prompt.push_str(&format!("**PASS example:** {}\n\n", pass));
            }
            if let Some(fail) = &examples.fail_example {
                prompt.push_str(&format!("**FAIL example:** {}\n\n", fail));
            }
        }

        prompt.push_str("## Content to Evaluate\n\n");
        prompt.push_str("```\n");
        prompt.push_str(content);
        prompt.push_str("\n```\n\n");

        prompt.push_str("## Your Response\n\n");
        prompt.push_str("Respond with EXACTLY one of:\n");
        prompt.push_str("- `PASS` if all criteria are met\n");
        prompt.push_str("- `FAIL: <reason>` if any criteria fail (be specific about what needs to change)\n\n");
        prompt.push_str("Your response (PASS or FAIL: <reason>):");

        prompt
    }
}

/// Examples for the judge.
#[derive(Debug, Clone)]
pub struct JudgeExamples {
    /// Example of content that would pass.
    pub pass_example: Option<String>,
    /// Example of content that would fail.
    pub fail_example: Option<String>,
}

impl JudgeExamples {
    /// Create new examples.
    pub fn new() -> Self {
        Self {
            pass_example: None,
            fail_example: None,
        }
    }

    /// Set the pass example.
    pub fn with_pass(mut self, example: impl Into<String>) -> Self {
        self.pass_example = Some(example.into());
        self
    }

    /// Set the fail example.
    pub fn with_fail(mut self, example: impl Into<String>) -> Self {
        self.fail_example = Some(example.into());
        self
    }
}

impl Default for JudgeExamples {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of the judge's evaluation.
#[derive(Debug, Clone)]
pub struct JudgeResult {
    /// Whether the judge approved.
    pub passed: bool,

    /// The judge's reasoning (especially important on failure).
    pub reasoning: String,

    /// Parsed failure details if failed.
    pub failures: Vec<FailureDetail>,

    /// How long the judgment took.
    pub duration: Duration,
}

impl JudgeResult {
    /// Create a passing result.
    pub fn pass(reasoning: impl Into<String>, duration: Duration) -> Self {
        Self {
            passed: true,
            reasoning: reasoning.into(),
            failures: Vec::new(),
            duration,
        }
    }

    /// Create a failing result.
    pub fn fail(reasoning: impl Into<String>, duration: Duration) -> Self {
        let reasoning = reasoning.into();
        Self {
            passed: false,
            failures: vec![FailureDetail::new(FailureCategory::Judge, &reasoning)],
            reasoning,
            duration,
        }
    }
}

/// LLM-as-judge validator.
pub struct LlmJudge {
    /// LLM client to use for evaluation.
    client: Arc<dyn LlmClient>,

    /// System prompt for the judge.
    system_prompt: String,

    /// Timeout for judge calls.
    timeout: Duration,

    /// Max tokens for judge response.
    max_tokens: u32,
}

impl LlmJudge {
    /// Create a new judge with the given LLM client.
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            system_prompt: "You are a precise code reviewer. Give binary PASS/FAIL judgments with specific feedback."
                .to_string(),
            timeout: Duration::from_secs(60),
            max_tokens: 500,
        }
    }

    /// Set the system prompt.
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Judge the given content against the criteria.
    pub async fn judge(&self, criteria: &JudgeCriteria, content: &str) -> Result<JudgeResult, JudgeError> {
        let start = Instant::now();

        let user_prompt = criteria.build_prompt(content);

        let request = CompletionRequest {
            system_prompt: self.system_prompt.clone(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(user_prompt),
            }],
            tools: Vec::new(),
            max_tokens: self.max_tokens,
        };

        let response = tokio::time::timeout(self.timeout, self.client.complete(request))
            .await
            .map_err(|_| JudgeError::Timeout(self.timeout))??;

        let duration = start.elapsed();

        let response_text = response
            .content
            .ok_or_else(|| JudgeError::ParseFailed("No content in response".to_string()))?;

        parse_judge_response(&response_text, duration)
    }

    /// Create standard criteria for plan validation.
    pub fn plan_criteria() -> JudgeCriteria {
        JudgeCriteria::new("Plan").with_questions([
            "Does the plan address all stated requirements?",
            "Can it be decomposed into 1-2 concrete specs?",
            "Are there any obvious technical blockers or impossibilities?",
            "Is the scope appropriate (not too broad, not too narrow)?",
        ])
    }

    /// Create standard criteria for spec validation.
    pub fn spec_criteria() -> JudgeCriteria {
        JudgeCriteria::new("Specification").with_questions([
            "Are the requirements concrete and implementable (not vague)?",
            "Are the acceptance criteria testable?",
            "Is the phase breakdown reasonable (3-7 phases)?",
            "Are dependencies between phases clear?",
        ])
    }

    /// Create standard criteria for phase validation.
    pub fn phase_criteria() -> JudgeCriteria {
        JudgeCriteria::new("Phase Implementation").with_questions([
            "Does this implementation satisfy the phase requirements?",
            "Are there any obvious bugs or logic errors?",
            "Is the code well-organized and maintainable?",
            "Are edge cases handled appropriately?",
        ])
    }

    /// Create standard criteria for documentation validation.
    pub fn documentation_criteria() -> JudgeCriteria {
        JudgeCriteria::new("Documentation").with_questions([
            "Is the documentation clear and easy to understand?",
            "Are all public APIs documented?",
            "Are usage examples provided where appropriate?",
            "Is the documentation complete (no TODOs or missing sections)?",
        ])
    }
}

/// Parse the judge's response into a JudgeResult.
fn parse_judge_response(response: &str, duration: Duration) -> Result<JudgeResult, JudgeError> {
    let response = response.trim();

    // Check for explicit PASS
    if response.starts_with("PASS") || response.eq_ignore_ascii_case("pass") {
        return Ok(JudgeResult::pass(response, duration));
    }

    // Check for FAIL: <reason>
    if response.starts_with("FAIL:") || response.starts_with("FAIL ") {
        let reason = response
            .strip_prefix("FAIL:")
            .or_else(|| response.strip_prefix("FAIL "))
            .unwrap_or(response)
            .trim();
        return Ok(JudgeResult::fail(reason, duration));
    }

    // Try to infer from content
    let lower = response.to_lowercase();
    if lower.contains("fail") || lower.contains("reject") || lower.contains("does not meet") {
        // Treat as failure
        return Ok(JudgeResult::fail(response, duration));
    }

    if lower.contains("pass") || lower.contains("approve") || lower.contains("meets all") {
        // Treat as pass
        return Ok(JudgeResult::pass(response, duration));
    }

    // Ambiguous response - treat as failure with the full response as feedback
    Err(JudgeError::ParseFailed(format!(
        "Could not parse judge response as PASS or FAIL: {}",
        truncate_for_error(response, 100)
    )))
}

/// Truncate text for error messages.
fn truncate_for_error(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionResponse, StopReason, StreamChunk, TokenUsage};
    use async_trait::async_trait;
    use tokio::sync::mpsc;

    // Mock LLM client for testing
    struct MockLlmClient {
        response: String,
    }

    impl MockLlmClient {
        fn new(response: impl Into<String>) -> Self {
            Self {
                response: response.into(),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlmClient {
        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: Some(self.response.clone()),
                tool_calls: Vec::new(),
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }

        async fn stream(
            &self,
            _request: CompletionRequest,
            _chunk_tx: mpsc::Sender<StreamChunk>,
        ) -> Result<CompletionResponse, LlmError> {
            self.complete(_request).await
        }
    }

    #[test]
    fn test_judge_criteria_new() {
        let criteria = JudgeCriteria::new("Plan")
            .with_question("Is the plan complete?")
            .with_question("Is it feasible?");

        assert_eq!(criteria.subject, "Plan");
        assert_eq!(criteria.questions.len(), 2);
    }

    #[test]
    fn test_judge_criteria_build_prompt() {
        let criteria = JudgeCriteria::new("Plan").with_question("Is it complete?");

        let prompt = criteria.build_prompt("# My Plan\n\nDo stuff.");

        assert!(prompt.contains("Plan"));
        assert!(prompt.contains("Is it complete?"));
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("PASS"));
        assert!(prompt.contains("FAIL"));
    }

    #[test]
    fn test_judge_examples() {
        let examples = JudgeExamples::new()
            .with_pass("A complete plan with clear goals")
            .with_fail("A vague plan with no specifics");

        assert!(examples.pass_example.is_some());
        assert!(examples.fail_example.is_some());
    }

    #[test]
    fn test_judge_result_pass() {
        let result = JudgeResult::pass("All criteria met", Duration::from_secs(1));
        assert!(result.passed);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn test_judge_result_fail() {
        let result = JudgeResult::fail("Missing acceptance criteria", Duration::from_secs(1));
        assert!(!result.passed);
        assert_eq!(result.failures.len(), 1);
        assert_eq!(result.failures[0].category, FailureCategory::Judge);
    }

    #[test]
    fn test_parse_judge_response_pass() {
        let result = parse_judge_response("PASS", Duration::from_secs(1)).unwrap();
        assert!(result.passed);

        let result = parse_judge_response("pass", Duration::from_secs(1)).unwrap();
        assert!(result.passed);
    }

    #[test]
    fn test_parse_judge_response_fail() {
        let result = parse_judge_response("FAIL: Missing requirements section", Duration::from_secs(1)).unwrap();
        assert!(!result.passed);
        assert!(result.reasoning.contains("Missing requirements"));

        let result = parse_judge_response("FAIL Missing something", Duration::from_secs(1)).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn test_parse_judge_response_inferred() {
        // Inferred pass
        let result =
            parse_judge_response("The plan meets all criteria and is approved.", Duration::from_secs(1)).unwrap();
        assert!(result.passed);

        // Inferred fail
        let result = parse_judge_response("The plan does not meet the requirements.", Duration::from_secs(1)).unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn test_parse_judge_response_ambiguous() {
        let result = parse_judge_response("I'm not sure about this plan.", Duration::from_secs(1));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_llm_judge_pass() {
        let client = Arc::new(MockLlmClient::new("PASS"));
        let judge = LlmJudge::new(client);

        let criteria = JudgeCriteria::new("Plan").with_question("Is it complete?");
        let result = judge.judge(&criteria, "# Plan\n\nComplete plan.").await.unwrap();

        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_llm_judge_fail() {
        let client = Arc::new(MockLlmClient::new("FAIL: Missing acceptance criteria"));
        let judge = LlmJudge::new(client);

        let criteria = JudgeCriteria::new("Spec").with_question("Is it complete?");
        let result = judge.judge(&criteria, "# Spec\n\nIncomplete.").await.unwrap();

        assert!(!result.passed);
        assert!(result.reasoning.contains("acceptance criteria"));
    }

    #[test]
    fn test_standard_criteria() {
        let plan = LlmJudge::plan_criteria();
        assert!(!plan.questions.is_empty());

        let spec = LlmJudge::spec_criteria();
        assert!(!spec.questions.is_empty());

        let phase = LlmJudge::phase_criteria();
        assert!(!phase.questions.is_empty());

        let docs = LlmJudge::documentation_criteria();
        assert!(!docs.questions.is_empty());
    }

    #[test]
    fn test_truncate_for_error() {
        assert_eq!(truncate_for_error("short", 10), "short");
        assert_eq!(truncate_for_error("a very long string", 10), "a very lon...");
    }
}
