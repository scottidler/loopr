#![allow(clippy::unwrap_used)]

use std::path::Path;

use domain::{AcceptanceCriteria, PlanId, Work};
use tools::ToolSchema;

use crate::{BundleLine, ContextBuilder, DirectorState, IterationSummary, StateSummary, WorkLine};

use super::InlineContextBuilder;

fn sample_work() -> Work {
    let mut w = Work::new(PlanId::new(), "add --version flag".to_string());
    w.acceptance_criteria = AcceptanceCriteria(vec![
        "cli parses --version".to_string(),
        "prints GIT_DESCRIBE output".to_string(),
    ]);
    w
}

fn bash_schema() -> ToolSchema {
    ToolSchema {
        name: "bash",
        description: "Run a bash command in the worktree",
        input_schema: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
    }
}

#[test]
fn build_returns_non_empty_assembled_context() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(
            &work,
            Path::new("/tmp/wt"),
            &[bash_schema()],
            &[],
            &StateSummary::default(),
            1,
        )
        .unwrap();
    assert!(!out.system_prompt.is_empty());
    assert!(!out.first_user_text().unwrap().is_empty());
    assert!(out.token_estimate > 0);
}

#[test]
fn renders_allowed_files_scope_when_present() {
    // Finding 10: work.files must be rendered so the agent can see its scope.
    let builder = InlineContextBuilder::new();
    let mut work = sample_work();
    work.files = vec!["src/cli.rs".to_string(), "src/config.rs".to_string()];
    let out = builder
        .build_for_implementer(
            &work,
            Path::new("/tmp/wt"),
            &[bash_schema()],
            &[],
            &StateSummary::default(),
            1,
        )
        .unwrap();
    let user = out.first_user_text().unwrap();
    assert!(user.contains("Allowed Files"), "scope section missing: {user}");
    assert!(user.contains("src/cli.rs"));
    assert!(user.contains("src/config.rs"));
}

#[test]
fn omits_allowed_files_section_when_scope_empty() {
    let builder = InlineContextBuilder::new();
    let work = sample_work(); // files empty by default
    let out = builder
        .build_for_implementer(
            &work,
            Path::new("/tmp/wt"),
            &[bash_schema()],
            &[],
            &StateSummary::default(),
            1,
        )
        .unwrap();
    assert!(!out.first_user_text().unwrap().contains("Allowed Files"));
}

#[test]
fn system_prompt_contains_tool_names() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(
            &work,
            Path::new("/tmp/wt"),
            &[bash_schema()],
            &[],
            &StateSummary::default(),
            1,
        )
        .unwrap();
    assert!(out.system_prompt.contains("bash"));
    assert!(out.system_prompt.contains("Run a bash command"));
}

#[test]
fn system_prompt_handles_zero_tools() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();
    assert!(
        out.system_prompt.contains("(none)"),
        "zero-tool case must be explicit: {}",
        out.system_prompt
    );
}

#[test]
fn user_message_contains_title_and_ac() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("add --version flag"));
    assert!(out.first_user_text().unwrap().contains("cli parses --version"));
    assert!(out.first_user_text().unwrap().contains("prints GIT_DESCRIBE"));
}

#[test]
fn user_message_includes_worktree_path() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(
            &work,
            Path::new("/var/tmp/wt-xyz"),
            &[],
            &[],
            &StateSummary::default(),
            1,
        )
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("/var/tmp/wt-xyz"));
}

#[test]
fn user_message_includes_iteration_count() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 17)
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("Iteration: 17"));
}

#[test]
fn user_message_empty_ac_does_not_crash() {
    let builder = InlineContextBuilder::new();
    let mut work = sample_work();
    work.acceptance_criteria = AcceptanceCriteria::default();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("(none specified)"));
}

#[test]
fn user_message_threads_rejected_bundle_reason() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let state = StateSummary {
        rejected_bundle_reason: Some("tests failed: test_foo".into()),
    };
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &state, 2)
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("Prior Bundle Was Rejected"));
    assert!(out.first_user_text().unwrap().contains("tests failed: test_foo"));
}

#[test]
fn user_message_renders_iteration_history() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let history = vec![
        IterationSummary {
            iteration: 1,
            actions_summary: "ran bash: ls".to_string(),
        },
        IterationSummary {
            iteration: 2,
            actions_summary: "wrote file cli.rs".to_string(),
        },
    ];
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &history, &StateSummary::default(), 3)
        .unwrap();
    assert!(out.first_user_text().unwrap().contains("Iteration 1"));
    assert!(out.first_user_text().unwrap().contains("ran bash: ls"));
    assert!(out.first_user_text().unwrap().contains("Iteration 2"));
    assert!(out.first_user_text().unwrap().contains("wrote file cli.rs"));
}

#[test]
fn iteration_summary_capped_at_4000_chars() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let huge = "x".repeat(10_000);
    let history = vec![IterationSummary {
        iteration: 1,
        actions_summary: huge.clone(),
    }];
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &history, &StateSummary::default(), 2)
        .unwrap();
    // The 10_000-char 'x' run must not appear verbatim in its entirety.
    assert!(
        !out.first_user_text().unwrap().contains(&huge),
        "10k-char summary must not appear in full"
    );
    // But a truncated prefix + marker must appear.
    assert!(
        out.first_user_text().unwrap().contains("truncated"),
        "expected truncation marker in: {}",
        &out.first_user_text().unwrap()[..500.min(out.first_user_text().unwrap().len())]
    );
    // Roughly: history body should not be longer than the cap + marker
    // + the surrounding iteration scaffolding; assert we're close to
    // the cap, not still carrying 10k chars.
    assert!(
        out.first_user_text().unwrap().len() < 6_000,
        "user_message grew to {} chars; cap should hold",
        out.first_user_text().unwrap().len()
    );
}

// ---------------------------------------------------------------------------
// build_for_director: mode-label rendering (Phase 6 of
// docs/design/2026-05-09-director-phase-2.md)
// ---------------------------------------------------------------------------

fn sample_director_state(mode: &str) -> DirectorState {
    DirectorState {
        plan_id: "pl-test-1".to_string(),
        mode: mode.to_string(),
        works: vec![WorkLine {
            id: "wk-1".to_string(),
            title: "first work".to_string(),
            status: "Pending".to_string(),
            attempt_count: 0,
        }],
        bundles: vec![BundleLine {
            id: "bd-1".to_string(),
            work_id: "wk-1".to_string(),
            status: "Reviewed".to_string(),
        }],
        blocked_reason: None,
        operator_notes: Vec::new(),
        max_work_attempts: 3,
    }
}

#[test]
fn director_user_prompt_renders_mode_label_conservative() {
    let builder = InlineContextBuilder::new();
    let state = sample_director_state("Conservative");
    let out = builder.build_for_director(&state, &[], 100_000).unwrap();
    let user = out.first_user_text().expect("director state user message");
    assert!(
        user.contains("**Director mode:** Conservative"),
        "user prompt must surface Conservative mode label: {user}"
    );
}

#[test]
fn director_user_prompt_renders_mode_label_needs_operator() {
    let builder = InlineContextBuilder::new();
    let state = sample_director_state("NeedsOperator");
    let out = builder.build_for_director(&state, &[], 100_000).unwrap();
    let user = out.first_user_text().expect("director state user message");
    assert!(
        user.contains("**Director mode:** NeedsOperator"),
        "user prompt must surface NeedsOperator mode label: {user}"
    );
}

#[test]
fn director_user_prompt_renders_operator_notes_section() {
    let builder = InlineContextBuilder::new();
    let mut state = sample_director_state("Normal");
    state.operator_notes = vec![
        "try the failing test in verbose mode".to_string(),
        "check the env var FOO".to_string(),
    ];
    let out = builder.build_for_director(&state, &[], 100_000).unwrap();
    let user = out.first_user_text().expect("director state user message");
    assert!(
        user.contains("### Operator Notes"),
        "user prompt must include the Operator Notes section header when notes present: {user}"
    );
    assert!(
        user.contains("try the failing test in verbose mode"),
        "user prompt must include first note body: {user}"
    );
    assert!(
        user.contains("check the env var FOO"),
        "user prompt must include second note body: {user}"
    );
}

#[test]
fn director_user_prompt_omits_operator_notes_section_when_empty() {
    let builder = InlineContextBuilder::new();
    let state = sample_director_state("Normal");
    assert!(state.operator_notes.is_empty(), "sample state defaults to no notes");
    let out = builder.build_for_director(&state, &[], 100_000).unwrap();
    let user = out.first_user_text().expect("director state user message");
    assert!(
        !user.contains("### Operator Notes"),
        "user prompt must NOT include the Operator Notes section when the vector is empty: {user}"
    );
}

#[test]
fn director_user_prompt_empty_mode_defaults_to_normal() {
    let builder = InlineContextBuilder::new();
    let state = sample_director_state("");
    let out = builder.build_for_director(&state, &[], 100_000).unwrap();
    let user = out.first_user_text().expect("director state user message");
    assert!(
        user.contains("**Director mode:** Normal"),
        "empty mode must render as Normal: {user}"
    );
}

#[test]
fn director_system_prompt_byte_stable_across_modes() {
    // Cache-locality regression guard end-to-end: rendering the
    // Director context with different modes must produce identical
    // system prompts. The mode label lives in the USER prompt only.
    let builder = InlineContextBuilder::new();
    let normal = builder
        .build_for_director(&sample_director_state("Normal"), &[], 100_000)
        .unwrap();
    let conservative = builder
        .build_for_director(&sample_director_state("Conservative"), &[], 100_000)
        .unwrap();
    let needs_op = builder
        .build_for_director(&sample_director_state("NeedsOperator"), &[], 100_000)
        .unwrap();
    assert_eq!(
        normal.system_prompt, conservative.system_prompt,
        "system prompt must be byte-stable across Normal/Conservative; the Anthropic ephemeral cache invalidates on any difference"
    );
    assert_eq!(
        normal.system_prompt, needs_op.system_prompt,
        "system prompt must be byte-stable across Normal/NeedsOperator"
    );
}

#[test]
fn token_estimate_scales_with_size() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let small = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();
    let entry = IterationSummary {
        iteration: 1,
        actions_summary: "long detail ".repeat(100),
    };
    let history = vec![entry.clone(), entry.clone(), entry];
    let with_history = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &history, &StateSummary::default(), 4)
        .unwrap();
    assert!(
        with_history.token_estimate > small.token_estimate,
        "history should grow token estimate: small={}, with_history={}",
        small.token_estimate,
        with_history.token_estimate
    );
}
