use super::{assemble_system, assemble_user};

#[test]
fn system_template_substitutes_tree_marker() {
    let s = assemble_system("src/main.rs\nREADME.md");
    assert!(!s.contains("{{ TREE }}"), "marker must be substituted: {s}");
    assert!(s.contains("src/main.rs"), "tree contents land in prompt: {s}");
    assert!(s.contains("software architect"), "framing retained: {s}");
}

#[test]
fn system_template_handles_empty_workspace_sentinel() {
    let s = assemble_system("(empty workspace)");
    assert!(s.contains("(empty workspace)"), "sentinel interpolates: {s}");
}

#[test]
fn user_message_without_prev_error_has_plan_only() {
    let u = assemble_user("build a CLI", None);
    assert!(u.contains("## Plan"));
    assert!(u.contains("build a CLI"));
    assert!(!u.contains("Previous Attempt Failed"), "no retry section on first call");
}

#[test]
fn user_message_with_prev_error_includes_retry_section() {
    let u = assemble_user("build a CLI", Some("tool_use block missing"));
    assert!(u.contains("## Plan"));
    assert!(u.contains("## Previous Attempt Failed"));
    assert!(u.contains("tool_use block missing"));
    assert!(u.contains("fix the issues"));
}

#[test]
fn user_message_truncates_oversized_retry_error_with_exact_suffix() {
    // Build a 10 KiB error (10240 bytes of ASCII).
    let oversized: String = "a".repeat(10240);
    let u = assemble_user("goal", Some(&oversized));

    // The exact suffix wording must match the Phase 6 assertion.
    assert!(
        u.contains("[error truncated from 10240 bytes]"),
        "exact truncation suffix missing; got: ...{}",
        &u[u.len().saturating_sub(200)..]
    );
    // And the full 10 KiB should NOT appear verbatim.
    assert!(
        !u.contains(&oversized),
        "original oversized error text should not appear verbatim"
    );
}

#[test]
fn user_message_does_not_truncate_retry_error_under_cap() {
    let small = "boom";
    let u = assemble_user("goal", Some(small));
    assert!(u.contains("boom"));
    assert!(!u.contains("error truncated"), "under-cap error must not be truncated");
}
