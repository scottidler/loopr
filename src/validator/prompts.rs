//! Per-type validation prompt templates for the Doc Validator LLM.
//!
//! Each collection type (Plan, Spec, Phase) has specific evaluation criteria
//! and additional fields that the LLM uses to assess readiness for Draft -> Active.

/// Build a validation prompt for a Plan document.
///
/// `markdown_content` is the full `docs/loopr/<id>.md` file including frontmatter.
pub fn plan_prompt(markdown_content: &str) -> String {
    crate::prompts::store()
        .validator_plan
        .replace("{markdown_content}", markdown_content)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}

/// Build a validation prompt for a Spec document.
///
/// `markdown_content` is the spec's full `.md` file.
/// `parent_markdown_content` is the parent Plan's full `.md` file.
pub fn spec_prompt(markdown_content: &str, parent_markdown_content: &str) -> String {
    crate::prompts::store()
        .validator_spec
        .replace("{markdown_content}", markdown_content)
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}

/// Build a validation prompt for a Phase document.
///
/// `markdown_content` is the phase's full `.md` file.
/// `parent_markdown_content` is the parent Spec's full `.md` file.
pub fn phase_prompt(markdown_content: &str, parent_markdown_content: &str) -> String {
    crate::prompts::store()
        .validator_phase
        .replace("{markdown_content}", markdown_content)
        .replace("{parent_markdown_content}", parent_markdown_content)
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
    fn test_plan_prompt_contains_content() {
        init();
        let md = "---\ntitle: My Plan\n---\n\nA description\n\n## Acceptance Criteria\n\n- Must pass tests";
        let prompt = plan_prompt(md);
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("A description"));
        assert!(prompt.contains("Must pass tests"));
    }

    #[test]
    fn test_plan_prompt_contains_criteria() {
        init();
        let md = "---\ntitle: T\n---\n\nBody";
        let prompt = plan_prompt(md);
        assert!(prompt.contains("Clear objective stated"));
        assert!(prompt.contains("Measurable acceptance criteria"));
        assert!(prompt.contains("Scope is bounded"));
    }

    #[test]
    fn test_plan_prompt_contains_schema() {
        init();
        let md = "---\ntitle: T\n---\n\nBody";
        let prompt = plan_prompt(md);
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"issues\""));
        assert!(prompt.contains("\"summary\""));
    }

    #[test]
    fn test_spec_prompt_contains_fields() {
        init();
        let md = "---\ntitle: My Spec\nparent-id: pl-123\n---\n\nSpec desc";
        let parent_md = "---\ntitle: Parent Plan\n---\n\nPlan body";
        let prompt = spec_prompt(md, parent_md);
        assert!(prompt.contains("My Spec"));
        assert!(prompt.contains("Spec desc"));
        assert!(prompt.contains("Parent Plan"));
    }

    #[test]
    fn test_spec_prompt_contains_criteria() {
        init();
        let md = "---\ntitle: T\n---\n\nBody";
        let parent_md = "---\ntitle: P\n---\n\nParent";
        let prompt = spec_prompt(md, parent_md);
        assert!(prompt.contains("Plan alignment"));
        assert!(prompt.contains("Data Flow completeness"));
        assert!(prompt.contains("Module Structure substantiveness"));
        assert!(prompt.contains("Interface elaboration"));
        assert!(prompt.contains("Failure Modes coverage"));
        assert!(prompt.contains("Test Inventory traceability"));
        assert!(prompt.contains("Optional sections"));
    }

    #[test]
    fn test_phase_prompt_contains_fields() {
        init();
        let md = "---\ntitle: My Phase\norder: 3\nparent-id: sp-123\n---\n\nPhase desc";
        let parent_md = "---\ntitle: Parent Spec\n---\n\nSpec body";
        let prompt = phase_prompt(md, parent_md);
        assert!(prompt.contains("My Phase"));
        assert!(prompt.contains("Phase desc"));
        assert!(prompt.contains("order: 3"));
        assert!(prompt.contains("Parent Spec"));
    }

    #[test]
    fn test_phase_prompt_contains_criteria() {
        init();
        let md = "---\ntitle: T\norder: 1\n---\n\nBody";
        let parent_md = "---\ntitle: S\n---\n\nParent";
        let prompt = phase_prompt(md, parent_md);
        assert!(prompt.contains("Deliverables are concrete"));
        assert!(prompt.contains("Dependencies identified"));
        assert!(prompt.contains("Ordered correctly"));
    }

    #[test]
    fn test_all_prompts_request_json_only() {
        init();
        let md = "---\ntitle: T\n---\n\nBody";
        let parent_md = "---\ntitle: P\n---\n\nParent";
        let plan = plan_prompt(md);
        let spec = spec_prompt(md, parent_md);
        let phase = phase_prompt(md, parent_md);
        for prompt in [&plan, &spec, &phase] {
            assert!(prompt.contains("Respond with ONLY valid JSON"));
        }
    }
}
