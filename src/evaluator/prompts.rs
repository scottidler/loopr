//! Coverage evaluation prompt templates for each decomposition boundary.

/// Build a coverage evaluation prompt for Plan -> Specs.
pub fn plan_specs_prompt(
    plan_title: &str,
    plan_content: &str,
    plan_acceptance_criteria: &str,
    specs_list: &str,
) -> String {
    crate::prompts::store()
        .coverage_plan_specs
        .replace("{plan_title}", plan_title)
        .replace("{plan_content}", plan_content)
        .replace("{plan_acceptance_criteria}", plan_acceptance_criteria)
        .replace("{specs_list}", specs_list)
        .replace("{schema}", &crate::prompts::store().coverage_schema)
}

/// Build a coverage evaluation prompt for Spec -> Phases.
pub fn spec_phases_prompt(spec_title: &str, spec_content: &str, plan_title: &str, phases_list: &str) -> String {
    crate::prompts::store()
        .coverage_spec_phases
        .replace("{spec_title}", spec_title)
        .replace("{spec_content}", spec_content)
        .replace("{plan_title}", plan_title)
        .replace("{phases_list}", phases_list)
        .replace("{schema}", &crate::prompts::store().coverage_schema)
}

/// Build a coverage evaluation prompt for Phase -> Works.
pub fn phase_works_prompt(
    phase_title: &str,
    phase_content: &str,
    phase_order: u32,
    spec_title: &str,
    works_list: &str,
) -> String {
    crate::prompts::store()
        .coverage_phase_works
        .replace("{phase_title}", phase_title)
        .replace("{phase_content}", phase_content)
        .replace("{phase_order}", &phase_order.to_string())
        .replace("{spec_title}", spec_title)
        .replace("{works_list}", works_list)
        .replace("{schema}", &crate::prompts::store().coverage_schema)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        crate::prompts::init_defaults();
    }

    #[test]
    fn test_plan_specs_prompt_contains_fields() {
        init();
        let prompt = plan_specs_prompt("My Plan", "A description", "Must pass tests", "- Spec 1\n- Spec 2");
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("A description"));
        assert!(prompt.contains("Must pass tests"));
        assert!(prompt.contains("- Spec 1"));
    }

    #[test]
    fn test_plan_specs_prompt_contains_schema() {
        init();
        let prompt = plan_specs_prompt("T", "D", "C", "specs");
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"gaps\""));
        assert!(prompt.contains("\"summary\""));
    }

    #[test]
    fn test_plan_specs_prompt_no_residual_placeholders() {
        init();
        let prompt = plan_specs_prompt("T", "D", "C", "S");
        assert!(!prompt.contains("{plan_title}"));
        assert!(!prompt.contains("{plan_description}"));
        assert!(!prompt.contains("{plan_acceptance_criteria}"));
        assert!(!prompt.contains("{specs_list}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_spec_phases_prompt_contains_fields() {
        init();
        let prompt = spec_phases_prompt("My Spec", "Spec desc", "Parent Plan", "- Phase 1");
        assert!(prompt.contains("My Spec"));
        assert!(prompt.contains("Spec desc"));
        assert!(prompt.contains("Parent Plan"));
        assert!(prompt.contains("- Phase 1"));
    }

    #[test]
    fn test_spec_phases_prompt_no_residual_placeholders() {
        init();
        let prompt = spec_phases_prompt("T", "D", "P", "phases");
        assert!(!prompt.contains("{spec_title}"));
        assert!(!prompt.contains("{spec_description}"));
        assert!(!prompt.contains("{plan_title}"));
        assert!(!prompt.contains("{phases_list}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_phase_works_prompt_contains_fields() {
        init();
        let prompt = phase_works_prompt("My Phase", "Phase desc", 2, "Parent Spec", "- Work 1");
        assert!(prompt.contains("My Phase"));
        assert!(prompt.contains("Phase desc"));
        assert!(prompt.contains("Order: 2"));
        assert!(prompt.contains("Parent Spec"));
        assert!(prompt.contains("- Work 1"));
    }

    #[test]
    fn test_phase_works_prompt_no_residual_placeholders() {
        init();
        let prompt = phase_works_prompt("T", "D", 1, "S", "works");
        assert!(!prompt.contains("{phase_title}"));
        assert!(!prompt.contains("{phase_description}"));
        assert!(!prompt.contains("{phase_order}"));
        assert!(!prompt.contains("{spec_title}"));
        assert!(!prompt.contains("{works_list}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_all_coverage_prompts_request_json() {
        init();
        let p1 = plan_specs_prompt("T", "D", "C", "S");
        let p2 = spec_phases_prompt("T", "D", "P", "Ph");
        let p3 = phase_works_prompt("T", "D", 1, "S", "W");
        for prompt in [&p1, &p2, &p3] {
            assert!(prompt.contains("JSON"), "Coverage prompt must instruct JSON output");
        }
    }
}
