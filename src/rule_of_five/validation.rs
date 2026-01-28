//! Pass-specific validation for Rule of Five.
//!
//! Each pass has specific validation criteria beyond the generic LLM response
//! parsing. This module provides validators for each pass.

#![allow(dead_code)]

use super::passes::ReviewPass;

/// Result of validating a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PassValidationResult {
    /// Pass succeeded - can advance to next pass.
    Passed,
    /// Pass needs more work - LLM feedback describes issues.
    NeedsWork(Vec<String>),
    /// Pass failed with an error.
    Error(String),
}

impl PassValidationResult {
    /// Check if this result allows advancing to the next pass.
    pub fn can_advance(&self) -> bool {
        matches!(self, Self::Passed)
    }

    /// Get the issues if this is NeedsWork.
    pub fn issues(&self) -> Option<&[String]> {
        match self {
            Self::NeedsWork(issues) => Some(issues),
            _ => None,
        }
    }
}

/// Validator for a specific review pass.
pub struct PassValidator {
    pass: ReviewPass,
}

impl PassValidator {
    /// Create a new validator for the given pass.
    pub fn new(pass: ReviewPass) -> Self {
        Self { pass }
    }

    /// Get the pass this validator is for.
    pub fn pass(&self) -> ReviewPass {
        self.pass
    }

    /// Validate the LLM's response for this pass.
    ///
    /// Returns whether the pass succeeded and any issues found.
    pub fn validate_response(&self, response: &str) -> PassValidationResult {
        let response_trimmed = response.trim();

        // Check for explicit pass marker
        if response_trimmed.starts_with("PASS:") {
            return PassValidationResult::Passed;
        }

        // Check for explicit needs work marker
        if response_trimmed.starts_with("NEEDS_WORK:") {
            let issues = parse_issues_from_response(response_trimmed);
            return PassValidationResult::NeedsWork(issues);
        }

        // Try to infer from content
        if looks_like_passing_response(response_trimmed) {
            return PassValidationResult::Passed;
        }

        // Default to needs work and extract any issues mentioned
        let issues = parse_issues_from_response(response_trimmed);
        if issues.is_empty() {
            // If we can't parse issues, use the whole response as feedback
            PassValidationResult::NeedsWork(vec![response_trimmed.to_string()])
        } else {
            PassValidationResult::NeedsWork(issues)
        }
    }

    /// Validate plan content for pass-specific requirements.
    ///
    /// This checks the plan itself, not the LLM's review response.
    pub fn validate_plan_content(&self, plan_content: &str) -> PassValidationResult {
        match self.pass {
            ReviewPass::Completeness => validate_completeness(plan_content),
            ReviewPass::Correctness => validate_correctness(plan_content),
            ReviewPass::EdgeCases => validate_edge_cases(plan_content),
            ReviewPass::Architecture => validate_architecture(plan_content),
            ReviewPass::Clarity => validate_clarity(plan_content),
        }
    }
}

/// Validate pass-specific requirements for the given pass.
pub fn validate_pass(pass: ReviewPass, llm_response: &str, plan_content: &str) -> PassValidationResult {
    let validator = PassValidator::new(pass);

    // First check the LLM's response
    let response_result = validator.validate_response(llm_response);

    // If LLM says it passed, also do our own content validation
    if response_result.can_advance() {
        let content_result = validator.validate_plan_content(plan_content);
        if !content_result.can_advance() {
            return content_result;
        }
    }

    response_result
}

/// Check if a response looks like it's approving (even without explicit PASS:).
fn looks_like_passing_response(response: &str) -> bool {
    let response_lower = response.to_lowercase();

    // Positive indicators (these override negative if present)
    let strong_positive = response_lower.contains("all required sections present")
        || response_lower.contains("no logical errors")
        || response_lower.contains("edge cases adequately covered")
        || response_lower.contains("architecture fits well")
        || response_lower.contains("clear and implementable")
        || response_lower.contains("no issues found");

    // Weak positive indicators
    let weak_positive = response_lower.contains("looks good") || response_lower.contains("well done");

    // Negative indicators (but "no X errors" patterns are excluded)
    let has_negative = response_lower.contains("missing")
        || response_lower.contains("needs work")
        || (response_lower.contains("issue") && !response_lower.contains("no issue"))
        || response_lower.contains("problem")
        || (response_lower.contains(" error")
            && !response_lower.contains("no error")
            && !response_lower.contains("no logical error"))
        || response_lower.contains("incorrect")
        || response_lower.contains("unclear")
        || response_lower.contains("ambiguous");

    // Strong positives always pass, weak positives only pass if no negatives
    strong_positive || (weak_positive && !has_negative)
}

/// Parse issues from a response (numbered list or bullet points).
fn parse_issues_from_response(response: &str) -> Vec<String> {
    let mut issues = Vec::new();

    // Skip the NEEDS_WORK: prefix if present
    let content = if let Some(stripped) = response.strip_prefix("NEEDS_WORK:") {
        stripped.trim()
    } else {
        response
    };

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines
        if trimmed.is_empty() {
            continue;
        }

        // Check for numbered list (1. 2. etc.)
        if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit())
            && let Some(issue) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
            && !issue.is_empty()
        {
            issues.push(issue.trim().to_string());
            continue;
        }

        // Check for bullet points
        if let Some(issue) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* "))
            && !issue.is_empty()
        {
            issues.push(issue.trim().to_string());
        }
    }

    issues
}

// Pass-specific validation functions

fn validate_completeness(plan_content: &str) -> PassValidationResult {
    let mut missing = Vec::new();

    // Check for required sections
    let required_sections = [
        ("Summary", "## Summary"),
        ("Goals", "## Goals"),
        ("Non-Goals", "## Non-Goals"),
        ("Proposed Solution", "## Proposed Solution"),
        ("Specs", "## Specs"),
        ("Risks", "## Risks"),
    ];

    for (name, marker) in required_sections {
        if !plan_content.contains(marker) {
            missing.push(format!("Missing section: {}", name));
        }
    }

    // Check for at least one spec defined
    if !plan_content.contains("### Spec") {
        missing.push("No specs defined (expected ### Spec N: <name>)".to_string());
    }

    if missing.is_empty() {
        PassValidationResult::Passed
    } else {
        PassValidationResult::NeedsWork(missing)
    }
}

fn validate_correctness(_plan_content: &str) -> PassValidationResult {
    // Correctness is mainly validated by the LLM review.
    // We could add heuristic checks here in the future.
    PassValidationResult::Passed
}

fn validate_edge_cases(plan_content: &str) -> PassValidationResult {
    // Check that risks section mentions error handling
    if plan_content.contains("## Risks") {
        // Basic check - risks section exists
        return PassValidationResult::Passed;
    }

    PassValidationResult::NeedsWork(vec!["Risks section should address error handling".to_string()])
}

fn validate_architecture(_plan_content: &str) -> PassValidationResult {
    // Architecture is mainly validated by the LLM review.
    PassValidationResult::Passed
}

fn validate_clarity(plan_content: &str) -> PassValidationResult {
    let mut issues = Vec::new();

    // Check for ambiguous language
    let ambiguous_terms = ["TBD", "TODO", "FIXME", "???"];
    for term in ambiguous_terms {
        if plan_content.contains(term) {
            issues.push(format!("Contains ambiguous marker: {}", term));
        }
    }

    if issues.is_empty() {
        PassValidationResult::Passed
    } else {
        PassValidationResult::NeedsWork(issues)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_result_can_advance() {
        assert!(PassValidationResult::Passed.can_advance());
        assert!(!PassValidationResult::NeedsWork(vec!["issue".to_string()]).can_advance());
        assert!(!PassValidationResult::Error("error".to_string()).can_advance());
    }

    #[test]
    fn test_validation_result_issues() {
        let issues = vec!["issue1".to_string(), "issue2".to_string()];
        let result = PassValidationResult::NeedsWork(issues.clone());
        assert_eq!(result.issues(), Some(issues.as_slice()));

        assert_eq!(PassValidationResult::Passed.issues(), None);
        assert_eq!(PassValidationResult::Error("err".to_string()).issues(), None);
    }

    #[test]
    fn test_validate_response_explicit_pass() {
        let validator = PassValidator::new(ReviewPass::Completeness);

        let response = "PASS: All required sections present and filled.";
        let result = validator.validate_response(response);
        assert!(result.can_advance());
    }

    #[test]
    fn test_validate_response_explicit_needs_work() {
        let validator = PassValidator::new(ReviewPass::Completeness);

        let response = "NEEDS_WORK:\n1. Missing Summary section\n2. No goals defined";
        let result = validator.validate_response(response);

        assert!(!result.can_advance());
        let issues = result.issues().unwrap();
        assert_eq!(issues.len(), 2);
        assert!(issues[0].contains("Summary"));
        assert!(issues[1].contains("goals"));
    }

    #[test]
    fn test_validate_response_inferred_pass() {
        let validator = PassValidator::new(ReviewPass::Completeness);

        let response = "All required sections present and the plan looks good.";
        let result = validator.validate_response(response);
        assert!(result.can_advance());
    }

    #[test]
    fn test_validate_response_inferred_needs_work() {
        let validator = PassValidator::new(ReviewPass::Completeness);

        let response = "The plan is missing the Goals section and has some issues.";
        let result = validator.validate_response(response);
        assert!(!result.can_advance());
    }

    #[test]
    fn test_parse_issues_numbered_list() {
        let response = "NEEDS_WORK:\n1. First issue\n2. Second issue\n3. Third issue";
        let issues = parse_issues_from_response(response);

        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0], "First issue");
        assert_eq!(issues[1], "Second issue");
        assert_eq!(issues[2], "Third issue");
    }

    #[test]
    fn test_parse_issues_bullet_list() {
        let response = "NEEDS_WORK:\n- First issue\n- Second issue\n* Third issue";
        let issues = parse_issues_from_response(response);

        assert_eq!(issues.len(), 3);
        assert_eq!(issues[0], "First issue");
        assert_eq!(issues[1], "Second issue");
        assert_eq!(issues[2], "Third issue");
    }

    #[test]
    fn test_validate_completeness_all_sections() {
        let plan = r#"# Plan: Test

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

        let result = validate_completeness(plan);
        assert!(result.can_advance());
    }

    #[test]
    fn test_validate_completeness_missing_sections() {
        let plan = "# Plan: Test\n\n## Summary\nA test plan.";

        let result = validate_completeness(plan);
        assert!(!result.can_advance());

        let issues = result.issues().unwrap();
        assert!(issues.iter().any(|i| i.contains("Goals")));
        assert!(issues.iter().any(|i| i.contains("Specs")));
    }

    #[test]
    fn test_validate_clarity_clean() {
        let plan = "# Plan: Test\n\nA clear plan with no ambiguity.";

        let result = validate_clarity(plan);
        assert!(result.can_advance());
    }

    #[test]
    fn test_validate_clarity_with_todos() {
        let plan = "# Plan: Test\n\n## TODO: Fill this in later";

        let result = validate_clarity(plan);
        assert!(!result.can_advance());

        let issues = result.issues().unwrap();
        assert!(issues[0].contains("TODO"));
    }

    #[test]
    fn test_validate_pass_full() {
        let llm_response = "PASS: All required sections present and filled.";
        let plan_content = r#"# Plan: Test

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

        let result = validate_pass(ReviewPass::Completeness, llm_response, plan_content);
        assert!(result.can_advance());
    }

    #[test]
    fn test_validate_pass_llm_passes_but_content_fails() {
        // LLM says it passed but content validation catches an issue
        let llm_response = "PASS: Looks good.";
        let plan_content = "# Plan: Test\n\nTODO: Write the actual plan";

        let result = validate_pass(ReviewPass::Clarity, llm_response, plan_content);
        assert!(!result.can_advance()); // Should fail because of TODO
    }

    #[test]
    fn test_looks_like_passing_response() {
        assert!(looks_like_passing_response("All required sections present"));
        assert!(looks_like_passing_response("No logical errors found"));
        assert!(looks_like_passing_response("Edge cases adequately covered"));
        assert!(looks_like_passing_response("Architecture fits well with the system"));
        assert!(looks_like_passing_response("Clear and implementable plan"));

        assert!(!looks_like_passing_response("Missing the Summary section"));
        assert!(!looks_like_passing_response("There are some issues to address"));
        assert!(!looks_like_passing_response("Problem found in the logic"));
    }
}
