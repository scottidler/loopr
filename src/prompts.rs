use std::path::Path;
use std::sync::OnceLock;

use crate::config::Config;
use crate::domain::bundle::BundleStatus;
use crate::domain::plan::HierarchyStatus;
use crate::domain::work::WorkStatus;
use crate::resources::Resources;

pub struct PromptStore {
    pub coordinator: String,
    pub implementer: String,
    pub reviewer: String,
    /// Arbitrator prompt: used for disputed bundles in place of the normal reviewer prompt.
    pub arbitrator: String,
    pub researcher: String,
    pub validator_schema: String,
    pub validator_plan: String,
    pub validator_spec: String,
    pub validator_phase: String,
    pub generation_work: String,
    pub coverage_schema: String,
    pub coverage_plan_specs: String,
    pub coverage_spec_phases: String,
    pub coverage_phase_works: String,
    pub interview: String,
    pub chat: String,
    pub chat_interview: String,
    pub chat_draft: String,
    pub chat_refine: String,
    pub chat_executing: String,
    pub tier_gate: String,
    pub decompose_spec: String,
    pub decompose_phase: String,
    pub decompose_work: String,
    pub decompose_validate: String,
    pub decompose_ratify: String,
    pub director: String,
}

static STORE: OnceLock<PromptStore> = OnceLock::new();

/// Canonical section header names used in both markdown documents and Rust parsing.
/// These are the *names* without the `## ` prefix. Use `format!("## {}", SECTION_AC)`
/// when emitting a heading, and pass the bare name to `strip_markdown_section`.
pub const SECTION_AC: &str = "Acceptance Criteria";
pub const SECTION_OVERVIEW: &str = "Overview";
pub const SECTION_IMPLEMENTATION: &str = "Implementation Notes";

/// Replace status value placeholders with canonical VARIANT_NAMES from enums,
/// and inject the configured abandon-ratio percentage threshold.
fn interpolate_status_values(content: String, max_abandon_ratio: f64) -> String {
    let pct = (max_abandon_ratio * 100.0).round() as u32;
    content
        .replace("{work_status_values}", &WorkStatus::VARIANT_NAMES.join(", "))
        .replace("{bundle_status_values}", &BundleStatus::VARIANT_NAMES.join(", "))
        .replace("{hierarchy_status_values}", &HierarchyStatus::VARIANT_NAMES.join(", "))
        .replace("{work_override_statuses}", "Ready, Superseded, Abandoned, InReview")
        .replace("{max_abandon_ratio_pct}", &pct.to_string())
}

/// Initialize the global prompt store from config. Call once at startup after config load.
///
/// `repo_path` enables repo-local prompt overrides at `{repo}/resources/{name}.pmt`.
/// Pass `Some(&config.project.repo_path)` from the daemon; pass `None` when no target
/// repo is available (standalone CLI, tests).
///
/// Prompt paths are read from config. Each path can be:
/// - A relative path resolved via Resources::load (repo-local override, XDG override,
///   then embedded default)
/// - An absolute path (loaded directly; FATAL if missing or empty - prevents silent
///   fallback that would corrupt AR experimental data by scoring the baseline as the
///   trial prompt)
pub fn init(config: &Config, repo_path: Option<&Path>) -> eyre::Result<()> {
    let max_abandon_ratio = config.agents.coordinator.max_abandon_ratio;

    // Convert config path to resource path: "coordinator" -> "coordinator.pmt",
    // "decompose/spec" -> "decompose/spec.pmt", "/abs/path.pmt" -> "/abs/path.pmt"
    let load = |configured_path: &str| -> eyre::Result<String> {
        let path = if configured_path.starts_with('/') {
            configured_path.to_string()
        } else {
            format!("{}.pmt", configured_path)
        };
        Resources::load(&path, repo_path)
    };

    // OnceLock::set returns Err if already initialized - harmless no-op
    let _ = STORE.set(PromptStore {
        coordinator: interpolate_status_values(load(&config.agents.coordinator.role.prompt)?, max_abandon_ratio),
        implementer: load(&config.agents.implementer.prompt)?,
        reviewer: load(&config.agents.reviewer.prompt)?,
        arbitrator: Resources::load("agents/arbitrator.pmt", repo_path)?,
        researcher: load(&config.agents.researcher.prompt)?,
        validator_schema: load(&config.validator.prompts.schema)?,
        validator_plan: load(&config.validator.prompts.plan)?,
        validator_spec: load(&config.validator.prompts.spec)?,
        validator_phase: load(&config.validator.prompts.phase)?,
        generation_work: load(&config.decomposer.prompts.generation_work)?,
        coverage_schema: load(&config.evaluator.prompts.schema)?,
        coverage_plan_specs: load(&config.evaluator.prompts.plan_specs)?,
        coverage_spec_phases: load(&config.evaluator.prompts.spec_phases)?,
        coverage_phase_works: load(&config.evaluator.prompts.phase_works)?,
        // agents/interview.pmt: open question on final config location; XDG-only for now
        interview: Resources::load("agents/interview.pmt", None)?,
        chat: load(&config.chat.prompts.default)?,
        chat_interview: load(&config.chat.prompts.interview)?,
        chat_draft: load(&config.chat.prompts.draft)?,
        chat_refine: load(&config.chat.prompts.refine)?,
        chat_executing: load(&config.chat.prompts.executing)?,
        tier_gate: load(&config.tier_gate.prompt)?,
        decompose_spec: load(&config.decomposer.prompts.spec)?,
        decompose_phase: load(&config.decomposer.prompts.phase)?,
        decompose_work: load(&config.decomposer.prompts.work)?,
        decompose_validate: load(&config.decomposer.prompts.validate)?,
        decompose_ratify: load(&config.decomposer.prompts.ratify)?,
        director: Resources::load("agents/director.pmt", repo_path)?,
    });
    Ok(())
}

/// Initialize with embedded defaults only (no filesystem). For tests.
pub fn init_defaults() {
    let load = |path: &str| -> String {
        Resources::load(path, None).unwrap_or_else(|e| panic!("missing embedded resource {}: {}", path, e))
    };
    let _ = STORE.set(PromptStore {
        coordinator: interpolate_status_values(load("agents/coordinator.pmt"), 0.4),
        implementer: load("agents/implementer.pmt"),
        reviewer: load("agents/reviewer.pmt"),
        arbitrator: load("agents/arbitrator.pmt"),
        researcher: load("agents/researcher.pmt"),
        validator_schema: load("decompose/schema.pmt"),
        validator_plan: load("decompose/plan/validator.pmt"),
        validator_spec: load("decompose/spec/validator.pmt"),
        validator_phase: load("decompose/phase/validator.pmt"),
        generation_work: load("decompose/work/generation.pmt"),
        coverage_schema: load("decompose/coverage-schema.pmt"),
        coverage_plan_specs: load("decompose/spec/coverage.pmt"),
        coverage_spec_phases: load("decompose/phase/coverage.pmt"),
        coverage_phase_works: load("decompose/work/coverage.pmt"),
        interview: load("agents/interview.pmt"),
        chat: load("chat/default.pmt"),
        chat_interview: load("chat/interview.pmt"),
        chat_draft: load("chat/draft.pmt"),
        chat_refine: load("chat/refine.pmt"),
        chat_executing: load("chat/executing.pmt"),
        tier_gate: load("agents/tier-gate.pmt"),
        decompose_spec: load("decompose/spec/prompt.pmt"),
        decompose_phase: load("decompose/phase/prompt.pmt"),
        decompose_work: load("decompose/work/prompt.pmt"),
        decompose_validate: load("decompose/validate.pmt"),
        decompose_ratify: load("decompose/ratify.pmt"),
        director: load("agents/director.pmt"),
    });
}

/// Get the global prompt store. Panics if init() was not called.
pub fn store() -> &'static PromptStore {
    STORE
        .get()
        .expect("prompts::init() must be called before prompts::store()")
}

#[allow(clippy::unwrap_used)]
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
        assert!(!s.generation_work.is_empty());
    }

    #[test]
    fn test_defaults_match_resources() {
        init_defaults();
        let s = store();
        // Coordinator is interpolated (status placeholders replaced)
        let raw_coordinator = Resources::load("agents/coordinator.pmt", None).unwrap();
        assert_eq!(s.coordinator, interpolate_status_values(raw_coordinator, 0.4));
        // Other prompts are loaded verbatim from embedded resources
        assert_eq!(s.implementer, Resources::load("agents/implementer.pmt", None).unwrap());
        assert_eq!(s.reviewer, Resources::load("agents/reviewer.pmt", None).unwrap());
        assert_eq!(s.researcher, Resources::load("agents/researcher.pmt", None).unwrap());
    }

    #[test]
    fn test_placeholder_assertions() {
        init_defaults();
        let s = store();
        // Researcher prompt must contain {query} placeholder
        assert!(s.researcher.contains("{query}"));
        // Validator templates must contain their placeholders
        assert!(s.validator_plan.contains("{markdown_content}"));
        assert!(s.validator_plan.contains("{schema}"));
        assert!(s.validator_spec.contains("{markdown_content}"));
        assert!(s.validator_spec.contains("{parent_markdown_content}"));
        assert!(s.validator_spec.contains("{schema}"));
        assert!(s.validator_phase.contains("{markdown_content}"));
        assert!(s.validator_phase.contains("{parent_markdown_content}"));
        assert!(s.validator_phase.contains("{schema}"));
    }

    #[test]
    fn test_override_from_temp_dir() {
        let dir = TestDir::new("loopr-pmt-override");
        let resources_dir = dir.join("resources");
        fs::create_dir_all(&resources_dir).unwrap();
        fs::create_dir_all(resources_dir.join("agents")).unwrap();
        fs::write(
            resources_dir.join("agents/coordinator.pmt"),
            "CUSTOM COORDINATOR PROMPT",
        )
        .unwrap();

        let content = Resources::load("agents/coordinator.pmt", Some(&dir)).unwrap();
        assert_eq!(content, "CUSTOM COORDINATOR PROMPT");

        // Non-overridden file falls back to embedded default
        let implementer = Resources::load("agents/implementer.pmt", Some(&dir)).unwrap();
        assert!(!implementer.is_empty());
    }

    #[test]
    fn test_empty_override_falls_back() {
        let dir = TestDir::new("loopr-pmt-empty");
        let resources_dir = dir.join("resources");
        fs::create_dir_all(&resources_dir).unwrap();
        fs::create_dir_all(resources_dir.join("agents")).unwrap();
        fs::write(resources_dir.join("agents/coordinator.pmt"), "   \n  ").unwrap();

        // Empty override treated as absent; falls back to embedded default
        let content = Resources::load("agents/coordinator.pmt", Some(&dir)).unwrap();
        assert!(
            !content.trim().is_empty(),
            "should fall back to non-empty embedded default"
        );
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
        // Live action types
        for action in [
            "create_work",
            "assign_agent",
            "spawn_researcher",
            "acquire_lock",
            "release_lock",
            "validate_document",
            "accept_bundle",
            "transition",
            "create_learning",
            "need_help",
            "done",
        ] {
            assert!(p.contains(action), "coordinator.pmt missing action: {}", action);
        }
        // Dead actions must not appear (hallucination risk guard)
        assert!(
            !p.contains("create_plan"),
            "coordinator.pmt must not contain create_plan"
        );
        assert!(
            !p.contains("create_spec"),
            "coordinator.pmt must not contain create_spec"
        );
        assert!(
            !p.contains("create_phase"),
            "coordinator.pmt must not contain create_phase"
        );
        // triage_bundle removed: bundles are now auto-triaged by the daemon (Fix 7)
        assert!(
            !p.contains("\"triage_bundle\""),
            "coordinator.pmt must not contain triage_bundle - triage is now automatic"
        );
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
        // Noop guard prompt hardening
        assert!(
            p.contains("ALREADY COMMITTED to"),
            "implementer.pmt missing noop guard: 'ALREADY COMMITTED to'"
        );
        assert!(
            p.contains("write_file` or `edit_file` at ANY point"),
            "implementer.pmt missing noop guard: write_file/edit_file rule"
        );
        assert!(
            p.contains("noop_paths"),
            "implementer.pmt missing noop_paths instruction"
        );
    }

    #[test]
    fn test_reviewer_pmt_identity() {
        init_defaults();
        let p = &store().reviewer;
        assert!(p.starts_with("You are a Reviewer agent in the Loopr development orchestrator."));
        for criterion in [
            SECTION_AC,
            "Safety & Security",
            "Build & Tests",
            "Scope",
            "Quality & Style",
        ] {
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
        let md = "---\ntitle: Implement Auth\nacceptance-criteria:\n  - All endpoints require valid token\n  - Tests pass\n---\n\nAdd JWT-based authentication to the API";

        let output = crate::validator::prompts::plan_prompt(md);

        // Full markdown content is embedded
        assert!(output.contains("Implement Auth"));
        assert!(output.contains("Add JWT-based authentication to the API"));
        assert!(output.contains("All endpoints require valid token"));
        // Schema is inlined
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        assert!(output.contains("\"severity\": \"error | warning | info\""));
        // No residual placeholders
        assert!(!output.contains("{markdown_content}"));
        assert!(!output.contains("{schema}"));
    }

    #[test]
    fn test_validator_spec_output_equivalence() {
        init_defaults();
        let md = "---\ntitle: JWT Auth Spec\nparent-id: pl-123\n---\n\nUse RS256 tokens with 15-minute expiry";
        let parent_md = "---\ntitle: Implement Auth\n---\n\nParent plan body";

        let output = crate::validator::prompts::spec_prompt(md, parent_md);

        assert!(output.contains("JWT Auth Spec"));
        assert!(output.contains("Use RS256 tokens with 15-minute expiry"));
        assert!(output.contains("Implement Auth"));
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        // No residual placeholders
        assert!(!output.contains("{markdown_content}"));
        assert!(!output.contains("{parent_markdown_content}"));
        assert!(!output.contains("{schema}"));
    }

    #[test]
    fn test_validator_phase_output_equivalence() {
        init_defaults();
        let md =
            "---\ntitle: Token Validation\norder: 2\nparent-id: sp-456\n---\n\nImplement middleware for JWT validation";
        let parent_md = "---\ntitle: JWT Auth Spec\n---\n\nSpec body";

        let output = crate::validator::prompts::phase_prompt(md, parent_md);

        assert!(output.contains("Token Validation"));
        assert!(output.contains("Implement middleware for JWT validation"));
        assert!(output.contains("order: 2"));
        assert!(output.contains("JWT Auth Spec"));
        assert!(output.contains("\"verdict\": \"pass | fail | warn\""));
        // No residual placeholders
        assert!(!output.contains("{markdown_content}"));
        assert!(!output.contains("{parent_markdown_content}"));
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
    // =========================================================================

    #[test]
    fn test_generation_work_pmt_content() {
        init_defaults();
        let p = &store().generation_work;
        assert!(p.contains("Create Works for this Phase"));
        assert!(p.contains("files"));
        assert!(p.contains("create_work"));
        assert!(p.contains("parent_id"));
    }

    // =========================================================================
    // Generation prompt integration — verify .pmt content lands in assembled msg
    // =========================================================================

    #[test]
    fn test_generation_work_prompt_contains_pmt_instructions() {
        init_defaults();
        let phase = crate::domain::phase::Phase::new("s1".into(), "Ph".into());
        let prompt = crate::agents::generation::build_work_prompt(
            &phase,
            "",
            &[],
            &std::collections::HashMap::new(),
            &[],
            &[],
            None,
            None,
        );
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
        let fields: [(&str, &str); 14] = [
            ("coordinator", &s.coordinator),
            ("implementer", &s.implementer),
            ("reviewer", &s.reviewer),
            ("researcher", &s.researcher),
            ("validator_schema", &s.validator_schema),
            ("validator_plan", &s.validator_plan),
            ("validator_spec", &s.validator_spec),
            ("validator_phase", &s.validator_phase),
            ("generation_work", &s.generation_work),
            ("coverage_schema", &s.coverage_schema),
            ("coverage_plan_specs", &s.coverage_plan_specs),
            ("coverage_spec_phases", &s.coverage_spec_phases),
            ("coverage_phase_works", &s.coverage_phase_works),
            ("interview", &s.interview),
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
        let json_prompts: [(&str, &str); 7] = [
            ("coordinator", &s.coordinator),
            ("implementer", &s.implementer),
            ("reviewer", &s.reviewer),
            ("researcher", &s.researcher),
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
        assert!(p.contains("files"));
        assert!(p.contains("propose_bundle"));
    }

    #[test]
    fn test_reviewer_pmt_has_expanded_criteria() {
        init_defaults();
        let p = &store().reviewer;
        assert!(p.contains("concurrency"));
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

    #[test]
    fn test_coordinator_status_placeholders_interpolated() {
        init_defaults();
        let p = &store().coordinator;
        // No raw placeholders should remain
        assert!(
            !p.contains("{work_status_values}"),
            "un-interpolated {{work_status_values}}"
        );
        assert!(
            !p.contains("{bundle_status_values}"),
            "un-interpolated {{bundle_status_values}}"
        );
        assert!(
            !p.contains("{hierarchy_status_values}"),
            "un-interpolated {{hierarchy_status_values}}"
        );
        assert!(
            !p.contains("{work_override_statuses}"),
            "un-interpolated {{work_override_statuses}}"
        );
        assert!(
            !p.contains("{max_abandon_ratio_pct}"),
            "un-interpolated {{max_abandon_ratio_pct}}"
        );
        // Canonical values should appear
        assert!(p.contains("Draft"), "missing Draft in interpolated coordinator prompt");
        assert!(
            p.contains("InProgress"),
            "missing InProgress in interpolated coordinator prompt"
        );
        assert!(
            p.contains("Proposed"),
            "missing Proposed (BundleStatus) in interpolated coordinator prompt"
        );
    }

    // =========================================================================
    // Sentinel tests: every interpolated .pmt has zero residual placeholders
    // and all sentinel values appear in the output.
    // =========================================================================

    /// Assert no `{word}` patterns remain in the output (residual placeholders).
    ///
    /// `exceptions` lists placeholder strings that are expected in the output
    /// (e.g., `{plan_id}` appearing as a prose example, not a substitution target).
    fn assert_no_residual_placeholders(output: &str, template_name: &str, exceptions: &[&str]) {
        let re = regex::Regex::new(r"\{[a-z_]+\}").unwrap();
        let residuals: Vec<&str> = re
            .find_iter(output)
            .map(|m| m.as_str())
            .filter(|p| !exceptions.contains(p))
            .collect();
        assert!(
            residuals.is_empty(),
            "{} has residual placeholders: {:?}",
            template_name,
            residuals
        );
    }

    #[test]
    fn test_sentinel_validator_plan() {
        init_defaults();
        let sentinel = "SENTINEL_PLAN_MARKDOWN_ae92f1";
        let output = crate::validator::prompts::plan_prompt(sentinel);
        assert!(
            output.contains(sentinel),
            "sentinel value missing from validator-plan output"
        );
        assert_no_residual_placeholders(&output, "validator-plan.pmt", &[]);
    }

    #[test]
    fn test_sentinel_validator_spec() {
        init_defaults();
        let sentinel_md = "SENTINEL_SPEC_MD_b37c02";
        let sentinel_parent = "SENTINEL_PARENT_MD_c48d03";
        let output = crate::validator::prompts::spec_prompt(sentinel_md, sentinel_parent);
        assert!(output.contains(sentinel_md), "spec sentinel missing");
        assert!(output.contains(sentinel_parent), "parent sentinel missing");
        assert_no_residual_placeholders(&output, "validator-spec.pmt", &[]);
    }

    #[test]
    fn test_sentinel_validator_phase() {
        init_defaults();
        let sentinel_md = "SENTINEL_PHASE_MD_d59e04";
        let sentinel_parent = "SENTINEL_PARENT_MD_e60f05";
        let output = crate::validator::prompts::phase_prompt(sentinel_md, sentinel_parent);
        assert!(output.contains(sentinel_md), "phase sentinel missing");
        assert!(output.contains(sentinel_parent), "parent sentinel missing");
        assert_no_residual_placeholders(&output, "validator-phase.pmt", &[]);
    }

    #[test]
    fn test_sentinel_coverage_plan_specs() {
        init_defaults();
        let sentinel_parent = "SENTINEL_PLAN_MD_f71a06";
        let sentinel_children = "SENTINEL_SPECS_MD_a82b07";
        let output = crate::evaluator::prompts::plan_specs_prompt(sentinel_parent, sentinel_children);
        assert!(output.contains(sentinel_parent), "parent sentinel missing");
        assert!(output.contains(sentinel_children), "children sentinel missing");
        assert_no_residual_placeholders(&output, "coverage-plan-specs.pmt", &[]);
    }

    #[test]
    fn test_sentinel_coverage_spec_phases() {
        init_defaults();
        let sentinel_parent = "SENTINEL_SPEC_MD_b93c08";
        let sentinel_children = "SENTINEL_PHASES_MD_c04d09";
        let output = crate::evaluator::prompts::spec_phases_prompt(sentinel_parent, sentinel_children);
        assert!(output.contains(sentinel_parent), "parent sentinel missing");
        assert!(output.contains(sentinel_children), "children sentinel missing");
        assert_no_residual_placeholders(&output, "coverage-spec-phases.pmt", &[]);
    }

    #[test]
    fn test_sentinel_coverage_phase_works() {
        init_defaults();
        let sentinel_parent = "SENTINEL_PHASE_MD_d15e10";
        let sentinel_children = "SENTINEL_WORKS_MD_e26f11";
        let output = crate::evaluator::prompts::phase_works_prompt(sentinel_parent, sentinel_children);
        assert!(output.contains(sentinel_parent), "parent sentinel missing");
        assert!(output.contains(sentinel_children), "children sentinel missing");
        assert_no_residual_placeholders(&output, "coverage-phase-works.pmt", &[]);
    }

    #[test]
    fn test_sentinel_researcher() {
        init_defaults();
        let sentinel_query = "SENTINEL_QUERY_f37a12";
        let output = store().researcher.replace("{query}", sentinel_query);
        assert!(output.contains(sentinel_query), "query sentinel missing");
        assert_no_residual_placeholders(&output, "researcher.pmt", &["{plan_id}"]);
    }

    // =========================================================================
    // Prompt path resolution tests (via Resources::load)
    // =========================================================================

    #[test]
    fn test_absolute_prompt_path_not_found_returns_err() {
        let result = Resources::load("/nonexistent/path/that/does/not/exist.pmt", None);
        assert!(result.is_err(), "absolute path for nonexistent file must fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("absolute resource path not found")
        );
    }

    #[test]
    fn test_absolute_prompt_path_empty_returns_err() {
        let dir = TestDir::new("loopr-pmt-abs-empty");
        let empty_file = dir.join("empty.pmt");
        fs::write(&empty_file, "   \n  ").unwrap();
        let abs_path = empty_file.to_string_lossy().to_string();

        let result = Resources::load(&abs_path, None);
        assert!(result.is_err(), "empty absolute path file must fail");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("absolute resource path is empty")
        );
    }

    #[test]
    fn test_absolute_prompt_path_loads_correctly() {
        let dir = TestDir::new("loopr-pmt-abs-ok");
        let prompt_file = dir.join("custom.pmt");
        fs::write(&prompt_file, "CUSTOM ABSOLUTE PROMPT").unwrap();
        let abs_path = prompt_file.to_string_lossy().to_string();

        let result = Resources::load(&abs_path, None).unwrap();
        assert_eq!(result, "CUSTOM ABSOLUTE PROMPT");
    }
}
