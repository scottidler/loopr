// Phase 7: Validation System - Traits
// Core validation interfaces

use crate::error::Result;
use async_trait::async_trait;
use std::path::Path;

/// Result of a validation operation
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// Whether validation passed
    pub passed: bool,
    /// Output from validation (stdout, combined output, etc.)
    pub output: String,
    /// List of specific errors found
    pub errors: Vec<String>,
}

impl ValidationResult {
    /// Create a passing result
    pub fn pass() -> Self {
        Self {
            passed: true,
            output: String::new(),
            errors: Vec::new(),
        }
    }

    /// Create a passing result with output
    pub fn pass_with_output(output: impl Into<String>) -> Self {
        Self {
            passed: true,
            output: output.into(),
            errors: Vec::new(),
        }
    }

    /// Create a failing result with a single error
    pub fn fail(error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            passed: false,
            output: error.clone(),
            errors: vec![error],
        }
    }

    /// Create a failing result with multiple errors
    pub fn fail_with_errors(errors: Vec<String>) -> Self {
        let output = errors.join("\n");
        Self {
            passed: false,
            output,
            errors,
        }
    }

    /// Create a failing result with output and errors
    pub fn fail_with_output_and_errors(output: impl Into<String>, errors: Vec<String>) -> Self {
        Self {
            passed: false,
            output: output.into(),
            errors,
        }
    }

    /// Add an error to this result
    pub fn add_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.errors.push(error.clone());
        if !self.output.is_empty() {
            self.output.push('\n');
        }
        self.output.push_str(&error);
        self.passed = false;
    }

    /// Merge another result into this one
    pub fn merge(&mut self, other: ValidationResult) {
        if !other.passed {
            self.passed = false;
        }
        if !other.output.is_empty() {
            if !self.output.is_empty() {
                self.output.push('\n');
            }
            self.output.push_str(&other.output);
        }
        self.errors.extend(other.errors);
    }

    /// Check if there are any errors
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Get the number of errors
    pub fn error_count(&self) -> usize {
        self.errors.len()
    }
}

impl Default for ValidationResult {
    fn default() -> Self {
        Self::pass()
    }
}

/// Trait for validators that check loop outputs
#[async_trait]
pub trait Validator: Send + Sync {
    /// Validate an artifact in the context of a worktree
    ///
    /// # Arguments
    /// * `artifact` - Path to the artifact being validated (e.g., plan.md)
    /// * `worktree` - Path to the git worktree where validation runs
    ///
    /// # Returns
    /// ValidationResult indicating pass/fail with details
    async fn validate(&self, artifact: &Path, worktree: &Path) -> Result<ValidationResult>;

    /// Get a description of what this validator checks
    fn description(&self) -> &str {
        "validator"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_result_pass() {
        let result = ValidationResult::pass();
        assert!(result.passed);
        assert!(result.output.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_validation_result_pass_with_output() {
        let result = ValidationResult::pass_with_output("all tests passed");
        assert!(result.passed);
        assert_eq!(result.output, "all tests passed");
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_validation_result_fail() {
        let result = ValidationResult::fail("missing required section");
        assert!(!result.passed);
        assert_eq!(result.output, "missing required section");
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0], "missing required section");
    }

    #[test]
    fn test_validation_result_fail_with_errors() {
        let errors = vec!["error 1".to_string(), "error 2".to_string()];
        let result = ValidationResult::fail_with_errors(errors);
        assert!(!result.passed);
        assert_eq!(result.output, "error 1\nerror 2");
        assert_eq!(result.errors.len(), 2);
    }

    #[test]
    fn test_validation_result_fail_with_output_and_errors() {
        let result = ValidationResult::fail_with_output_and_errors(
            "full output",
            vec!["specific error".to_string()],
        );
        assert!(!result.passed);
        assert_eq!(result.output, "full output");
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_validation_result_add_error() {
        let mut result = ValidationResult::pass();
        assert!(result.passed);

        result.add_error("new error");
        assert!(!result.passed);
        assert!(result.output.contains("new error"));
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_validation_result_add_error_appends() {
        let mut result = ValidationResult::pass_with_output("existing");
        result.add_error("new error");

        assert!(!result.passed);
        assert_eq!(result.output, "existing\nnew error");
    }

    #[test]
    fn test_validation_result_merge_passing() {
        let mut result1 = ValidationResult::pass_with_output("output 1");
        let result2 = ValidationResult::pass_with_output("output 2");

        result1.merge(result2);
        assert!(result1.passed);
        assert_eq!(result1.output, "output 1\noutput 2");
    }

    #[test]
    fn test_validation_result_merge_one_failing() {
        let mut result1 = ValidationResult::pass();
        let result2 = ValidationResult::fail("error");

        result1.merge(result2);
        assert!(!result1.passed);
        assert_eq!(result1.errors.len(), 1);
    }

    #[test]
    fn test_validation_result_merge_both_failing() {
        let mut result1 = ValidationResult::fail("error 1");
        let result2 = ValidationResult::fail("error 2");

        result1.merge(result2);
        assert!(!result1.passed);
        assert_eq!(result1.errors.len(), 2);
    }

    #[test]
    fn test_validation_result_has_errors() {
        let pass = ValidationResult::pass();
        let fail = ValidationResult::fail("error");

        assert!(!pass.has_errors());
        assert!(fail.has_errors());
    }

    #[test]
    fn test_validation_result_error_count() {
        let result = ValidationResult::fail_with_errors(vec![
            "e1".to_string(),
            "e2".to_string(),
            "e3".to_string(),
        ]);
        assert_eq!(result.error_count(), 3);
    }

    #[test]
    fn test_validation_result_default() {
        let result = ValidationResult::default();
        assert!(result.passed);
        assert!(result.output.is_empty());
        assert!(result.errors.is_empty());
    }

    // Mock validator for testing the trait
    struct MockValidator {
        should_pass: bool,
    }

    #[async_trait]
    impl Validator for MockValidator {
        async fn validate(&self, _artifact: &Path, _worktree: &Path) -> Result<ValidationResult> {
            if self.should_pass {
                Ok(ValidationResult::pass())
            } else {
                Ok(ValidationResult::fail("mock failure"))
            }
        }

        fn description(&self) -> &str {
            "mock validator"
        }
    }

    #[tokio::test]
    async fn test_validator_trait_pass() {
        let validator = MockValidator { should_pass: true };
        let result = validator
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();
        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_validator_trait_fail() {
        let validator = MockValidator { should_pass: false };
        let result = validator
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();
        assert!(!result.passed);
    }

    #[test]
    fn test_validator_description() {
        let validator = MockValidator { should_pass: true };
        assert_eq!(validator.description(), "mock validator");
    }
}
