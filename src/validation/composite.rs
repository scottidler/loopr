// Phase 7: Validation System - Composite Validator
// Chains multiple validators together

use crate::error::Result;
use crate::validation::traits::{ValidationResult, Validator};
use async_trait::async_trait;
use std::path::Path;

/// A composite validator that chains multiple validators together.
/// All validators must pass for the overall validation to pass.
pub struct CompositeValidator {
    /// The validators to run in sequence
    validators: Vec<Box<dyn Validator>>,
    /// Description of what this composite validates
    description: String,
}

impl CompositeValidator {
    /// Create a new empty composite validator
    pub fn new() -> Self {
        Self {
            validators: Vec::new(),
            description: "composite validator".to_string(),
        }
    }

    /// Create a new composite validator with a custom description
    pub fn with_description(description: impl Into<String>) -> Self {
        Self {
            validators: Vec::new(),
            description: description.into(),
        }
    }

    /// Add a validator to the chain (builder pattern)
    pub fn with_validator(mut self, validator: impl Validator + 'static) -> Self {
        self.validators.push(Box::new(validator));
        self
    }

    /// Add a boxed validator to the chain
    pub fn add_boxed(mut self, validator: Box<dyn Validator>) -> Self {
        self.validators.push(validator);
        self
    }

    /// Get the number of validators in the chain
    pub fn len(&self) -> usize {
        self.validators.len()
    }

    /// Check if the composite has no validators
    pub fn is_empty(&self) -> bool {
        self.validators.is_empty()
    }

    /// Get descriptions of all validators in the chain
    pub fn validator_descriptions(&self) -> Vec<&str> {
        self.validators.iter().map(|v| v.description()).collect()
    }
}

impl Default for CompositeValidator {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Validator for CompositeValidator {
    async fn validate(&self, artifact: &Path, worktree: &Path) -> Result<ValidationResult> {
        let mut combined = ValidationResult::pass();

        for validator in &self.validators {
            let result = validator.validate(artifact, worktree).await?;
            combined.merge(result);
        }

        Ok(combined)
    }

    fn description(&self) -> &str {
        &self.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mock validator for testing
    struct MockValidator {
        should_pass: bool,
        error_msg: String,
        desc: String,
    }

    impl MockValidator {
        fn passing(desc: &str) -> Self {
            Self {
                should_pass: true,
                error_msg: String::new(),
                desc: desc.to_string(),
            }
        }

        fn failing(desc: &str, error: &str) -> Self {
            Self {
                should_pass: false,
                error_msg: error.to_string(),
                desc: desc.to_string(),
            }
        }
    }

    #[async_trait]
    impl Validator for MockValidator {
        async fn validate(&self, _artifact: &Path, _worktree: &Path) -> Result<ValidationResult> {
            if self.should_pass {
                Ok(ValidationResult::pass_with_output(format!(
                    "{} passed",
                    self.desc
                )))
            } else {
                Ok(ValidationResult::fail(&self.error_msg))
            }
        }

        fn description(&self) -> &str {
            &self.desc
        }
    }

    #[test]
    fn test_composite_new() {
        let composite = CompositeValidator::new();
        assert!(composite.is_empty());
        assert_eq!(composite.len(), 0);
    }

    #[test]
    fn test_composite_with_description() {
        let composite = CompositeValidator::with_description("plan validation");
        assert_eq!(composite.description(), "plan validation");
    }

    #[test]
    fn test_composite_add() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("v1"))
            .with_validator(MockValidator::passing("v2"));

        assert_eq!(composite.len(), 2);
        assert!(!composite.is_empty());
    }

    #[test]
    fn test_composite_validator_descriptions() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("format check"))
            .with_validator(MockValidator::passing("command check"));

        let descriptions = composite.validator_descriptions();
        assert_eq!(descriptions, vec!["format check", "command check"]);
    }

    #[tokio::test]
    async fn test_composite_all_pass() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("v1"))
            .with_validator(MockValidator::passing("v2"))
            .with_validator(MockValidator::passing("v3"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(result.passed);
        assert!(result.output.contains("v1 passed"));
        assert!(result.output.contains("v2 passed"));
        assert!(result.output.contains("v3 passed"));
    }

    #[tokio::test]
    async fn test_composite_one_fails() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("v1"))
            .with_validator(MockValidator::failing("v2", "validation error"))
            .with_validator(MockValidator::passing("v3"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(!result.passed);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0], "validation error");
    }

    #[tokio::test]
    async fn test_composite_multiple_fail() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::failing("v1", "error 1"))
            .with_validator(MockValidator::passing("v2"))
            .with_validator(MockValidator::failing("v3", "error 2"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(!result.passed);
        assert_eq!(result.errors.len(), 2);
        assert!(result.errors.contains(&"error 1".to_string()));
        assert!(result.errors.contains(&"error 2".to_string()));
    }

    #[tokio::test]
    async fn test_composite_empty() {
        let composite = CompositeValidator::new();

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_composite_collects_all_output() {
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("format"))
            .with_validator(MockValidator::passing("cargo"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(result.output.contains("format passed"));
        assert!(result.output.contains("cargo passed"));
    }

    #[test]
    fn test_composite_default() {
        let composite = CompositeValidator::default();
        assert!(composite.is_empty());
    }

    #[tokio::test]
    async fn test_composite_single_validator_pass() {
        let composite = CompositeValidator::new().with_validator(MockValidator::passing("only"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_composite_single_validator_fail() {
        let composite = CompositeValidator::new().with_validator(MockValidator::failing("only", "failed"));

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(!result.passed);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn test_composite_add_boxed() {
        let validator: Box<dyn Validator> = Box::new(MockValidator::passing("boxed"));
        let composite = CompositeValidator::new().add_boxed(validator);

        assert_eq!(composite.len(), 1);
    }

    #[tokio::test]
    async fn test_composite_with_boxed_validators() {
        let v1: Box<dyn Validator> = Box::new(MockValidator::passing("boxed1"));
        let v2: Box<dyn Validator> = Box::new(MockValidator::passing("boxed2"));

        let composite = CompositeValidator::new().add_boxed(v1).add_boxed(v2);

        let result = composite
            .validate(Path::new("/tmp/artifact"), Path::new("/tmp/worktree"))
            .await
            .unwrap();

        assert!(result.passed);
    }

    #[test]
    fn test_composite_builder_pattern() {
        // Verify the builder pattern works fluently
        let composite = CompositeValidator::with_description("full validation")
            .with_validator(MockValidator::passing("format"))
            .with_validator(MockValidator::passing("lint"))
            .with_validator(MockValidator::passing("test"));

        assert_eq!(composite.len(), 3);
        assert_eq!(composite.description(), "full validation");
    }

    #[tokio::test]
    async fn test_composite_preserves_order() {
        // Verify validators run in the order they were added
        let composite = CompositeValidator::new()
            .with_validator(MockValidator::passing("first"))
            .with_validator(MockValidator::passing("second"))
            .with_validator(MockValidator::passing("third"));

        let descs = composite.validator_descriptions();
        assert_eq!(descs[0], "first");
        assert_eq!(descs[1], "second");
        assert_eq!(descs[2], "third");
    }
}
