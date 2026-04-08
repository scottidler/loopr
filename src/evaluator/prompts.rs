//! Coverage evaluation prompt templates for each decomposition boundary.

/// Build a coverage evaluation prompt for Plan -> Specs.
///
/// `parent_markdown_content` is the Plan's full `.md` file.
/// `children_markdown_content` is the concatenated Spec `.md` files separated by `---`.
pub fn plan_specs_prompt(parent_markdown_content: &str, children_markdown_content: &str) -> String {
    crate::prompts::store()
        .coverage_plan_specs
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{children_markdown_content}", children_markdown_content)
        .replace("{schema}", &crate::prompts::store().coverage_schema)
}

/// Build a coverage evaluation prompt for Spec -> Phases.
///
/// `parent_markdown_content` is the Spec's full `.md` file.
/// `children_markdown_content` is the concatenated Phase `.md` files separated by `---`.
pub fn spec_phases_prompt(parent_markdown_content: &str, children_markdown_content: &str) -> String {
    crate::prompts::store()
        .coverage_spec_phases
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{children_markdown_content}", children_markdown_content)
        .replace("{schema}", &crate::prompts::store().coverage_schema)
}

/// Build a coverage evaluation prompt for Phase -> Works.
///
/// `parent_markdown_content` is the Phase's full `.md` file.
/// `children_markdown_content` is the concatenated Work `.md` files separated by `---`.
pub fn phase_works_prompt(parent_markdown_content: &str, children_markdown_content: &str) -> String {
    crate::prompts::store()
        .coverage_phase_works
        .replace("{parent_markdown_content}", parent_markdown_content)
        .replace("{children_markdown_content}", children_markdown_content)
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
    fn test_plan_specs_prompt_contains_content() {
        init();
        let parent = "---\ntitle: My Plan\nacceptance-criteria:\n  - Must pass tests\n---\n\nA description";
        let children = "---\ntitle: Spec 1\n---\n\nSpec body 1\n\n---\n\n---\ntitle: Spec 2\n---\n\nSpec body 2";
        let prompt = plan_specs_prompt(parent, children);
        assert!(prompt.contains("My Plan"));
        assert!(prompt.contains("A description"));
        assert!(prompt.contains("Must pass tests"));
        assert!(prompt.contains("Spec 1"));
        assert!(prompt.contains("Spec 2"));
    }

    #[test]
    fn test_plan_specs_prompt_contains_schema() {
        init();
        let prompt = plan_specs_prompt("parent", "children");
        assert!(prompt.contains("\"verdict\""));
        assert!(prompt.contains("\"gaps\""));
        assert!(prompt.contains("\"summary\""));
    }

    #[test]
    fn test_plan_specs_prompt_no_residual_placeholders() {
        init();
        let prompt = plan_specs_prompt("P", "C");
        assert!(!prompt.contains("{parent_markdown_content}"));
        assert!(!prompt.contains("{children_markdown_content}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_spec_phases_prompt_contains_content() {
        init();
        let parent = "---\ntitle: My Spec\n---\n\nSpec desc";
        let children = "---\ntitle: Phase 1\norder: 0\n---\n\nPhase body";
        let prompt = spec_phases_prompt(parent, children);
        assert!(prompt.contains("My Spec"));
        assert!(prompt.contains("Spec desc"));
        assert!(prompt.contains("Phase 1"));
    }

    #[test]
    fn test_spec_phases_prompt_no_residual_placeholders() {
        init();
        let prompt = spec_phases_prompt("P", "C");
        assert!(!prompt.contains("{parent_markdown_content}"));
        assert!(!prompt.contains("{children_markdown_content}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_phase_works_prompt_contains_content() {
        init();
        let parent = "---\ntitle: My Phase\norder: 2\n---\n\nPhase desc";
        let children = "---\ntitle: Work 1\n---\n\nWork body";
        let prompt = phase_works_prompt(parent, children);
        assert!(prompt.contains("My Phase"));
        assert!(prompt.contains("Phase desc"));
        assert!(prompt.contains("order: 2"));
        assert!(prompt.contains("Work 1"));
    }

    #[test]
    fn test_phase_works_prompt_no_residual_placeholders() {
        init();
        let prompt = phase_works_prompt("P", "C");
        assert!(!prompt.contains("{parent_markdown_content}"));
        assert!(!prompt.contains("{children_markdown_content}"));
        assert!(!prompt.contains("{schema}"));
    }

    #[test]
    fn test_all_coverage_prompts_request_json() {
        init();
        let p1 = plan_specs_prompt("P", "C");
        let p2 = spec_phases_prompt("P", "C");
        let p3 = phase_works_prompt("P", "C");
        for prompt in [&p1, &p2, &p3] {
            assert!(prompt.contains("JSON"), "Coverage prompt must instruct JSON output");
        }
    }
}
