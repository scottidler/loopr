//! Structured feedback for validation failures.
//!
//! When validation fails, we need actionable feedback that the LLM can use
//! to fix issues in the next iteration. This module provides types for
//! capturing and formatting that feedback.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Category of failure for better organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCategory {
    /// Test failure (cargo test, npm test, etc.)
    Test,
    /// Lint failure (clippy, eslint, etc.)
    Lint,
    /// Type check failure (tsc, cargo check, etc.)
    Type,
    /// Format check failure (cargo fmt, prettier, etc.)
    Format,
    /// Structure validation failure (missing sections, etc.)
    Structure,
    /// LLM judge rejection
    Judge,
    /// Command execution failure
    Command,
    /// Timeout
    Timeout,
}

impl FailureCategory {
    /// Get a human-readable name for the category.
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureCategory::Test => "test",
            FailureCategory::Lint => "lint",
            FailureCategory::Type => "type",
            FailureCategory::Format => "format",
            FailureCategory::Structure => "structure",
            FailureCategory::Judge => "judge",
            FailureCategory::Command => "command",
            FailureCategory::Timeout => "timeout",
        }
    }
}

impl std::fmt::Display for FailureCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Detailed information about a single failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDetail {
    /// Category of the failure.
    pub category: FailureCategory,

    /// Human-readable failure message.
    pub message: String,

    /// File involved, if applicable.
    pub file: Option<String>,

    /// Line number, if applicable.
    pub line: Option<u32>,

    /// Column number, if applicable.
    pub column: Option<u32>,

    /// Additional context (e.g., expected vs actual values).
    pub context: Option<String>,
}

impl FailureDetail {
    /// Create a new failure detail.
    pub fn new(category: FailureCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            file: None,
            line: None,
            column: None,
            context: None,
        }
    }

    /// Set the file location.
    pub fn with_location(mut self, file: impl Into<String>, line: Option<u32>, column: Option<u32>) -> Self {
        self.file = Some(file.into());
        self.line = line;
        self.column = column;
        self
    }

    /// Set additional context.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Format the location string (e.g., "src/main.rs:42:10").
    pub fn location_string(&self) -> Option<String> {
        self.file.as_ref().map(|f| {
            let mut loc = f.clone();
            if let Some(line) = self.line {
                loc.push_str(&format!(":{}", line));
                if let Some(col) = self.column {
                    loc.push_str(&format!(":{}", col));
                }
            }
            loc
        })
    }
}

/// Feedback from a single iteration's validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IterationFeedback {
    /// Which iteration this feedback is from.
    pub iteration: u32,

    /// Type of validation that ran (command, llm-judge, composite).
    pub validation_type: String,

    /// Whether validation passed.
    pub passed: bool,

    /// List of failures (empty if passed).
    pub failures: Vec<FailureDetail>,

    /// When the validation ran.
    pub timestamp: DateTime<Utc>,

    /// How long validation took in milliseconds.
    pub duration_ms: u64,
}

impl IterationFeedback {
    /// Create feedback for a passing validation.
    pub fn pass(iteration: u32, validation_type: impl Into<String>, duration_ms: u64) -> Self {
        Self {
            iteration,
            validation_type: validation_type.into(),
            passed: true,
            failures: Vec::new(),
            timestamp: Utc::now(),
            duration_ms,
        }
    }

    /// Create feedback for a failing validation.
    pub fn fail(
        iteration: u32,
        validation_type: impl Into<String>,
        failures: Vec<FailureDetail>,
        duration_ms: u64,
    ) -> Self {
        Self {
            iteration,
            validation_type: validation_type.into(),
            passed: false,
            failures,
            timestamp: Utc::now(),
            duration_ms,
        }
    }

    /// Add a failure to this feedback.
    pub fn add_failure(&mut self, failure: FailureDetail) {
        self.passed = false;
        self.failures.push(failure);
    }
}

/// Formatter for incorporating feedback into prompts.
pub struct FeedbackFormatter {
    /// Maximum number of lines to include from output.
    pub max_output_lines: usize,

    /// Maximum number of failures to show per category.
    pub max_failures_per_category: usize,

    /// Whether to include timestamps.
    pub include_timestamps: bool,
}

impl Default for FeedbackFormatter {
    fn default() -> Self {
        Self {
            max_output_lines: 50,
            max_failures_per_category: 10,
            include_timestamps: false,
        }
    }
}

impl FeedbackFormatter {
    /// Create a new formatter with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum output lines.
    pub fn with_max_output_lines(mut self, max: usize) -> Self {
        self.max_output_lines = max;
        self
    }

    /// Set the maximum failures per category.
    pub fn with_max_failures_per_category(mut self, max: usize) -> Self {
        self.max_failures_per_category = max;
        self
    }

    /// Format a single iteration's feedback for inclusion in a prompt.
    pub fn format_single(&self, feedback: &IterationFeedback) -> String {
        if feedback.passed {
            return String::new();
        }

        let mut output = String::new();

        output.push_str(&format!(
            "### Iteration {} Failures ({} validation)\n\n",
            feedback.iteration, feedback.validation_type
        ));

        // Group failures by category
        let mut by_category: std::collections::HashMap<FailureCategory, Vec<&FailureDetail>> =
            std::collections::HashMap::new();

        for failure in &feedback.failures {
            by_category.entry(failure.category).or_default().push(failure);
        }

        for (category, failures) in by_category {
            output.push_str(&format!("**{}**:\n", category));

            let shown = failures.iter().take(self.max_failures_per_category);
            for failure in shown {
                if let Some(loc) = failure.location_string() {
                    output.push_str(&format!("- {} ({})\n", failure.message, loc));
                } else {
                    output.push_str(&format!("- {}\n", failure.message));
                }

                if let Some(ctx) = &failure.context {
                    let truncated = truncate_lines(ctx, 5);
                    output.push_str(&format!("  ```\n{}\n  ```\n", indent_text(&truncated, "  ")));
                }
            }

            if failures.len() > self.max_failures_per_category {
                output.push_str(&format!(
                    "- ... and {} more {} failures\n",
                    failures.len() - self.max_failures_per_category,
                    category
                ));
            }

            output.push('\n');
        }

        output
    }

    /// Format a history of iteration feedback for a prompt.
    pub fn format_history(&self, history: &[IterationFeedback]) -> String {
        if history.is_empty() {
            return String::new();
        }

        let failed_iterations: Vec<_> = history.iter().filter(|f| !f.passed).collect();
        if failed_iterations.is_empty() {
            return String::new();
        }

        let mut output = String::new();
        output.push_str("## Previous Iteration Results\n\n");

        // Show summary of all failed iterations
        if failed_iterations.len() > 1 {
            output.push_str("**Summary:**\n");
            for fb in &failed_iterations {
                let categories: Vec<_> = fb
                    .failures
                    .iter()
                    .map(|f| f.category.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                output.push_str(&format!(
                    "- Iteration {}: {} failure(s) in {}\n",
                    fb.iteration,
                    fb.failures.len(),
                    categories.join(", ")
                ));
            }
            output.push('\n');
        }

        // Show detailed feedback for the most recent failure
        if let Some(latest) = failed_iterations.last() {
            output.push_str("**Most recent failure (focus on fixing this first):**\n\n");
            output.push_str(&self.format_single(latest));
        }

        output
    }
}

/// Truncate text to a maximum number of lines.
fn truncate_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().take(max_lines).collect();
    let truncated = lines.len() < text.lines().count();
    let mut result = lines.join("\n");
    if truncated {
        result.push_str("\n... (truncated)");
    }
    result
}

/// Indent all lines of text by a prefix.
fn indent_text(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|line| format!("{}{}", prefix, line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_failure_category_as_str() {
        assert_eq!(FailureCategory::Test.as_str(), "test");
        assert_eq!(FailureCategory::Lint.as_str(), "lint");
        assert_eq!(FailureCategory::Judge.as_str(), "judge");
    }

    #[test]
    fn test_failure_detail_new() {
        let detail = FailureDetail::new(FailureCategory::Test, "test_foo failed");
        assert_eq!(detail.category, FailureCategory::Test);
        assert_eq!(detail.message, "test_foo failed");
        assert!(detail.file.is_none());
    }

    #[test]
    fn test_failure_detail_with_location() {
        let detail = FailureDetail::new(FailureCategory::Lint, "unused variable").with_location(
            "src/main.rs",
            Some(42),
            Some(10),
        );

        assert_eq!(detail.file, Some("src/main.rs".to_string()));
        assert_eq!(detail.line, Some(42));
        assert_eq!(detail.column, Some(10));
        assert_eq!(detail.location_string(), Some("src/main.rs:42:10".to_string()));
    }

    #[test]
    fn test_failure_detail_location_string() {
        // No location
        let detail = FailureDetail::new(FailureCategory::Test, "error");
        assert_eq!(detail.location_string(), None);

        // File only
        let detail = FailureDetail::new(FailureCategory::Test, "error").with_location("src/lib.rs", None, None);
        assert_eq!(detail.location_string(), Some("src/lib.rs".to_string()));

        // File + line
        let detail = FailureDetail::new(FailureCategory::Test, "error").with_location("src/lib.rs", Some(10), None);
        assert_eq!(detail.location_string(), Some("src/lib.rs:10".to_string()));
    }

    #[test]
    fn test_iteration_feedback_pass() {
        let feedback = IterationFeedback::pass(1, "command", 1000);
        assert!(feedback.passed);
        assert!(feedback.failures.is_empty());
        assert_eq!(feedback.iteration, 1);
    }

    #[test]
    fn test_iteration_feedback_fail() {
        let failures = vec![
            FailureDetail::new(FailureCategory::Test, "test1 failed"),
            FailureDetail::new(FailureCategory::Test, "test2 failed"),
        ];
        let feedback = IterationFeedback::fail(2, "composite", failures, 2000);

        assert!(!feedback.passed);
        assert_eq!(feedback.failures.len(), 2);
        assert_eq!(feedback.iteration, 2);
    }

    #[test]
    fn test_iteration_feedback_add_failure() {
        let mut feedback = IterationFeedback::pass(1, "command", 1000);
        assert!(feedback.passed);

        feedback.add_failure(FailureDetail::new(FailureCategory::Lint, "warning"));
        assert!(!feedback.passed);
        assert_eq!(feedback.failures.len(), 1);
    }

    #[test]
    fn test_feedback_formatter_single_pass() {
        let formatter = FeedbackFormatter::new();
        let feedback = IterationFeedback::pass(1, "command", 1000);
        let output = formatter.format_single(&feedback);
        assert!(output.is_empty());
    }

    #[test]
    fn test_feedback_formatter_single_fail() {
        let formatter = FeedbackFormatter::new();
        let failures = vec![
            FailureDetail::new(FailureCategory::Test, "test_foo failed").with_location("src/lib.rs", Some(42), None),
        ];
        let feedback = IterationFeedback::fail(1, "command", failures, 1000);
        let output = formatter.format_single(&feedback);

        assert!(output.contains("Iteration 1"));
        assert!(output.contains("test_foo failed"));
        assert!(output.contains("src/lib.rs:42"));
    }

    #[test]
    fn test_feedback_formatter_history() {
        let formatter = FeedbackFormatter::new();
        let history = vec![
            IterationFeedback::fail(
                1,
                "command",
                vec![FailureDetail::new(FailureCategory::Test, "error1")],
                1000,
            ),
            IterationFeedback::fail(
                2,
                "command",
                vec![FailureDetail::new(FailureCategory::Lint, "error2")],
                1000,
            ),
        ];

        let output = formatter.format_history(&history);
        assert!(output.contains("Previous Iteration Results"));
        assert!(output.contains("Iteration 1"));
        assert!(output.contains("Iteration 2"));
        assert!(output.contains("focus on fixing this first"));
    }

    #[test]
    fn test_feedback_formatter_empty_history() {
        let formatter = FeedbackFormatter::new();
        let output = formatter.format_history(&[]);
        assert!(output.is_empty());
    }

    #[test]
    fn test_truncate_lines() {
        let text = "line1\nline2\nline3\nline4\nline5";
        let truncated = truncate_lines(text, 3);
        assert!(truncated.contains("line1"));
        assert!(truncated.contains("line2"));
        assert!(truncated.contains("line3"));
        assert!(truncated.contains("truncated"));
        assert!(!truncated.contains("line4"));
    }

    #[test]
    fn test_indent_text() {
        let text = "line1\nline2";
        let indented = indent_text(text, "  ");
        assert_eq!(indented, "  line1\n  line2");
    }
}
