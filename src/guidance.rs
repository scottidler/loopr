//! Agent Guidance System — auto-generated schema docs + LOOPR.md loading.
//!
//! Four layers of guidance assembled at context-build time:
//! 1. Built-in role prompts (.pmt) — unchanged, handled by `prompts.rs`
//! 2. Global user preferences (~/.config/loopr/LOOPR.md)
//! 3. Project conventions ($TARGET_PROJECT/LOOPR.md)
//! 4. Auto-generated schema docs (transition graphs, valid actions, status enums)

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;
use std::path::Path;

use log::{info, warn};

use crate::domain::bundle::bundle_transitions;
use crate::domain::plan::hierarchy_transitions;
use crate::domain::role::Role;
use crate::domain::transition::TransitionRule;
use crate::domain::work::work_transitions;

/// Assembled guidance from all layers, ready for context injection.
#[derive(Debug, Clone)]
pub struct AgentGuidance {
    /// Layer 2: Global user preferences (from ~/.config/loopr/LOOPR.md)
    pub global_md: Option<String>,
    /// Layer 3: Project conventions (from $TARGET/LOOPR.md)
    pub project_md: Option<String>,
    /// Layer 4: Auto-generated schema, keyed by role
    pub schema_docs: HashMap<Role, String>,
}

/// All roles that need schema docs generated.
const ALL_ROLES: [Role; 5] = [
    Role::Coordinator,
    Role::Integrator,
    Role::Implementer,
    Role::Reviewer,
    Role::Researcher,
];

impl AgentGuidance {
    /// Generate guidance with schema docs for all roles, no LOOPR.md files.
    pub fn schema_only() -> Self {
        let schema_docs = ALL_ROLES
            .into_iter()
            .map(|role| (role, generate_schema_doc(role)))
            .collect();
        Self {
            global_md: None,
            project_md: None,
            schema_docs,
        }
    }
}

/// Load an optional UTF-8 text file, returning None if missing or unreadable.
fn load_optional_file(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => {
            info!("Loaded guidance file: {}", path.display());
            Some(content)
        }
        Ok(_) => {
            // File exists but is empty/whitespace
            None
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!("Failed to read guidance file {}: {}", path.display(), e);
            None
        }
    }
}

/// Load guidance from filesystem + generate schema docs.
pub fn load_guidance(repo_path: &Path) -> AgentGuidance {
    let global_md = dirs::config_dir()
        .map(|d| d.join("loopr/LOOPR.md"))
        .and_then(|p| load_optional_file(&p));

    let project_md = load_optional_file(&repo_path.join("LOOPR.md"));

    let schema_docs = ALL_ROLES
        .into_iter()
        .map(|role| (role, generate_schema_doc(role)))
        .collect();

    AgentGuidance {
        global_md,
        project_md,
        schema_docs,
    }
}

/// Generate role-specific schema documentation from transition rules.
///
/// Reads from the same `work_transitions()`, `bundle_transitions()`,
/// `hierarchy_transitions()` functions that the runtime validates against —
/// guaranteed in sync by construction.
pub fn generate_schema_doc(role: Role) -> String {
    let mut doc = String::with_capacity(2048);

    doc.push_str(&format!("### System Rules (your role: {})\n\n", role));

    // Work transitions
    let work_rules = work_transitions();
    doc.push_str("## Work Status Transitions\n");
    append_transitions(&mut doc, &work_rules, role);
    append_terminal_states(&mut doc, &work_rules);

    // Hierarchy transitions (Plan/Spec/Phase share the same FSM)
    let hierarchy_rules = hierarchy_transitions();
    doc.push_str("## Plan/Spec/Phase Status Transitions\n");
    append_transitions(&mut doc, &hierarchy_rules, role);
    append_terminal_states(&mut doc, &hierarchy_rules);

    // Bundle transitions
    let bundle_rules = bundle_transitions();
    doc.push_str("## Bundle Status Transitions\n");
    append_transitions(&mut doc, &bundle_rules, role);
    append_terminal_states(&mut doc, &bundle_rules);

    doc
}

/// Derive terminal states from transition rules.
///
/// A terminal state is one that appears as a `to` destination but never as a `from` source.
/// This is derived purely from the rules — no hardcoded state names.
fn append_terminal_states<S: Debug + Copy + Eq + Hash>(doc: &mut String, rules: &[TransitionRule<S>]) {
    let from_states: HashSet<_> = rules.iter().map(|r| r.from).collect();
    let to_states: HashSet<_> = rules.iter().map(|r| r.to).collect();
    let mut terminals: Vec<_> = to_states.difference(&from_states).collect();
    // Sort for deterministic output
    terminals.sort_by_key(|s| format!("{:?}", s));
    if !terminals.is_empty() {
        let names: Vec<String> = terminals.iter().map(|s| format!("{:?}", s)).collect();
        doc.push_str(&format!(
            "\nTerminal states: {} (no outgoing transitions)\n\n",
            names.join(", ")
        ));
    }
}

/// Append filtered transition rules to the doc string.
/// Shows rules that the given role can execute (role matches or role is None = any).
fn append_transitions<S: Debug>(doc: &mut String, rules: &[TransitionRule<S>], role: Role) {
    let mut found = false;
    for rule in rules {
        let allowed = rule.role.is_none() || rule.role == Some(role);
        if allowed {
            doc.push_str(&format!("{:?} → {:?}", rule.from, rule.to));
            if rule.role.is_none() {
                doc.push_str("  (any role)");
            }
            doc.push('\n');
            found = true;
        }
    }
    if !found {
        doc.push_str("(no transitions available for your role)\n");
    }
}

/// Assemble the full guidance text block for a given role.
///
/// Order: schema docs (never truncated) → global LOOPR.md → project LOOPR.md.
/// The caller is responsible for token budget enforcement on the md sections.
pub fn assemble_guidance(guidance: &AgentGuidance, role: Role, guidance_budget: usize) -> String {
    let mut text = String::with_capacity(4096);

    // Schema docs — always included, never truncated
    if let Some(schema) = guidance.schema_docs.get(&role) {
        text.push_str(schema);
    }

    let schema_tokens = crate::agents::context::estimate_tokens(&text);
    let remaining = guidance_budget.saturating_sub(schema_tokens);

    // Global LOOPR.md
    if let Some(ref global) = guidance.global_md {
        let global_tokens = crate::agents::context::estimate_tokens(global);
        if global_tokens <= remaining {
            text.push_str("### User Preferences\n\n");
            text.push_str(global);
            text.push_str("\n\n");
        } else if remaining > 50 {
            text.push_str("### User Preferences\n\n");
            // Truncate global to fit
            let max_chars = remaining * 4;
            let truncated = &global[..global.len().min(max_chars)];
            text.push_str(truncated);
            text.push_str("\n[truncated]\n\n");
            warn!("Global LOOPR.md truncated to fit guidance budget");
            return text; // No room for project md
        }
    }

    let current_tokens = crate::agents::context::estimate_tokens(&text);
    let remaining = guidance_budget.saturating_sub(current_tokens);

    // Project LOOPR.md — truncated first if budget exceeded
    if let Some(ref project) = guidance.project_md {
        let project_tokens = crate::agents::context::estimate_tokens(project);
        if project_tokens <= remaining {
            text.push_str("### Project Conventions\n\n");
            text.push_str(project);
            text.push_str("\n\n");
        } else if remaining > 50 {
            text.push_str("### Project Conventions\n\n");
            let max_chars = remaining * 4;
            let truncated = &project[..project.len().min(max_chars)];
            text.push_str(truncated);
            text.push_str("\n[truncated]\n\n");
            warn!("Project LOOPR.md truncated to fit guidance budget");
        }
    }

    text
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    // =====================================================
    // generate_schema_doc: Coordinator
    // =====================================================

    #[test]
    fn test_coordinator_sees_all_work_transitions_it_can_execute() {
        let doc = generate_schema_doc(Role::Coordinator);
        // Coordinator can execute these work transitions
        assert!(doc.contains("Draft → Ready"), "missing Draft → Ready");
        assert!(doc.contains("Ready → InProgress"), "missing Ready → InProgress");
        assert!(doc.contains("Blocked → Ready"), "missing Blocked → Ready");
        assert!(doc.contains("InReview → InProgress"), "missing InReview → InProgress");
        assert!(doc.contains("Integrated → Done"), "missing Integrated → Done");
        // InProgress → Blocked is any-role, coordinator should see it
        assert!(doc.contains("InProgress → Blocked"), "missing InProgress → Blocked");
        assert!(doc.contains("(any role)"), "missing any-role annotation");
        // Abandoned transitions
        assert!(doc.contains("Draft → Abandoned"), "missing Draft → Abandoned");
        assert!(doc.contains("Ready → Abandoned"), "missing Ready → Abandoned");
    }

    #[test]
    fn test_coordinator_does_not_see_implementer_only_transitions() {
        let doc = generate_schema_doc(Role::Coordinator);
        // InProgress → InReview is Implementer-only
        assert!(
            !doc.contains("InProgress → InReview"),
            "Coordinator should not see InProgress → InReview (Implementer-only)"
        );
    }

    #[test]
    fn test_coordinator_does_not_see_integrator_only_work_transitions() {
        let doc = generate_schema_doc(Role::Coordinator);
        // InReview → Integrated is Integrator-only
        assert!(
            !doc.contains("InReview → Integrated"),
            "Coordinator should not see InReview → Integrated (Integrator-only)"
        );
    }

    #[test]
    fn test_coordinator_sees_hierarchy_transitions() {
        let doc = generate_schema_doc(Role::Coordinator);
        assert!(doc.contains("Draft → Active"), "missing Draft → Active");
        assert!(doc.contains("Active → Complete"), "missing Active → Complete");
        assert!(doc.contains("Draft → Abandoned"), "missing hierarchy Draft → Abandoned");
        assert!(
            doc.contains("Active → Abandoned"),
            "missing hierarchy Active → Abandoned"
        );
    }

    #[test]
    fn test_coordinator_sees_bundle_transitions() {
        let doc = generate_schema_doc(Role::Coordinator);
        assert!(doc.contains("Proposed → Triaged"), "missing Proposed → Triaged");
        assert!(doc.contains("Triaged → Reviewed"), "missing Triaged → Reviewed");
        assert!(doc.contains("Reviewed → Accepted"), "missing Reviewed → Accepted");
        // Coordinator can reject
        assert!(doc.contains("Proposed → Rejected"), "missing Proposed → Rejected");
        // Coordinator can supersede
        assert!(doc.contains("Proposed → Superseded"), "missing Proposed → Superseded");
    }

    // =====================================================
    // generate_schema_doc: Implementer
    // =====================================================

    #[test]
    fn test_implementer_sees_own_transitions() {
        let doc = generate_schema_doc(Role::Implementer);
        // Implementer can do InProgress → InReview
        assert!(doc.contains("InProgress → InReview"), "missing InProgress → InReview");
        // InProgress → Blocked is any-role
        assert!(doc.contains("InProgress → Blocked"), "missing InProgress → Blocked");
    }

    #[test]
    fn test_implementer_does_not_see_coordinator_transitions() {
        let doc = generate_schema_doc(Role::Implementer);
        // Draft → Ready is Coordinator-only
        assert!(
            !doc.contains("Draft → Ready"),
            "Implementer should not see Draft → Ready"
        );
        // Ready → InProgress is Coordinator-only
        assert!(
            !doc.contains("Ready → InProgress"),
            "Implementer should not see Ready → InProgress"
        );
    }

    #[test]
    fn test_implementer_sees_no_hierarchy_transitions() {
        let doc = generate_schema_doc(Role::Implementer);
        // All hierarchy transitions require Coordinator
        assert!(
            !doc.contains("Draft → Active"),
            "Implementer should not see hierarchy transitions"
        );
    }

    // =====================================================
    // generate_schema_doc: Integrator
    // =====================================================

    #[test]
    fn test_integrator_sees_own_transitions() {
        let doc = generate_schema_doc(Role::Integrator);
        // Integrator can do InReview → Integrated
        assert!(doc.contains("InReview → Integrated"), "missing InReview → Integrated");
        // Integrator can do Integrated → Done
        assert!(doc.contains("Integrated → Done"), "missing Integrated → Done");
        // InProgress → Blocked is any-role
        assert!(doc.contains("InProgress → Blocked"), "missing InProgress → Blocked");
    }

    #[test]
    fn test_integrator_bundle_transitions() {
        let doc = generate_schema_doc(Role::Integrator);
        assert!(doc.contains("Accepted → Integrating"), "missing Accepted → Integrating");
        assert!(doc.contains("Integrating → Merged"), "missing Integrating → Merged");
        assert!(doc.contains("Integrating → Rejected"), "missing Integrating → Rejected");
        assert!(
            doc.contains("Accepted → Rejected"),
            "missing Accepted → Rejected (stale base)"
        );
    }

    // =====================================================
    // generate_schema_doc: Reviewer
    // =====================================================

    #[test]
    fn test_reviewer_sees_only_any_role_work_transitions() {
        let doc = generate_schema_doc(Role::Reviewer);
        // Reviewer has no specific work transitions, only any-role ones
        assert!(
            doc.contains("InProgress → Blocked"),
            "missing any-role InProgress → Blocked"
        );
        // Should NOT see Coordinator-only transitions
        assert!(!doc.contains("Draft → Ready"), "Reviewer should not see Draft → Ready");
    }

    #[test]
    fn test_reviewer_bundle_transitions() {
        let doc = generate_schema_doc(Role::Reviewer);
        assert!(doc.contains("Triaged → Reviewed"), "missing Triaged → Reviewed");
        assert!(doc.contains("Proposed → Rejected"), "missing Proposed → Rejected");
        assert!(doc.contains("Triaged → Rejected"), "missing Triaged → Rejected");
        assert!(doc.contains("Reviewed → Rejected"), "missing Reviewed → Rejected");
    }

    // =====================================================
    // generate_schema_doc: Researcher
    // =====================================================

    #[test]
    fn test_researcher_sees_only_any_role_transitions() {
        let doc = generate_schema_doc(Role::Researcher);
        // Only any-role transitions
        assert!(
            doc.contains("InProgress → Blocked"),
            "missing any-role InProgress → Blocked"
        );
        // No Coordinator-only transitions
        assert!(
            !doc.contains("Draft → Ready"),
            "Researcher should not see Draft → Ready"
        );
    }

    #[test]
    fn test_researcher_no_hierarchy_transitions() {
        let doc = generate_schema_doc(Role::Researcher);
        assert!(
            doc.contains("(no transitions available for your role)"),
            "Researcher should have no hierarchy transitions"
        );
    }

    // =====================================================
    // generate_schema_doc: structural checks
    // =====================================================

    #[test]
    fn test_schema_doc_contains_section_headers() {
        for role in ALL_ROLES {
            let doc = generate_schema_doc(role);
            assert!(
                doc.contains("## Work Status Transitions"),
                "missing work section for {role}"
            );
            assert!(
                doc.contains("## Plan/Spec/Phase Status Transitions"),
                "missing hierarchy section for {role}"
            );
            assert!(
                doc.contains("## Bundle Status Transitions"),
                "missing bundle section for {role}"
            );
        }
    }

    #[test]
    fn test_schema_doc_contains_terminal_states() {
        for role in ALL_ROLES {
            let doc = generate_schema_doc(role);
            // Work terminals (derived from rules: states with no outgoing transitions)
            assert!(doc.contains("Done"), "missing work terminal Done for {role}");
            assert!(doc.contains("Abandoned"), "missing work terminal Abandoned for {role}");
            // Hierarchy terminals
            assert!(
                doc.contains("Complete"),
                "missing hierarchy terminal Complete for {role}"
            );
            // Bundle terminals
            assert!(doc.contains("Merged"), "missing bundle terminal Merged for {role}");
            assert!(doc.contains("Rejected"), "missing bundle terminal Rejected for {role}");
            assert!(
                doc.contains("Superseded"),
                "missing bundle terminal Superseded for {role}"
            );
            // Verify the "Terminal states:" label appears (derived, not hardcoded)
            assert!(
                doc.contains("Terminal states:"),
                "missing terminal states label for {role}"
            );
        }
    }

    #[test]
    fn test_schema_doc_completeness_work_transitions() {
        // Every work transition rule should appear in at least one role's schema doc
        let rules = work_transitions();
        for rule in &rules {
            let role = rule.role.unwrap_or(Role::Coordinator);
            let doc = generate_schema_doc(role);
            let from = format!("{:?}", rule.from);
            let to = format!("{:?}", rule.to);
            assert!(
                doc.contains(&format!("{from} → {to}")),
                "Work transition {from} → {to} missing from {role}'s schema doc"
            );
        }
    }

    #[test]
    fn test_schema_doc_completeness_bundle_transitions() {
        let rules = bundle_transitions();
        for rule in &rules {
            let role = rule.role.unwrap_or(Role::Coordinator);
            let doc = generate_schema_doc(role);
            let from = format!("{:?}", rule.from);
            let to = format!("{:?}", rule.to);
            assert!(
                doc.contains(&format!("{from} → {to}")),
                "Bundle transition {from} → {to} missing from {role}'s schema doc"
            );
        }
    }

    #[test]
    fn test_schema_doc_completeness_hierarchy_transitions() {
        let rules = hierarchy_transitions();
        for rule in &rules {
            let role = rule.role.unwrap_or(Role::Coordinator);
            let doc = generate_schema_doc(role);
            let from = format!("{:?}", rule.from);
            let to = format!("{:?}", rule.to);
            assert!(
                doc.contains(&format!("{from} → {to}")),
                "Hierarchy transition {from} → {to} missing from {role}'s schema doc"
            );
        }
    }

    // =====================================================
    // AgentGuidance
    // =====================================================

    #[test]
    fn test_schema_only_has_all_roles() {
        let guidance = AgentGuidance::schema_only();
        assert!(guidance.global_md.is_none());
        assert!(guidance.project_md.is_none());
        for role in ALL_ROLES {
            assert!(guidance.schema_docs.contains_key(&role), "missing schema for {role}");
        }
    }

    // =====================================================
    // assemble_guidance
    // =====================================================

    #[test]
    fn test_assemble_guidance_schema_only() {
        let guidance = AgentGuidance::schema_only();
        let text = assemble_guidance(&guidance, Role::Coordinator, 5000);
        assert!(text.contains("## Work Status Transitions"));
        assert!(!text.contains("### User Preferences"));
        assert!(!text.contains("### Project Conventions"));
    }

    #[test]
    fn test_assemble_guidance_with_global_and_project() {
        let mut guidance = AgentGuidance::schema_only();
        guidance.global_md = Some("Use ES modules".to_string());
        guidance.project_md = Some("Use rspec".to_string());

        let text = assemble_guidance(&guidance, Role::Coordinator, 5000);
        assert!(text.contains("### User Preferences"));
        assert!(text.contains("Use ES modules"));
        assert!(text.contains("### Project Conventions"));
        assert!(text.contains("Use rspec"));
    }

    #[test]
    fn test_assemble_guidance_truncation_priority() {
        let mut guidance = AgentGuidance::schema_only();
        guidance.global_md = Some("short global".to_string());
        // Make project md very large — should get truncated first
        guidance.project_md = Some("x".repeat(20000));

        // Small budget that fits schema + global but not project
        let schema_size =
            crate::agents::context::estimate_tokens(guidance.schema_docs.get(&Role::Coordinator).unwrap());
        let budget = schema_size + 100; // room for global but not full project

        let text = assemble_guidance(&guidance, Role::Coordinator, budget);
        assert!(text.contains("short global"), "global should fit");
        // Project may be truncated or missing depending on remaining budget
        if text.contains("### Project Conventions") {
            assert!(text.contains("[truncated]"), "project should be truncated");
        }
    }

    // =====================================================
    // load_optional_file
    // =====================================================

    #[test]
    fn test_load_optional_file_missing() {
        let result = load_optional_file(Path::new("/nonexistent/LOOPR.md"));
        assert!(result.is_none());
    }

    #[test]
    fn test_load_optional_file_empty() {
        let dir = TestDir::new("loopr-guidance");
        let file = dir.join("LOOPR.md");
        std::fs::write(&file, "   \n  ").unwrap();
        let result = load_optional_file(&file);
        assert!(result.is_none());
    }

    #[test]
    fn test_load_optional_file_content() {
        let dir = TestDir::new("loopr-guidance");
        let file = dir.join("LOOPR.md");
        std::fs::write(&file, "# Conventions\n- Use rspec").unwrap();
        let result = load_optional_file(&file);
        assert_eq!(result, Some("# Conventions\n- Use rspec".to_string()));
    }

    // =====================================================
    // load_guidance
    // =====================================================

    #[test]
    fn test_load_guidance_no_files() {
        let dir = TestDir::new("loopr-guidance");
        let guidance = load_guidance(&dir);
        assert!(guidance.project_md.is_none());
        // global_md depends on whether ~/.config/loopr/LOOPR.md exists
        assert_eq!(guidance.schema_docs.len(), 5);
    }

    #[test]
    fn test_load_guidance_with_project_file() {
        let dir = TestDir::new("loopr-guidance");
        std::fs::write(dir.join("LOOPR.md"), "# Project\n- Use Jest").unwrap();

        let guidance = load_guidance(&dir);
        assert_eq!(guidance.project_md, Some("# Project\n- Use Jest".to_string()));
    }
}
