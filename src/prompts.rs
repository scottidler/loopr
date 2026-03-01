use std::fs;
use std::sync::OnceLock;

use log::info;

// Compile-time defaults — baked into the binary data segment, zero runtime cost
const DEFAULT_COORDINATOR: &str = include_str!("../prompts/coordinator.pmt");
const DEFAULT_IMPLEMENTER: &str = include_str!("../prompts/implementer.pmt");
const DEFAULT_REVIEWER: &str = include_str!("../prompts/reviewer.pmt");
const DEFAULT_RESEARCHER: &str = include_str!("../prompts/researcher.pmt");
const DEFAULT_VALIDATOR_SCHEMA: &str = include_str!("../prompts/validator-schema.pmt");
const DEFAULT_VALIDATOR_PLAN: &str = include_str!("../prompts/validator-plan.pmt");
const DEFAULT_VALIDATOR_SPEC: &str = include_str!("../prompts/validator-spec.pmt");
const DEFAULT_VALIDATOR_PHASE: &str = include_str!("../prompts/validator-phase.pmt");
const DEFAULT_GENERATION_PLAN: &str = include_str!("../prompts/generation-plan.pmt");
const DEFAULT_GENERATION_SPEC: &str = include_str!("../prompts/generation-spec.pmt");
const DEFAULT_GENERATION_PHASE: &str = include_str!("../prompts/generation-phase.pmt");
const DEFAULT_GENERATION_WORK: &str = include_str!("../prompts/generation-work.pmt");

pub struct PromptStore {
    pub coordinator: String,
    pub implementer: String,
    pub reviewer: String,
    pub researcher: String,
    pub validator_schema: String,
    pub validator_plan: String,
    pub validator_spec: String,
    pub validator_phase: String,
    pub generation_plan: String,
    pub generation_spec: String,
    pub generation_phase: String,
    pub generation_work: String,
}

static STORE: OnceLock<PromptStore> = OnceLock::new();

/// Initialize the global prompt store. Call once at startup after config load.
/// Checks ~/.config/loopr/prompts/ for overrides.
pub fn init() {
    let overrides_dir = dirs::config_dir().map(|d| d.join("loopr/prompts"));

    let load = |filename: &str, default: &str| -> String {
        if let Some(ref dir) = overrides_dir {
            let path = dir.join(filename);
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    info!("prompt override loaded: {}", path.display());
                    return content;
                }
                Ok(_) => {
                    // Empty file — fall back to default
                }
                Err(_) => {
                    // File not found — expected, use default
                }
            }
        }
        default.to_string()
    };

    // OnceLock::set returns Err if already initialized — harmless no-op
    let _ = STORE.set(PromptStore {
        coordinator: load("coordinator.pmt", DEFAULT_COORDINATOR),
        implementer: load("implementer.pmt", DEFAULT_IMPLEMENTER),
        reviewer: load("reviewer.pmt", DEFAULT_REVIEWER),
        researcher: load("researcher.pmt", DEFAULT_RESEARCHER),
        validator_schema: load("validator-schema.pmt", DEFAULT_VALIDATOR_SCHEMA),
        validator_plan: load("validator-plan.pmt", DEFAULT_VALIDATOR_PLAN),
        validator_spec: load("validator-spec.pmt", DEFAULT_VALIDATOR_SPEC),
        validator_phase: load("validator-phase.pmt", DEFAULT_VALIDATOR_PHASE),
        generation_plan: load("generation-plan.pmt", DEFAULT_GENERATION_PLAN),
        generation_spec: load("generation-spec.pmt", DEFAULT_GENERATION_SPEC),
        generation_phase: load("generation-phase.pmt", DEFAULT_GENERATION_PHASE),
        generation_work: load("generation-work.pmt", DEFAULT_GENERATION_WORK),
    });
}

/// Initialize with compiled-in defaults only (no filesystem). For tests.
pub fn init_defaults() {
    let _ = STORE.set(PromptStore {
        coordinator: DEFAULT_COORDINATOR.to_string(),
        implementer: DEFAULT_IMPLEMENTER.to_string(),
        reviewer: DEFAULT_REVIEWER.to_string(),
        researcher: DEFAULT_RESEARCHER.to_string(),
        validator_schema: DEFAULT_VALIDATOR_SCHEMA.to_string(),
        validator_plan: DEFAULT_VALIDATOR_PLAN.to_string(),
        validator_spec: DEFAULT_VALIDATOR_SPEC.to_string(),
        validator_phase: DEFAULT_VALIDATOR_PHASE.to_string(),
        generation_plan: DEFAULT_GENERATION_PLAN.to_string(),
        generation_spec: DEFAULT_GENERATION_SPEC.to_string(),
        generation_phase: DEFAULT_GENERATION_PHASE.to_string(),
        generation_work: DEFAULT_GENERATION_WORK.to_string(),
    });
}

/// Get the global prompt store. Panics if init() was not called.
pub fn store() -> &'static PromptStore {
    STORE
        .get()
        .expect("prompts::init() must be called before prompts::store()")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;
    use std::fs;

    #[test]
    fn test_defaults_non_empty() {
        init_defaults();
        let s = store();
        assert!(!s.coordinator.is_empty());
        assert!(!s.implementer.is_empty());
        assert!(!s.reviewer.is_empty());
        assert!(!s.researcher.is_empty());
        assert!(!s.validator_schema.is_empty());
        assert!(!s.validator_plan.is_empty());
        assert!(!s.validator_spec.is_empty());
        assert!(!s.validator_phase.is_empty());
        assert!(!s.generation_plan.is_empty());
        assert!(!s.generation_spec.is_empty());
        assert!(!s.generation_phase.is_empty());
        assert!(!s.generation_work.is_empty());
    }

    #[test]
    fn test_defaults_match_include_str() {
        init_defaults();
        let s = store();
        assert_eq!(s.coordinator, DEFAULT_COORDINATOR);
        assert_eq!(s.implementer, DEFAULT_IMPLEMENTER);
        assert_eq!(s.reviewer, DEFAULT_REVIEWER);
        assert_eq!(s.researcher, DEFAULT_RESEARCHER);
        assert_eq!(s.validator_schema, DEFAULT_VALIDATOR_SCHEMA);
        assert_eq!(s.validator_plan, DEFAULT_VALIDATOR_PLAN);
        assert_eq!(s.validator_spec, DEFAULT_VALIDATOR_SPEC);
        assert_eq!(s.validator_phase, DEFAULT_VALIDATOR_PHASE);
        assert_eq!(s.generation_plan, DEFAULT_GENERATION_PLAN);
        assert_eq!(s.generation_spec, DEFAULT_GENERATION_SPEC);
        assert_eq!(s.generation_phase, DEFAULT_GENERATION_PHASE);
        assert_eq!(s.generation_work, DEFAULT_GENERATION_WORK);
    }

    #[test]
    fn test_placeholder_assertions() {
        init_defaults();
        let s = store();
        // Researcher prompt must contain {query} placeholder
        assert!(s.researcher.contains("{query}"));
        // Validator templates must contain their placeholders
        assert!(s.validator_plan.contains("{title}"));
        assert!(s.validator_plan.contains("{description}"));
        assert!(s.validator_plan.contains("{acceptance_criteria}"));
        assert!(s.validator_plan.contains("{schema}"));
        assert!(s.validator_spec.contains("{title}"));
        assert!(s.validator_spec.contains("{plan_title}"));
        assert!(s.validator_spec.contains("{schema}"));
        assert!(s.validator_phase.contains("{title}"));
        assert!(s.validator_phase.contains("{order}"));
        assert!(s.validator_phase.contains("{spec_title}"));
        assert!(s.validator_phase.contains("{schema}"));
    }

    #[test]
    fn test_override_from_temp_dir() {
        // This test uses a fresh OnceLock via a subprocess-like pattern.
        // Since OnceLock can only be set once per process, we test the load closure directly.
        let dir = TestDir::new("loopr-pmt-override");
        fs::write(dir.join("coordinator.pmt"), "CUSTOM COORDINATOR PROMPT").unwrap();

        let overrides_dir = Some(dir.to_path_buf());
        let load = |filename: &str, default: &str| -> String {
            if let Some(ref dir) = overrides_dir {
                let path = dir.join(filename);
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        return content;
                    }
                    _ => {}
                }
            }
            default.to_string()
        };

        assert_eq!(load("coordinator.pmt", "default"), "CUSTOM COORDINATOR PROMPT");
        assert_eq!(load("implementer.pmt", "default"), "default");
    }

    #[test]
    fn test_empty_override_falls_back() {
        let dir = TestDir::new("loopr-pmt-empty");
        fs::write(dir.join("coordinator.pmt"), "   \n  ").unwrap();

        let overrides_dir = Some(dir.to_path_buf());
        let load = |filename: &str, default: &str| -> String {
            if let Some(ref dir) = overrides_dir {
                let path = dir.join(filename);
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        return content;
                    }
                    _ => {}
                }
            }
            default.to_string()
        };

        assert_eq!(load("coordinator.pmt", "default value"), "default value");
    }

    // =========================================================================
    // Agent prompt content identity tests
    //
    // These verify the .pmt files contain the same key content that was
    // previously hardcoded in each agent's const SYSTEM_PROMPT.
    // =========================================================================

    #[test]
    fn test_coordinator_pmt_identity() {
        init_defaults();
        let p = &store().coordinator;
        // Opening identity line
        assert!(p.starts_with("You are the Coordinator agent in the Loopr development orchestrator."));
        // All 15 action types
        for action in [
            "create_plan",
            "create_spec",
            "create_phase",
            "create_work",
            "assign_agent",
            "spawn_researcher",
            "acquire_lock",
            "release_lock",
            "validate_document",
            "triage_bundle",
            "accept_bundle",
            "transition",
            "create_learning",
            "need_help",
            "done",
        ] {
            assert!(p.contains(action), "coordinator.pmt missing action: {}", action);
        }
        // Key rules
        assert!(p.contains("Create ALL Works for a Phase in a single batch"));
        assert!(p.contains("ALWAYS respond with ONLY a JSON array"));
    }

    #[test]
    fn test_implementer_pmt_identity() {
        init_defaults();
        let p = &store().implementer;
        assert!(p.starts_with("You are an Implementer agent in the Loopr development orchestrator."));
        for action in [
            "write_file",
            "read_file",
            "run_tool",
            "commit",
            "propose_bundle",
            "create_learning",
            "done",
            "need_help",
        ] {
            assert!(p.contains(action), "implementer.pmt missing action: {}", action);
        }
        assert!(p.contains("iteration budget"));
        assert!(p.contains("clippy"));
    }

    #[test]
    fn test_reviewer_pmt_identity() {
        init_defaults();
        let p = &store().reviewer;
        assert!(p.starts_with("You are a Reviewer agent in the Loopr development orchestrator."));
        for criterion in ["Correctness", "Quality", "Tests", "Scope", "Safety"] {
            assert!(p.contains(criterion), "reviewer.pmt missing criterion: {}", criterion);
        }
        assert!(p.contains("approve"));
        assert!(p.contains("request_changes"));
        assert!(p.contains("reject"));
    }

    #[test]
    fn test_researcher_pmt_identity() {
        init_defaults();
        let p = &store().researcher;
        assert!(p.starts_with("You are a Researcher agent in the Loopr development orchestrator."));
        for action in [
            "search_code",
            "search_files",
            "read_file",
            "list_directory",
            "create_learning",
            "done",
            "need_help",
        ] {
            assert!(p.contains(action), "researcher.pmt missing action: {}", action);
        }
        assert!(p.contains("read-only"));
        assert!(
            p.contains("{query}"),
            "researcher.pmt must retain {{query}} placeholder"
        );
    }

    // =========================================================================
    // Validator output equivalence tests
    //
    // The highest-risk migration: format!() → .replace(). These verify the
    // assembled output matches what the old format!() code produced.
    // =========================================================================

    #[test]
    fn test_validator_plan_output_equivalence() {
        init_defaults();
        let title = "Implement Auth";
        let desc = "Add JWT-based authentication to the API";
        let criteria = "All endpoints require valid token; tests pass";

        let output = crate::validator::prompts::plan_prompt(title, desc, criteria);

        // Verify structure matches old format! output
        assert!(output.contains(&format!("Title: {}", title)));
        assert!(output.contains(desc));
        assert!(output.contains(&format!("Acceptance Criteria:\n{}", criteria)));
        // Schema was inlined by the old RESPONSE_SCHEMA const
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        assert!(output.contains("\"severity\": \"error | warning | info\""));
        // No residual placeholders
        assert!(!output.contains("{title}"));
        assert!(!output.contains("{description}"));
        assert!(!output.contains("{acceptance_criteria}"));
        assert!(!output.contains("{schema}"));
    }

    #[test]
    fn test_validator_spec_output_equivalence() {
        init_defaults();
        let title = "JWT Auth Spec";
        let desc = "Use RS256 tokens with 15-minute expiry";
        let plan_title = "Implement Auth";

        let output = crate::validator::prompts::spec_prompt(title, desc, plan_title);

        assert!(output.contains(&format!("Title: {}", title)));
        assert!(output.contains(desc));
        assert!(output.contains(&format!("Parent Plan: {}", plan_title)));
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        // No residual placeholders
        assert!(!output.contains("{title}"));
        assert!(!output.contains("{description}"));
        assert!(!output.contains("{plan_title}"));
        assert!(!output.contains("{schema}"));
    }

    #[test]
    fn test_validator_phase_output_equivalence() {
        init_defaults();
        let title = "Token Validation";
        let desc = "Implement middleware for JWT validation";
        let order: u32 = 2;
        let spec_title = "JWT Auth Spec";

        let output = crate::validator::prompts::phase_prompt(title, desc, order, spec_title);

        assert!(output.contains(&format!("Title: {}", title)));
        assert!(output.contains(desc));
        assert!(output.contains(&format!("Order: {}", order)));
        assert!(output.contains(&format!("Parent Spec: {}", spec_title)));
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        // No residual placeholders
        assert!(!output.contains("{title}"));
        assert!(!output.contains("{description}"));
        assert!(!output.contains("{order}"));
        assert!(!output.contains("{spec_title}"));
        assert!(!output.contains("{schema}"));
    }

    #[test]
    fn test_validator_schema_pmt_is_valid_json_structure() {
        init_defaults();
        let schema = &store().validator_schema;
        // The schema template itself isn't valid JSON (contains "pass | fail"),
        // but it must contain the structural keys
        assert!(schema.contains("\"verdict\""));
        assert!(schema.contains("\"issues\""));
        assert!(schema.contains("\"severity\""));
        assert!(schema.contains("\"category\""));
        assert!(schema.contains("\"message\""));
        assert!(schema.contains("\"summary\""));
    }

    // =========================================================================
    // Researcher .replace("{query}") correctness
    // =========================================================================

    #[test]
    fn test_researcher_query_replacement_no_residual() {
        init_defaults();
        let prompt = store()
            .researcher
            .replace("{query}", "Find all error handling patterns");
        assert!(prompt.contains("Find all error handling patterns"));
        assert!(!prompt.contains("{query}"));
    }

    #[test]
    fn test_researcher_query_with_special_chars() {
        init_defaults();
        let query = r#"Find patterns matching fn\s+handle_ in {src/}"#;
        let prompt = store().researcher.replace("{query}", query);
        assert!(prompt.contains(query));
        assert!(!prompt.contains("{query}"));
    }

    // =========================================================================
    // Generation prompt content tests
    //
    // Verify the .pmt instruction text appears correctly in assembled prompts.
    // =========================================================================

    #[test]
    fn test_generation_plan_pmt_content() {
        init_defaults();
        let p = &store().generation_plan;
        assert!(p.contains("Create a Plan with:"));
        assert!(p.contains("bounded title"));
        assert!(p.contains("acceptance criteria"));
        assert!(p.contains("create_plan"));
    }

    #[test]
    fn test_generation_spec_pmt_content() {
        init_defaults();
        let p = &store().generation_spec;
        assert!(p.contains("Create a Spec for this Plan with:"));
        assert!(p.contains("technical approach"));
        assert!(p.contains("create_spec"));
        assert!(p.contains("plan_id"));
    }

    #[test]
    fn test_generation_phase_pmt_content() {
        init_defaults();
        let p = &store().generation_phase;
        assert!(p.contains("Create ordered implementation Phases"));
        assert!(p.contains("deliverables"));
        assert!(p.contains("create_phase"));
        assert!(p.contains("spec_id"));
    }

    #[test]
    fn test_generation_work_pmt_content() {
        init_defaults();
        let p = &store().generation_work;
        assert!(p.contains("Create Works for this Phase"));
        assert!(p.contains("resource_tags"));
        assert!(p.contains("create_work"));
        assert!(p.contains("phase_id"));
    }

    // =========================================================================
    // Generation prompt integration — verify .pmt content lands in assembled msg
    // =========================================================================

    #[test]
    fn test_generation_plan_prompt_contains_pmt_instructions() {
        init_defaults();
        let prompt = crate::agents::generation::build_plan_prompt("Test goal", &[], &[], None);
        let pmt = &store().generation_plan;
        // The .pmt content should appear in the assembled user_message
        assert!(
            prompt.user_message.contains(pmt.trim()),
            "Plan generation prompt missing .pmt instruction content"
        );
    }

    #[test]
    fn test_generation_spec_prompt_contains_pmt_instructions() {
        init_defaults();
        let plan = crate::domain::plan::Plan::new("P".into(), "d".into(), "c".into());
        let prompt = crate::agents::generation::build_spec_prompt(&plan, &[], &[], &[], None);
        let pmt = &store().generation_spec;
        assert!(
            prompt.user_message.contains(pmt.trim()),
            "Spec generation prompt missing .pmt instruction content"
        );
    }

    #[test]
    fn test_generation_phase_prompt_contains_pmt_instructions() {
        init_defaults();
        let spec = crate::domain::spec::Spec::new("p1".into(), "S".into(), "d".into());
        let prompt = crate::agents::generation::build_phase_prompt(&spec, &[], &[], None);
        let pmt = &store().generation_phase;
        assert!(
            prompt.user_message.contains(pmt.trim()),
            "Phase generation prompt missing .pmt instruction content"
        );
    }

    #[test]
    fn test_generation_work_prompt_contains_pmt_instructions() {
        init_defaults();
        let phase = crate::domain::phase::Phase::new("s1".into(), "Ph".into(), "d".into(), 1);
        let prompt = crate::agents::generation::build_work_prompt(&phase, &[], &[], &[], None);
        let pmt = &store().generation_work;
        assert!(
            prompt.user_message.contains(pmt.trim()),
            "Work generation prompt missing .pmt instruction content"
        );
    }

    // =========================================================================
    // All 12 .pmt files: no accidental empty or whitespace-only content
    // =========================================================================

    #[test]
    fn test_all_pmt_files_have_substantial_content() {
        init_defaults();
        let s = store();
        let fields: [(&str, &str); 12] = [
            ("coordinator", &s.coordinator),
            ("implementer", &s.implementer),
            ("reviewer", &s.reviewer),
            ("researcher", &s.researcher),
            ("validator_schema", &s.validator_schema),
            ("validator_plan", &s.validator_plan),
            ("validator_spec", &s.validator_spec),
            ("validator_phase", &s.validator_phase),
            ("generation_plan", &s.generation_plan),
            ("generation_spec", &s.generation_spec),
            ("generation_phase", &s.generation_phase),
            ("generation_work", &s.generation_work),
        ];
        for (name, content) in &fields {
            assert!(
                content.trim().len() > 50,
                "{}.pmt is suspiciously short ({} chars): possibly truncated or empty",
                name,
                content.len()
            );
        }
    }

    #[test]
    fn test_all_prompts_contain_json_output_instruction() {
        init_defaults();
        let s = store();
        // All agent and generation prompts should instruct JSON output
        let json_prompts: [(&str, &str); 10] = [
            ("coordinator", &s.coordinator),
            ("implementer", &s.implementer),
            ("reviewer", &s.reviewer),
            ("researcher", &s.researcher),
            ("generation_plan", &s.generation_plan),
            ("generation_spec", &s.generation_spec),
            ("generation_phase", &s.generation_phase),
            ("generation_work", &s.generation_work),
            ("validator_plan", &s.validator_plan),
            ("validator_spec", &s.validator_spec),
        ];
        for (name, content) in &json_prompts {
            assert!(
                content.contains("JSON"),
                "{}.pmt must contain JSON output format instruction",
                name,
            );
        }
    }

    #[test]
    fn test_coordinator_pmt_has_error_handling_guidance() {
        init_defaults();
        let p = &store().coordinator;
        assert!(p.contains("do NOT retry the same action immediately"));
        assert!(p.contains("need_help"));
        assert!(p.contains("Lock Management"));
        assert!(p.contains("Failure Learning"));
    }

    #[test]
    fn test_implementer_pmt_has_workflow_and_scope() {
        init_defaults();
        let p = &store().implementer;
        assert!(p.contains("Workflow"));
        assert!(p.contains("resource_tags"));
        assert!(p.contains("propose_bundle"));
    }

    #[test]
    fn test_reviewer_pmt_has_expanded_criteria() {
        init_defaults();
        let p = &store().reviewer;
        assert!(p.contains("Concurrency"));
        assert!(p.contains("Architecture"));
        assert!(p.contains("Verdict Thresholds"));
    }

    #[test]
    fn test_researcher_pmt_has_zero_result_handling() {
        init_defaults();
        let p = &store().researcher;
        assert!(p.contains("Zero-Result Handling"));
        assert!(p.contains("File Size Limits"));
        assert!(p.contains("Learning Scope Values"));
    }
}
