//! Per-type validation prompt templates for the Doc Validator LLM.
//!
//! Each collection type (Plan, Spec, Phase) has specific evaluation criteria
//! and additional fields that the LLM uses to assess readiness for Draft → Active.

/// Build a validation prompt for a Plan document.
pub fn plan_prompt(title: &str, content: &str, acceptance_criteria: &str) -> String {
    crate::prompts::store()
        .validator_plan
        .replace("{title}", title)
        .replace("{content}", content)
        .replace("{acceptance_criteria}", acceptance_criteria)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}

/// Build a validation prompt for a Spec document.
pub fn spec_prompt(title: &str, content: &str, plan_title: &str) -> String {
    crate::prompts::store()
        .validator_spec
        .replace("{title}", title)
        .replace("{content}", content)
        .replace("{plan_title}", plan_title)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}

/// Build a validation prompt for a Phase document.
pub fn phase_prompt(title: &str, content: &str, order: u32, spec_title: &str) -> String {
    crate::prompts::store()
        .validator_phase
        .replace("{title}", title)
        .replace("{content}", content)
        .replace("{order}", &order.to_string())
        .replace("{spec_title}", spec_title)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        crate::prompts::init_defaults();
    }

    #[test]
    fn test_plan_prompt_contains_title() {
        init();
        let prompt = plan_prompt("My Plan", "A description", "Must pass tests");
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("A description"));
        assert!(prompt.contains("Must pass tests"));
    }

    #[test]
    fn test_plan_prompt_contains_criteria() {
        init();
        let prompt = plan_prompt("Title", "Desc", "Criteria");
        assert!(prompt.contains("Clear objective stated"));
        assert!(prompt.contains("Measurable acceptance criteria"));
        assert!(prompt.contains("Scope is bounded"));
    }

    #[test]
    fn test_plan_prompt_contains_schema() {
        init();
        let prompt = plan_prompt("T", "D", "C");
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"issues\""));
        assert!(prompt.contains("\"summary\""));
    }

    #[test]
    fn test_spec_prompt_contains_fields() {
        init();
        let prompt = spec_prompt("My Spec", "Spec desc", "Parent Plan");
        assert!(prompt.contains("My Spec"));
        assert!(prompt.contains("Spec desc"));
        assert!(prompt.contains("Parent Plan"));
    }

    #[test]
    fn test_spec_prompt_contains_criteria() {
        init();
        let prompt = spec_prompt("T", "D", "P");
        assert!(prompt.contains("Technical approach described"));
        assert!(prompt.contains("Key decisions documented"));
        assert!(prompt.contains("Testability addressed"));
    }

    #[test]
    fn test_phase_prompt_contains_fields() {
        init();
        let prompt = phase_prompt("My Phase", "Phase desc", 3, "Parent Spec");
        assert!(prompt.contains("My Phase"));
        assert!(prompt.contains("Phase desc"));
        assert!(prompt.contains("Order: 3"));
        assert!(prompt.contains("Parent Spec"));
    }

    #[test]
    fn test_phase_prompt_contains_criteria() {
        init();
        let prompt = phase_prompt("T", "D", 1, "S");
        assert!(prompt.contains("Deliverables are concrete"));
        assert!(prompt.contains("Dependencies identified"));
        assert!(prompt.contains("Ordered correctly"));
    }

    #[test]
    fn test_all_prompts_request_json_only() {
        init();
        let plan = plan_prompt("T", "D", "C");
        let spec = spec_prompt("T", "D", "P");
        let phase = phase_prompt("T", "D", 1, "S");
        for prompt in [&plan, &spec, &phase] {
            assert!(prompt.contains("Respond with ONLY valid JSON"));
        }
    }
}
