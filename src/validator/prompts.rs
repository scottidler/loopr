//! Per-type validation prompt templates for the Doc Validator LLM.
//!
//! Each collection type (Plan, Spec, Phase) has specific evaluation criteria
//! and additional fields that the LLM uses to assess readiness for Draft → Active.

/// JSON schema fragment included in all prompts to guide LLM response format.
const RESPONSE_SCHEMA: &str = r#"{
  "verdict": "pass | fail | warn",
  "issues": [
    {
      "severity": "error | warning | info",
      "category": "completeness | clarity | testability | scope",
      "message": "description of the issue",
      "suggestion": "optional suggestion for fixing"
    }
  ],
  "summary": "human-readable summary of the assessment"
}"#;

/// Build a validation prompt for a Plan document.
pub fn plan_prompt(title: &str, description: &str, acceptance_criteria: &str) -> String {
    format!(
        r#"You are a technical document validator for a software development orchestrator.
You are reviewing a Plan document. Your job is to assess whether this document is complete and clear enough to move from Draft to Active status.

## Document Under Review

Title: {title}
Description:
{description}

Acceptance Criteria:
{acceptance_criteria}

## Evaluation Criteria

1. Clear objective stated — the plan should describe what it aims to achieve.
2. Measurable acceptance criteria defined — there should be concrete criteria to determine when the plan is complete.
3. Scope is bounded (not open-ended) — the plan should have a clear boundary of what is and isn't included.

## Output Format

Respond with ONLY valid JSON matching this schema:
{schema}"#,
        title = title,
        description = description,
        acceptance_criteria = acceptance_criteria,
        schema = RESPONSE_SCHEMA,
    )
}

/// Build a validation prompt for a Spec document.
pub fn spec_prompt(title: &str, description: &str, plan_title: &str) -> String {
    format!(
        r#"You are a technical document validator for a software development orchestrator.
You are reviewing a Spec document. Your job is to assess whether this document is complete and clear enough to move from Draft to Active status.

## Document Under Review

Title: {title}
Description:
{description}

Parent Plan: {plan_title}

## Evaluation Criteria

1. References a valid Plan — the spec should clearly relate to the parent plan's objective.
2. Technical approach described — the spec should outline how the objective will be achieved.
3. Key decisions documented — important design choices should be recorded with rationale.
4. Testability addressed — the spec should describe how the implementation will be verified.

## Output Format

Respond with ONLY valid JSON matching this schema:
{schema}"#,
        title = title,
        description = description,
        plan_title = plan_title,
        schema = RESPONSE_SCHEMA,
    )
}

/// Build a validation prompt for a Phase document.
pub fn phase_prompt(title: &str, description: &str, order: u32, spec_title: &str) -> String {
    format!(
        r#"You are a technical document validator for a software development orchestrator.
You are reviewing a Phase document. Your job is to assess whether this document is complete and clear enough to move from Draft to Active status.

## Document Under Review

Title: {title}
Description:
{description}

Order: {order}
Parent Spec: {spec_title}

## Evaluation Criteria

1. References a valid Spec — the phase should clearly relate to the parent spec.
2. Ordered correctly within the Spec — the phase order should make logical sense.
3. Deliverables are concrete — what the phase produces should be clearly defined.
4. Dependencies identified — any prerequisites or blockers should be listed.

## Output Format

Respond with ONLY valid JSON matching this schema:
{schema}"#,
        title = title,
        description = description,
        order = order,
        spec_title = spec_title,
        schema = RESPONSE_SCHEMA,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_prompt_contains_title() {
        let prompt = plan_prompt("My Plan", "A description", "Must pass tests");
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("A description"));
        assert!(prompt.contains("Must pass tests"));
    }

    #[test]
    fn test_plan_prompt_contains_criteria() {
        let prompt = plan_prompt("Title", "Desc", "Criteria");
        assert!(prompt.contains("Clear objective stated"));
        assert!(prompt.contains("Measurable acceptance criteria"));
        assert!(prompt.contains("Scope is bounded"));
    }

    #[test]
    fn test_plan_prompt_contains_schema() {
        let prompt = plan_prompt("T", "D", "C");
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"issues\""));
        assert!(prompt.contains("\"summary\""));
    }

    #[test]
    fn test_spec_prompt_contains_fields() {
        let prompt = spec_prompt("My Spec", "Spec desc", "Parent Plan");
        assert!(prompt.contains("My Spec"));
        assert!(prompt.contains("Spec desc"));
        assert!(prompt.contains("Parent Plan"));
    }

    #[test]
    fn test_spec_prompt_contains_criteria() {
        let prompt = spec_prompt("T", "D", "P");
        assert!(prompt.contains("Technical approach described"));
        assert!(prompt.contains("Key decisions documented"));
        assert!(prompt.contains("Testability addressed"));
    }

    #[test]
    fn test_phase_prompt_contains_fields() {
        let prompt = phase_prompt("My Phase", "Phase desc", 3, "Parent Spec");
        assert!(prompt.contains("My Phase"));
        assert!(prompt.contains("Phase desc"));
        assert!(prompt.contains("Order: 3"));
        assert!(prompt.contains("Parent Spec"));
    }

    #[test]
    fn test_phase_prompt_contains_criteria() {
        let prompt = phase_prompt("T", "D", 1, "S");
        assert!(prompt.contains("Deliverables are concrete"));
        assert!(prompt.contains("Dependencies identified"));
        assert!(prompt.contains("Ordered correctly"));
    }

    #[test]
    fn test_all_prompts_request_json_only() {
        let plan = plan_prompt("T", "D", "C");
        let spec = spec_prompt("T", "D", "P");
        let phase = phase_prompt("T", "D", 1, "S");
        for prompt in [&plan, &spec, &phase] {
            assert!(prompt.contains("Respond with ONLY valid JSON"));
        }
    }
}
