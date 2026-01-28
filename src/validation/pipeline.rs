//! Composite validation pipeline.
//!
//! Combines all three validation layers into a single pipeline:
//! 1. Format/structure checks (fast, catches obvious issues)
//! 2. Test execution (downstream gates)
//! 3. LLM-as-judge (subjective criteria)
//!
//! Gates run in sequence, failing fast on the first failure.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::feedback::{FailureDetail, IterationFeedback};
use super::format::{FormatValidationResult, FormatValidator};
use super::llm_judge::{JudgeCriteria, JudgeError, LlmJudge};
use super::test_runner::{TestRunner, TestRunnerConfig, TestRunnerError};
use crate::llm::LlmClient;
use crate::store::LoopType;

/// The outcome of running the validation pipeline.
#[derive(Debug, Clone)]
pub enum ValidationOutcome {
    /// All validation gates passed.
    Pass,
    /// At least one validation gate failed.
    Fail(IterationFeedback),
}

impl ValidationOutcome {
    /// Check if validation passed.
    pub fn passed(&self) -> bool {
        matches!(self, ValidationOutcome::Pass)
    }

    /// Get the feedback if failed.
    pub fn feedback(&self) -> Option<&IterationFeedback> {
        match self {
            ValidationOutcome::Pass => None,
            ValidationOutcome::Fail(fb) => Some(fb),
        }
    }
}

/// Result of a single gate's validation.
#[derive(Debug, Clone)]
pub enum GateResult {
    /// Gate passed.
    Pass,
    /// Gate failed with details.
    Fail(Vec<FailureDetail>),
    /// Gate was skipped (e.g., no LLM client for judge).
    Skipped(String),
}

impl GateResult {
    /// Check if the gate passed.
    pub fn passed(&self) -> bool {
        matches!(self, GateResult::Pass | GateResult::Skipped(_))
    }

    /// Get failures if any.
    pub fn failures(&self) -> Vec<FailureDetail> {
        match self {
            GateResult::Pass | GateResult::Skipped(_) => Vec::new(),
            GateResult::Fail(f) => f.clone(),
        }
    }
}

/// Configuration for a validation gate.
#[derive(Debug, Clone)]
pub enum GateConfig {
    /// Format/structure validation.
    Format {
        /// Loop type for format validation (creates validator on demand).
        loop_type: LoopType,
    },

    /// Command execution (tests, lint, etc.).
    Command {
        /// Command to run.
        command: String,
        /// Expected exit code.
        exit_code: i32,
        /// Timeout.
        timeout: Duration,
    },

    /// LLM-as-judge validation.
    LlmJudge {
        /// Criteria to evaluate.
        criteria: JudgeCriteria,
        /// Timeout for judge call.
        timeout: Duration,
    },
}

/// A validation gate in the pipeline.
#[derive(Debug, Clone)]
pub struct Gate {
    /// Name of this gate.
    pub name: String,
    /// Configuration for this gate.
    pub config: GateConfig,
}

impl Gate {
    /// Create a new format validation gate.
    pub fn format(loop_type: LoopType) -> Self {
        Self {
            name: format!("{}_format", loop_type.as_str()),
            config: GateConfig::Format { loop_type },
        }
    }

    /// Create a new command gate.
    pub fn command(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            config: GateConfig::Command {
                command: command.into(),
                exit_code: 0,
                timeout: Duration::from_secs(300),
            },
        }
    }

    /// Create a new LLM judge gate.
    pub fn llm_judge(name: impl Into<String>, criteria: JudgeCriteria) -> Self {
        Self {
            name: name.into(),
            config: GateConfig::LlmJudge {
                criteria,
                timeout: Duration::from_secs(60),
            },
        }
    }

    /// Set timeout (for command or judge gates).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        match &mut self.config {
            GateConfig::Command { timeout: t, .. } => *t = timeout,
            GateConfig::LlmJudge { timeout: t, .. } => *t = timeout,
            GateConfig::Format { .. } => {} // Format doesn't have timeout
        }
        self
    }
}

/// Composite validation pipeline.
pub struct CompositeValidator {
    /// Gates to run in order.
    gates: Vec<Gate>,
    /// LLM client for judge gates (optional).
    llm_client: Option<Arc<dyn LlmClient>>,
}

impl CompositeValidator {
    /// Create a new empty validator.
    pub fn new() -> Self {
        Self {
            gates: Vec::new(),
            llm_client: None,
        }
    }

    /// Set the LLM client for judge gates.
    pub fn with_llm_client(mut self, client: Arc<dyn LlmClient>) -> Self {
        self.llm_client = Some(client);
        self
    }

    /// Add a gate to the pipeline.
    pub fn add_gate(mut self, gate: Gate) -> Self {
        self.gates.push(gate);
        self
    }

    /// Add multiple gates.
    pub fn with_gates(mut self, gates: impl IntoIterator<Item = Gate>) -> Self {
        self.gates.extend(gates);
        self
    }

    /// Run validation against content and/or working directory.
    ///
    /// - `content`: Artifact content for format and judge validation
    /// - `working_dir`: Directory for command execution
    /// - `iteration`: Current iteration number for feedback
    pub async fn validate(
        &self,
        content: Option<&str>,
        working_dir: Option<&Path>,
        iteration: u32,
    ) -> Result<ValidationOutcome, ValidationPipelineError> {
        let start = Instant::now();

        for gate in &self.gates {
            let result = self.run_gate(gate, content, working_dir).await?;

            match result {
                GateResult::Pass => continue,
                GateResult::Skipped(reason) => {
                    tracing::debug!(gate = %gate.name, reason = %reason, "Gate skipped");
                    continue;
                }
                GateResult::Fail(failures) => {
                    // Fail fast: return on first failure
                    let feedback =
                        IterationFeedback::fail(iteration, &gate.name, failures, start.elapsed().as_millis() as u64);
                    return Ok(ValidationOutcome::Fail(feedback));
                }
            }
        }

        Ok(ValidationOutcome::Pass)
    }

    /// Run a single gate.
    async fn run_gate(
        &self,
        gate: &Gate,
        content: Option<&str>,
        working_dir: Option<&Path>,
    ) -> Result<GateResult, ValidationPipelineError> {
        match &gate.config {
            GateConfig::Format { loop_type } => {
                let Some(content) = content else {
                    return Ok(GateResult::Skipped("No content for format validation".to_string()));
                };

                let validator = FormatValidator::for_loop_type(*loop_type);
                let result = validator.validate(content);

                match result {
                    FormatValidationResult::Pass => Ok(GateResult::Pass),
                    FormatValidationResult::Fail(failures) => Ok(GateResult::Fail(failures)),
                }
            }

            GateConfig::Command {
                command,
                exit_code,
                timeout,
            } => {
                let Some(working_dir) = working_dir else {
                    return Ok(GateResult::Skipped("No working directory for command".to_string()));
                };

                let config = TestRunnerConfig::new(command)
                    .with_exit_code(*exit_code)
                    .with_timeout(*timeout);
                let runner = TestRunner::new(config);

                let result = runner
                    .run(working_dir)
                    .await
                    .map_err(ValidationPipelineError::TestRunner)?;

                if result.passed {
                    Ok(GateResult::Pass)
                } else {
                    Ok(GateResult::Fail(result.failures))
                }
            }

            GateConfig::LlmJudge { criteria, timeout } => {
                let Some(content) = content else {
                    return Ok(GateResult::Skipped("No content for judge validation".to_string()));
                };

                let Some(client) = &self.llm_client else {
                    return Ok(GateResult::Skipped("No LLM client for judge validation".to_string()));
                };

                let judge = LlmJudge::new(Arc::clone(client)).with_timeout(*timeout);

                let result = judge
                    .judge(criteria, content)
                    .await
                    .map_err(ValidationPipelineError::LlmJudge)?;

                if result.passed {
                    Ok(GateResult::Pass)
                } else {
                    Ok(GateResult::Fail(result.failures))
                }
            }
        }
    }
}

impl Default for CompositeValidator {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from the validation pipeline.
#[derive(Debug, thiserror::Error)]
pub enum ValidationPipelineError {
    #[error("Test runner error: {0}")]
    TestRunner(#[from] TestRunnerError),

    #[error("LLM judge error: {0}")]
    LlmJudge(#[from] JudgeError),
}

/// Pre-configured validation pipelines for each loop type.
pub struct ValidationPipeline;

impl ValidationPipeline {
    /// Create a validation pipeline for the given loop type.
    pub fn for_loop_type(loop_type: LoopType) -> CompositeValidator {
        match loop_type {
            LoopType::Plan => Self::for_plan(),
            LoopType::Spec => Self::for_spec(),
            LoopType::Phase => Self::for_phase(),
            LoopType::Ralph => Self::for_ralph(),
        }
    }

    /// Create a validation pipeline for plan loops.
    ///
    /// Plans are primarily validated by structure and LLM-as-judge.
    pub fn for_plan() -> CompositeValidator {
        CompositeValidator::new()
            .add_gate(Gate::format(LoopType::Plan))
            .add_gate(Gate::llm_judge("plan_judge", LlmJudge::plan_criteria()))
    }

    /// Create a validation pipeline for spec loops.
    ///
    /// Specs need structure validation and LLM-as-judge.
    pub fn for_spec() -> CompositeValidator {
        CompositeValidator::new()
            .add_gate(Gate::format(LoopType::Spec))
            .add_gate(Gate::llm_judge("spec_judge", LlmJudge::spec_criteria()))
    }

    /// Create a validation pipeline for phase loops.
    ///
    /// Phases run tests and optionally LLM-as-judge.
    pub fn for_phase() -> CompositeValidator {
        CompositeValidator::new()
            .add_gate(Gate::command("ci", "otto ci"))
            .add_gate(Gate::llm_judge("phase_judge", LlmJudge::phase_criteria()))
    }

    /// Create a validation pipeline for ralph loops.
    ///
    /// Ralph loops primarily rely on downstream gates (tests).
    pub fn for_ralph() -> CompositeValidator {
        CompositeValidator::new().add_gate(Gate::command("ci", "otto ci"))
    }

    /// Create a custom validation pipeline with the given command.
    pub fn with_command(command: impl Into<String>) -> CompositeValidator {
        CompositeValidator::new().add_gate(Gate::command("custom", command))
    }
}

/// Per-loop-type validation configuration.
#[derive(Debug, Clone)]
pub struct LoopTypeValidation {
    /// Loop type this config is for.
    pub loop_type: LoopType,

    /// Gates to run.
    pub gates: Vec<Gate>,

    /// Whether LLM-as-judge is required.
    pub requires_llm_judge: bool,
}

impl LoopTypeValidation {
    /// Create validation config for a loop type.
    pub fn for_type(loop_type: LoopType) -> Self {
        match loop_type {
            LoopType::Plan => Self {
                loop_type,
                gates: vec![
                    Gate::format(LoopType::Plan),
                    Gate::llm_judge("plan_quality", LlmJudge::plan_criteria()),
                ],
                requires_llm_judge: true,
            },
            LoopType::Spec => Self {
                loop_type,
                gates: vec![
                    Gate::format(LoopType::Spec),
                    Gate::llm_judge("spec_quality", LlmJudge::spec_criteria()),
                ],
                requires_llm_judge: true,
            },
            LoopType::Phase => Self {
                loop_type,
                gates: vec![
                    Gate::command("ci", "otto ci"),
                    Gate::llm_judge("phase_quality", LlmJudge::phase_criteria()),
                ],
                requires_llm_judge: false,
            },
            LoopType::Ralph => Self {
                loop_type,
                gates: vec![Gate::command("ci", "otto ci")],
                requires_llm_judge: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, StreamChunk, TokenUsage};
    use async_trait::async_trait;
    use tempfile::TempDir;
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
    fn test_validation_outcome_passed() {
        let pass = ValidationOutcome::Pass;
        assert!(pass.passed());
        assert!(pass.feedback().is_none());

        let fail = ValidationOutcome::Fail(IterationFeedback::fail(1, "test", Vec::new(), 100));
        assert!(!fail.passed());
        assert!(fail.feedback().is_some());
    }

    #[test]
    fn test_gate_result_passed() {
        assert!(GateResult::Pass.passed());
        assert!(GateResult::Skipped("reason".to_string()).passed());
        assert!(!GateResult::Fail(Vec::new()).passed());
    }

    #[test]
    fn test_gate_format() {
        let gate = Gate::format(LoopType::Plan);
        assert_eq!(gate.name, "plan_format");
        assert!(matches!(gate.config, GateConfig::Format { .. }));
    }

    #[test]
    fn test_gate_command() {
        let gate = Gate::command("test", "cargo test");
        assert_eq!(gate.name, "test");
        assert!(matches!(gate.config, GateConfig::Command { .. }));
    }

    #[test]
    fn test_gate_llm_judge() {
        let criteria = JudgeCriteria::new("Test").with_question("Is it good?");
        let gate = Gate::llm_judge("judge", criteria);
        assert_eq!(gate.name, "judge");
        assert!(matches!(gate.config, GateConfig::LlmJudge { .. }));
    }

    #[test]
    fn test_gate_with_timeout() {
        let gate = Gate::command("test", "cargo test").with_timeout(Duration::from_secs(60));

        if let GateConfig::Command { timeout, .. } = gate.config {
            assert_eq!(timeout, Duration::from_secs(60));
        } else {
            panic!("Expected command config");
        }
    }

    #[tokio::test]
    async fn test_composite_validator_format_pass() {
        let content = r#"# Plan

## Summary
A plan.

## Goals
- Goal

## Non-Goals
- Non-goal

## Proposed Solution
Solution.

## Specs

### Spec 1: Core
Spec.

## Risks
Risks.
"#;

        let validator = CompositeValidator::new().add_gate(Gate::format(LoopType::Plan));

        let result = validator.validate(Some(content), None, 1).await.unwrap();
        assert!(result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_format_fail() {
        let content = "# Plan\n\nIncomplete plan.";

        let validator = CompositeValidator::new().add_gate(Gate::format(LoopType::Plan));

        let result = validator.validate(Some(content), None, 1).await.unwrap();
        assert!(!result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_command_pass() {
        let temp = TempDir::new().unwrap();

        let validator = CompositeValidator::new().add_gate(Gate::command("true", "true"));

        let result = validator.validate(None, Some(temp.path()), 1).await.unwrap();
        assert!(result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_command_fail() {
        let temp = TempDir::new().unwrap();

        let validator = CompositeValidator::new().add_gate(Gate::command("false", "false"));

        let result = validator.validate(None, Some(temp.path()), 1).await.unwrap();
        assert!(!result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_llm_judge_pass() {
        let client = Arc::new(MockLlmClient::new("PASS"));
        let criteria = JudgeCriteria::new("Test").with_question("Is it good?");

        let validator = CompositeValidator::new()
            .with_llm_client(client)
            .add_gate(Gate::llm_judge("judge", criteria));

        let result = validator.validate(Some("content"), None, 1).await.unwrap();
        assert!(result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_llm_judge_fail() {
        let client = Arc::new(MockLlmClient::new("FAIL: Missing details"));
        let criteria = JudgeCriteria::new("Test").with_question("Is it good?");

        let validator = CompositeValidator::new()
            .with_llm_client(client)
            .add_gate(Gate::llm_judge("judge", criteria));

        let result = validator.validate(Some("content"), None, 1).await.unwrap();
        assert!(!result.passed());
    }

    #[tokio::test]
    async fn test_composite_validator_fail_fast() {
        let temp = TempDir::new().unwrap();

        // First gate fails, second should not run
        let validator = CompositeValidator::new()
            .add_gate(Gate::command("first", "false"))
            .add_gate(Gate::command("second", "true"));

        let result = validator.validate(None, Some(temp.path()), 1).await.unwrap();
        assert!(!result.passed());

        // Feedback should be from "first" gate
        let feedback = result.feedback().unwrap();
        assert_eq!(feedback.validation_type, "first");
    }

    #[tokio::test]
    async fn test_composite_validator_skip_no_content() {
        let validator = CompositeValidator::new().add_gate(Gate::format(LoopType::Plan));

        // No content provided - format gate should be skipped
        let result = validator.validate(None, None, 1).await.unwrap();
        assert!(result.passed()); // Skipped = pass
    }

    #[tokio::test]
    async fn test_composite_validator_skip_no_llm() {
        let criteria = JudgeCriteria::new("Test").with_question("Is it good?");

        let validator = CompositeValidator::new()
            // No LLM client set
            .add_gate(Gate::llm_judge("judge", criteria));

        let result = validator.validate(Some("content"), None, 1).await.unwrap();
        assert!(result.passed()); // Skipped = pass
    }

    #[test]
    fn test_validation_pipeline_for_loop_type() {
        let plan = ValidationPipeline::for_loop_type(LoopType::Plan);
        assert!(!plan.gates.is_empty());

        let spec = ValidationPipeline::for_loop_type(LoopType::Spec);
        assert!(!spec.gates.is_empty());

        let phase = ValidationPipeline::for_loop_type(LoopType::Phase);
        assert!(!phase.gates.is_empty());

        let ralph = ValidationPipeline::for_loop_type(LoopType::Ralph);
        assert!(!ralph.gates.is_empty());
    }

    #[test]
    fn test_validation_pipeline_with_command() {
        let pipeline = ValidationPipeline::with_command("cargo test");
        assert_eq!(pipeline.gates.len(), 1);
        assert_eq!(pipeline.gates[0].name, "custom");
    }

    #[test]
    fn test_loop_type_validation() {
        let plan = LoopTypeValidation::for_type(LoopType::Plan);
        assert!(plan.requires_llm_judge);

        let ralph = LoopTypeValidation::for_type(LoopType::Ralph);
        assert!(!ralph.requires_llm_judge);
    }
}
