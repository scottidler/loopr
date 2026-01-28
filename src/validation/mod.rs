//! Advanced validation module for Loopr.
//!
//! Implements the 3-layer backpressure system:
//! - **Layer 1: Format/syntax checks** - Structure validation for artifacts
//! - **Layer 2: Test execution** - Downstream gates (tests, lint, type-check)
//! - **Layer 3: LLM-as-judge** - Subjective criteria evaluation
//!
//! ## Key Concepts
//!
//! - **Composite validation**: Run multiple validation gates in sequence, fail-fast
//! - **Feedback incorporation**: Capture actionable feedback for next iteration
//! - **Per-loop-type validation**: Different loop types have different validators
//!
//! ## Usage
//!
//! ```rust,ignore
//! use loopr::validation::{CompositeValidator, Gate, ValidationPipeline};
//!
//! let pipeline = ValidationPipeline::for_loop_type(LoopType::Ralph)
//!     .with_timeout(Duration::from_secs(300));
//!
//! let result = pipeline.validate(&artifacts, working_dir).await?;
//! match result {
//!     ValidationOutcome::Pass => println!("All validations passed"),
//!     ValidationOutcome::Fail(feedback) => {
//!         println!("Failed: {}", feedback.format_for_prompt());
//!     }
//! }
//! ```

// Allow dead code and unused imports for now - this module is newly added and will be integrated soon
#![allow(dead_code)]
#![allow(unused_imports)]

mod feedback;
mod format;
mod llm_judge;
mod pipeline;
mod test_runner;

pub use feedback::{FailureCategory, FailureDetail, FeedbackFormatter, IterationFeedback};
pub use format::{FormatValidationResult, FormatValidator, StructureCheck};
pub use llm_judge::{JudgeCriteria, JudgeResult, LlmJudge};
pub use pipeline::{
    CompositeValidator, Gate, GateConfig, GateResult, LoopTypeValidation, ValidationOutcome, ValidationPipeline,
};
pub use test_runner::{TestResult, TestRunner, TestRunnerConfig};
