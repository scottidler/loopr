// Phase 7: Validation System - Format Validator
// Checks that required markdown sections exist in artifacts

use crate::error::Result;
use crate::validation::traits::{ValidationResult, Validator};
use async_trait::async_trait;
use std::path::Path;

/// Configuration for which sections are required
#[derive(Debug, Clone)]
pub struct FormatConfig {
    /// Required section headings (e.g., "## Overview")
    pub required_sections: Vec<String>,
    /// Description of what this format validator checks
    pub description: String,
}

impl FormatConfig {
    /// Create a new format configuration
    pub fn new(required_sections: Vec<String>) -> Self {
        Self {
            required_sections,
            description: "format validator".to_string(),
        }
    }

    /// Create a new format configuration with description
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Create config for Plan validation
    /// Required: ## Overview, ## Phases, ## Success Criteria, ## Specs to Create
    pub fn plan() -> Self {
        Self::new(vec![
            "## Overview".to_string(),
            "## Phases".to_string(),
            "## Success Criteria".to_string(),
            "## Specs to Create".to_string(),
        ])
        .with_description("plan format validator")
    }

    /// Create config for Spec validation
    /// Required: ## Parent Plan, ## Overview, ## Phases
    pub fn spec() -> Self {
        Self::new(vec![
            "## Parent Plan".to_string(),
            "## Overview".to_string(),
            "## Phases".to_string(),
        ])
        .with_description("spec format validator")
    }

    /// Create config for Phase validation
    /// Required: ## Task, ## Specific Work, ## Success Criteria
    pub fn phase() -> Self {
        Self::new(vec![
            "## Task".to_string(),
            "## Specific Work".to_string(),
            "## Success Criteria".to_string(),
        ])
        .with_description("phase format validator")
    }

    /// Create config for Code validation (no required sections)
    pub fn code() -> Self {
        Self::new(vec![]).with_description("code format validator")
    }
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self::new(vec![])
    }
}

/// Validator that checks markdown artifacts have required sections
pub struct FormatValidator {
    config: FormatConfig,
}

impl FormatValidator {
    /// Create a new format validator with the given configuration
    pub fn new(config: FormatConfig) -> Self {
        Self { config }
    }

    /// Create a format validator for plans
    pub fn for_plan() -> Self {
        Self::new(FormatConfig::plan())
    }

    /// Create a format validator for specs
    pub fn for_spec() -> Self {
        Self::new(FormatConfig::spec())
    }

    /// Create a format validator for phases
    pub fn for_phase() -> Self {
        Self::new(FormatConfig::phase())
    }

    /// Create a format validator for code (no format requirements)
    pub fn for_code() -> Self {
        Self::new(FormatConfig::code())
    }

    /// Check if content contains a required section
    fn has_section(content: &str, section: &str) -> bool {
        // Check for exact match at start of line
        content.lines().any(|line| line.trim() == section)
    }

    /// Find all missing sections
    fn find_missing_sections(&self, content: &str) -> Vec<String> {
        self.config
            .required_sections
            .iter()
            .filter(|section| !Self::has_section(content, section))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Validator for FormatValidator {
    async fn validate(&self, artifact: &Path, _worktree: &Path) -> Result<ValidationResult> {
        // Read the artifact file
        let content = match tokio::fs::read_to_string(artifact).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ValidationResult::fail(format!(
                    "Failed to read artifact {}: {}",
                    artifact.display(),
                    e
                )));
            }
        };

        // If no required sections, pass automatically
        if self.config.required_sections.is_empty() {
            return Ok(ValidationResult::pass_with_output("No format requirements to check"));
        }

        // Find missing sections
        let missing = self.find_missing_sections(&content);

        if missing.is_empty() {
            Ok(ValidationResult::pass_with_output(format!(
                "All {} required sections found",
                self.config.required_sections.len()
            )))
        } else {
            let errors: Vec<String> = missing
                .iter()
                .map(|s| format!("Missing required section: {}", s))
                .collect();
            Ok(ValidationResult::fail_with_errors(errors))
        }
    }

    fn description(&self) -> &str {
        &self.config.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    async fn create_temp_file(dir: &TempDir, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        fs::write(&path, content).await.unwrap();
        path
    }

    #[test]
    fn test_format_config_new() {
        let config = FormatConfig::new(vec!["## Section".to_string()]);
        assert_eq!(config.required_sections.len(), 1);
        assert_eq!(config.description, "format validator");
    }

    #[test]
    fn test_format_config_with_description() {
        let config = FormatConfig::new(vec![]).with_description("custom desc");
        assert_eq!(config.description, "custom desc");
    }

    #[test]
    fn test_format_config_plan() {
        let config = FormatConfig::plan();
        assert_eq!(config.required_sections.len(), 4);
        assert!(config.required_sections.contains(&"## Overview".to_string()));
        assert!(config.required_sections.contains(&"## Phases".to_string()));
        assert!(config.required_sections.contains(&"## Success Criteria".to_string()));
        assert!(config.required_sections.contains(&"## Specs to Create".to_string()));
    }

    #[test]
    fn test_format_config_spec() {
        let config = FormatConfig::spec();
        assert_eq!(config.required_sections.len(), 3);
        assert!(config.required_sections.contains(&"## Parent Plan".to_string()));
        assert!(config.required_sections.contains(&"## Overview".to_string()));
        assert!(config.required_sections.contains(&"## Phases".to_string()));
    }

    #[test]
    fn test_format_config_phase() {
        let config = FormatConfig::phase();
        assert_eq!(config.required_sections.len(), 3);
        assert!(config.required_sections.contains(&"## Task".to_string()));
        assert!(config.required_sections.contains(&"## Specific Work".to_string()));
        assert!(config.required_sections.contains(&"## Success Criteria".to_string()));
    }

    #[test]
    fn test_format_config_code() {
        let config = FormatConfig::code();
        assert!(config.required_sections.is_empty());
    }

    #[test]
    fn test_format_config_default() {
        let config = FormatConfig::default();
        assert!(config.required_sections.is_empty());
    }

    #[test]
    fn test_has_section_present() {
        let content = "# Title\n\n## Overview\n\nSome content";
        assert!(FormatValidator::has_section(content, "## Overview"));
    }

    #[test]
    fn test_has_section_missing() {
        let content = "# Title\n\n## Introduction\n\nSome content";
        assert!(!FormatValidator::has_section(content, "## Overview"));
    }

    #[test]
    fn test_has_section_with_whitespace() {
        let content = "# Title\n\n  ## Overview  \n\nSome content";
        assert!(FormatValidator::has_section(content, "## Overview"));
    }

    #[test]
    fn test_has_section_partial_match() {
        // Should not match partial section names
        let content = "## Overview of the System";
        assert!(!FormatValidator::has_section(content, "## Overview"));
    }

    #[test]
    fn test_find_missing_sections_none_missing() {
        let config = FormatConfig::new(vec!["## A".to_string(), "## B".to_string()]);
        let validator = FormatValidator::new(config);
        let content = "## A\n\n## B\n";
        let missing = validator.find_missing_sections(content);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_find_missing_sections_some_missing() {
        let config = FormatConfig::new(vec!["## A".to_string(), "## B".to_string()]);
        let validator = FormatValidator::new(config);
        let content = "## A\n\n## C\n";
        let missing = validator.find_missing_sections(content);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], "## B");
    }

    #[test]
    fn test_find_missing_sections_all_missing() {
        let config = FormatConfig::new(vec!["## A".to_string(), "## B".to_string()]);
        let validator = FormatValidator::new(config);
        let content = "## C\n\n## D\n";
        let missing = validator.find_missing_sections(content);
        assert_eq!(missing.len(), 2);
    }

    #[tokio::test]
    async fn test_validate_all_sections_present() {
        let dir = TempDir::new().unwrap();
        let content = "# Plan\n\n## Overview\n\nContent\n\n## Phases\n\n1. Phase 1\n\n## Success Criteria\n\n- Pass\n\n## Specs to Create\n\n- spec-a";
        let artifact = create_temp_file(&dir, "plan.md", content).await;

        let validator = FormatValidator::for_plan();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(result.passed);
        assert!(result.output.contains("4 required sections found"));
    }

    #[tokio::test]
    async fn test_validate_missing_section() {
        let dir = TempDir::new().unwrap();
        let content = "# Plan\n\n## Overview\n\nContent\n\n## Phases\n\n1. Phase 1";
        let artifact = create_temp_file(&dir, "plan.md", content).await;

        let validator = FormatValidator::for_plan();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(!result.passed);
        assert!(result.errors.iter().any(|e| e.contains("## Success Criteria")));
        assert!(result.errors.iter().any(|e| e.contains("## Specs to Create")));
    }

    #[tokio::test]
    async fn test_validate_file_not_found() {
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("nonexistent.md");

        let validator = FormatValidator::for_plan();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(!result.passed);
        assert!(result.output.contains("Failed to read artifact"));
    }

    #[tokio::test]
    async fn test_validate_no_requirements() {
        let dir = TempDir::new().unwrap();
        let content = "# Some Code";
        let artifact = create_temp_file(&dir, "code.md", content).await;

        let validator = FormatValidator::for_code();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(result.passed);
        assert!(result.output.contains("No format requirements"));
    }

    #[tokio::test]
    async fn test_validate_spec_format() {
        let dir = TempDir::new().unwrap();
        let content = "# Spec\n\n## Parent Plan\n\nplan.md\n\n## Overview\n\nContent\n\n## Phases\n\n1. Phase 1";
        let artifact = create_temp_file(&dir, "spec.md", content).await;

        let validator = FormatValidator::for_spec();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(result.passed);
    }

    #[tokio::test]
    async fn test_validate_phase_format() {
        let dir = TempDir::new().unwrap();
        let content = "# Phase 1\n\n## Task\n\nDo thing\n\n## Specific Work\n\n- Item\n\n## Success Criteria\n\n- Pass";
        let artifact = create_temp_file(&dir, "phase.md", content).await;

        let validator = FormatValidator::for_phase();
        let result = validator.validate(&artifact, dir.path()).await.unwrap();

        assert!(result.passed);
    }

    #[test]
    fn test_validator_description() {
        let validator = FormatValidator::for_plan();
        assert_eq!(validator.description(), "plan format validator");

        let validator = FormatValidator::for_spec();
        assert_eq!(validator.description(), "spec format validator");

        let validator = FormatValidator::for_phase();
        assert_eq!(validator.description(), "phase format validator");

        let validator = FormatValidator::for_code();
        assert_eq!(validator.description(), "code format validator");
    }

    #[test]
    fn test_custom_config() {
        let config = FormatConfig::new(vec!["## Custom Section".to_string()]).with_description("custom validator");
        let validator = FormatValidator::new(config);
        assert_eq!(validator.description(), "custom validator");
    }
}
