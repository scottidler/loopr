//! Layer 1: Format and structure validation.
//!
//! This layer validates the format and structure of artifacts before any
//! tests run. It catches:
//! - Missing required sections in plan.md/spec.md/phase.md
//! - Malformed markdown structure
//! - Invalid file formats
//!
//! Fast to run (no external commands), provides clear feedback.

use super::feedback::{FailureCategory, FailureDetail};
use crate::store::LoopType;

/// Result of format validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatValidationResult {
    /// All format checks passed.
    Pass,
    /// Some format checks failed.
    Fail(Vec<FailureDetail>),
}

impl FormatValidationResult {
    /// Check if validation passed.
    pub fn passed(&self) -> bool {
        matches!(self, FormatValidationResult::Pass)
    }

    /// Get failures if any.
    pub fn failures(&self) -> Option<&[FailureDetail]> {
        match self {
            FormatValidationResult::Pass => None,
            FormatValidationResult::Fail(f) => Some(f),
        }
    }

    /// Convert into failures vec.
    pub fn into_failures(self) -> Vec<FailureDetail> {
        match self {
            FormatValidationResult::Pass => Vec::new(),
            FormatValidationResult::Fail(f) => f,
        }
    }
}

/// A specific structure check to perform.
#[derive(Debug, Clone)]
pub struct StructureCheck {
    /// Name of the check.
    pub name: String,
    /// Pattern to look for (substring match).
    pub pattern: String,
    /// Whether this check is required (failure if missing).
    pub required: bool,
    /// Error message if check fails.
    pub error_message: String,
}

impl StructureCheck {
    /// Create a new required section check.
    pub fn required_section(section_name: &str, marker: &str) -> Self {
        Self {
            name: format!("section_{}", section_name.to_lowercase().replace(' ', "_")),
            pattern: marker.to_string(),
            required: true,
            error_message: format!("Missing required section: {}", section_name),
        }
    }

    /// Create a new optional check.
    pub fn optional(name: &str, pattern: &str, error_message: &str) -> Self {
        Self {
            name: name.to_string(),
            pattern: pattern.to_string(),
            required: false,
            error_message: error_message.to_string(),
        }
    }

    /// Run this check against content.
    pub fn check(&self, content: &str) -> Option<FailureDetail> {
        if self.required && !content.contains(&self.pattern) {
            Some(FailureDetail::new(FailureCategory::Structure, &self.error_message))
        } else {
            None
        }
    }
}

/// Validator for artifact format and structure.
///
/// Note: Cannot derive Clone/Debug due to the custom_checks closures.
pub struct FormatValidator {
    /// Checks to run.
    checks: Vec<StructureCheck>,
    /// Additional custom checks.
    #[allow(clippy::type_complexity)]
    custom_checks: Vec<Box<dyn Fn(&str) -> Option<FailureDetail> + Send + Sync>>,
}

impl std::fmt::Debug for FormatValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormatValidator")
            .field("checks", &self.checks)
            .field(
                "custom_checks",
                &format!("[{} custom checks]", self.custom_checks.len()),
            )
            .finish()
    }
}

impl FormatValidator {
    /// Create a new empty validator.
    pub fn new() -> Self {
        Self {
            checks: Vec::new(),
            custom_checks: Vec::new(),
        }
    }

    /// Create a validator for a specific loop type's artifact.
    pub fn for_loop_type(loop_type: LoopType) -> Self {
        match loop_type {
            LoopType::Plan => Self::for_plan(),
            LoopType::Spec => Self::for_spec(),
            LoopType::Phase => Self::for_phase(),
            LoopType::Ralph => Self::new(), // Ralph produces code, not structured artifacts
        }
    }

    /// Create a validator for plan.md artifacts.
    pub fn for_plan() -> Self {
        let mut validator = Self::new();

        // Required sections for a plan
        validator.checks.extend([
            StructureCheck::required_section("Summary", "## Summary"),
            StructureCheck::required_section("Goals", "## Goals"),
            StructureCheck::required_section("Non-Goals", "## Non-Goals"),
            StructureCheck::required_section("Proposed Solution", "## Proposed Solution"),
            StructureCheck::required_section("Specs", "## Specs"),
            StructureCheck::required_section("Risks", "## Risks"),
        ]);

        // Must have at least one spec defined
        validator.custom_checks.push(Box::new(|content| {
            if !content.contains("### Spec") {
                Some(FailureDetail::new(
                    FailureCategory::Structure,
                    "No specs defined (expected ### Spec N: <name>)",
                ))
            } else {
                None
            }
        }));

        validator
    }

    /// Create a validator for spec.md artifacts.
    pub fn for_spec() -> Self {
        let mut validator = Self::new();

        validator.checks.extend([
            StructureCheck::required_section("Overview", "## Overview"),
            StructureCheck::required_section("Requirements", "## Requirements"),
            StructureCheck::required_section("Acceptance Criteria", "## Acceptance Criteria"),
            StructureCheck::required_section("Phases", "## Phases"),
        ]);

        // Must have at least one phase defined
        validator.custom_checks.push(Box::new(|content| {
            if !content.contains("### Phase") {
                Some(FailureDetail::new(
                    FailureCategory::Structure,
                    "No phases defined (expected ### Phase N: <name>)",
                ))
            } else {
                None
            }
        }));

        validator
    }

    /// Create a validator for phase.md artifacts.
    pub fn for_phase() -> Self {
        let mut validator = Self::new();

        validator.checks.extend([
            StructureCheck::required_section("Goal", "## Goal"),
            StructureCheck::required_section("Tasks", "## Tasks"),
            StructureCheck::required_section("Acceptance Criteria", "## Acceptance Criteria"),
        ]);

        validator
    }

    /// Add a structure check.
    pub fn add_check(mut self, check: StructureCheck) -> Self {
        self.checks.push(check);
        self
    }

    /// Add a custom check function.
    pub fn add_custom_check<F>(mut self, check: F) -> Self
    where
        F: Fn(&str) -> Option<FailureDetail> + Send + Sync + 'static,
    {
        self.custom_checks.push(Box::new(check));
        self
    }

    /// Validate the given content.
    pub fn validate(&self, content: &str) -> FormatValidationResult {
        let mut failures = Vec::new();

        // Run structure checks
        for check in &self.checks {
            if let Some(failure) = check.check(content) {
                failures.push(failure);
            }
        }

        // Run custom checks
        for check in &self.custom_checks {
            if let Some(failure) = check(content) {
                failures.push(failure);
            }
        }

        // Check for ambiguous markers (TBD, TODO, FIXME, ???)
        let ambiguous_markers = ["TBD", "FIXME", "???"];
        for marker in ambiguous_markers {
            if content.contains(marker) {
                failures.push(FailureDetail::new(
                    FailureCategory::Structure,
                    format!("Contains ambiguous marker that should be resolved: {}", marker),
                ));
            }
        }

        if failures.is_empty() {
            FormatValidationResult::Pass
        } else {
            FormatValidationResult::Fail(failures)
        }
    }
}

impl Default for FormatValidator {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate plan format (convenience function).
pub fn validate_plan_format(content: &str) -> FormatValidationResult {
    FormatValidator::for_plan().validate(content)
}

/// Validate spec format (convenience function).
pub fn validate_spec_format(content: &str) -> FormatValidationResult {
    FormatValidator::for_spec().validate(content)
}

/// Validate phase format (convenience function).
pub fn validate_phase_format(content: &str) -> FormatValidationResult {
    FormatValidator::for_phase().validate(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_validation_result_passed() {
        let pass = FormatValidationResult::Pass;
        assert!(pass.passed());
        assert!(pass.failures().is_none());

        let fail = FormatValidationResult::Fail(vec![FailureDetail::new(FailureCategory::Structure, "error")]);
        assert!(!fail.passed());
        assert!(fail.failures().is_some());
    }

    #[test]
    fn test_structure_check_required_section() {
        let check = StructureCheck::required_section("Summary", "## Summary");

        // Content with section
        let result = check.check("# Plan\n\n## Summary\nThis is a plan.");
        assert!(result.is_none());

        // Content without section
        let result = check.check("# Plan\n\nNo summary here.");
        assert!(result.is_some());
        assert!(result.unwrap().message.contains("Summary"));
    }

    #[test]
    fn test_format_validator_for_plan_valid() {
        let content = r#"# Plan: Test

## Summary
A test plan.

## Goals
- Goal 1

## Non-Goals
- Non-goal 1

## Proposed Solution
The solution.

## Specs

### Spec 1: Core
Core spec.

## Risks
Risk handling.
"#;

        let result = validate_plan_format(content);
        assert!(result.passed());
    }

    #[test]
    fn test_format_validator_for_plan_missing_sections() {
        let content = "# Plan\n\n## Summary\nA plan.";

        let result = validate_plan_format(content);
        assert!(!result.passed());

        let failures = result.failures().unwrap();
        // Should fail for missing Goals, Non-Goals, Proposed Solution, Specs, Risks, and no spec defined
        assert!(failures.len() >= 5);
    }

    #[test]
    fn test_format_validator_for_plan_no_specs() {
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
(no specs yet)

## Risks
Risks.
"#;

        let result = validate_plan_format(content);
        assert!(!result.passed());

        let failures = result.failures().unwrap();
        assert!(failures.iter().any(|f| f.message.contains("No specs defined")));
    }

    #[test]
    fn test_format_validator_for_spec_valid() {
        let content = r#"# Spec: Core

## Overview
Overview of the spec.

## Requirements
- Req 1
- Req 2

## Acceptance Criteria
- [ ] Criterion 1
- [ ] Criterion 2

## Phases

### Phase 1: Setup
Setup phase.
"#;

        let result = validate_spec_format(content);
        assert!(result.passed());
    }

    #[test]
    fn test_format_validator_for_spec_missing_phases() {
        let content = r#"# Spec

## Overview
Overview.

## Requirements
- Req

## Acceptance Criteria
- Criterion
"#;

        let result = validate_spec_format(content);
        assert!(!result.passed());

        let failures = result.failures().unwrap();
        assert!(failures.iter().any(|f| f.message.contains("Phases")));
    }

    #[test]
    fn test_format_validator_for_phase_valid() {
        let content = r#"# Phase 1: Setup

## Goal
Set up the project structure.

## Tasks
- [ ] Create directory structure
- [ ] Add dependencies

## Acceptance Criteria
- Project compiles
- Tests pass
"#;

        let result = validate_phase_format(content);
        assert!(result.passed());
    }

    #[test]
    fn test_format_validator_ambiguous_markers() {
        let content = r#"# Plan

## Summary
A plan.

## Goals
- TBD

## Non-Goals
- None

## Proposed Solution
FIXME: need to figure this out

## Specs

### Spec 1: ???
To be determined.

## Risks
None.
"#;

        let result = validate_plan_format(content);
        assert!(!result.passed());

        let failures = result.failures().unwrap();
        // Should catch TBD, FIXME, ???
        let marker_failures: Vec<_> = failures.iter().filter(|f| f.message.contains("ambiguous")).collect();
        assert_eq!(marker_failures.len(), 3);
    }

    #[test]
    fn test_format_validator_for_loop_type() {
        // Just test that the factory method returns the right kind of validator
        let plan_validator = FormatValidator::for_loop_type(LoopType::Plan);
        assert!(!plan_validator.checks.is_empty());

        let ralph_validator = FormatValidator::for_loop_type(LoopType::Ralph);
        assert!(ralph_validator.checks.is_empty()); // Ralph doesn't have structure checks
    }

    #[test]
    fn test_format_validator_custom_check() {
        let validator = FormatValidator::new().add_custom_check(|content| {
            if content.contains("bad word") {
                Some(FailureDetail::new(FailureCategory::Structure, "Contains bad word"))
            } else {
                None
            }
        });

        let good_result = validator.validate("This is fine.");
        assert!(good_result.passed());

        let bad_result = validator.validate("This has a bad word in it.");
        assert!(!bad_result.passed());
    }

    #[test]
    fn test_format_validation_result_into_failures() {
        let pass = FormatValidationResult::Pass;
        assert!(pass.into_failures().is_empty());

        let failures = vec![FailureDetail::new(FailureCategory::Structure, "error")];
        let fail = FormatValidationResult::Fail(failures);
        let converted = fail.into_failures();
        assert_eq!(converted.len(), 1);
    }
}
