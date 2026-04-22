#![allow(clippy::unwrap_used)]

use std::path::Path;

use domain::{AcceptanceCriteria, PlanId, Work};
use tools::ToolSchema;

use crate::{ContextBuilder, IterationSummary, StateSummary};

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
    assert!(!out.user_message.is_empty());
    assert!(out.token_estimate > 0);
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
    assert!(out.user_message.contains("add --version flag"));
    assert!(out.user_message.contains("cli parses --version"));
    assert!(out.user_message.contains("prints GIT_DESCRIBE"));
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
    assert!(out.user_message.contains("/var/tmp/wt-xyz"));
}

#[test]
fn user_message_includes_iteration_count() {
    let builder = InlineContextBuilder::new();
    let work = sample_work();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 17)
        .unwrap();
    assert!(out.user_message.contains("Iteration: 17"));
}

#[test]
fn user_message_empty_ac_does_not_crash() {
    let builder = InlineContextBuilder::new();
    let mut work = sample_work();
    work.acceptance_criteria = AcceptanceCriteria::default();
    let out = builder
        .build_for_implementer(&work, Path::new("/tmp/wt"), &[], &[], &StateSummary::default(), 1)
        .unwrap();
    assert!(out.user_message.contains("(none specified)"));
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
    assert!(out.user_message.contains("Prior Bundle Was Rejected"));
    assert!(out.user_message.contains("tests failed: test_foo"));
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
    assert!(out.user_message.contains("Iteration 1"));
    assert!(out.user_message.contains("ran bash: ls"));
    assert!(out.user_message.contains("Iteration 2"));
    assert!(out.user_message.contains("wrote file cli.rs"));
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
        !out.user_message.contains(&huge),
        "10k-char summary must not appear in full"
    );
    // But a truncated prefix + marker must appear.
    assert!(
        out.user_message.contains("truncated"),
        "expected truncation marker in: {}",
        &out.user_message[..500.min(out.user_message.len())]
    );
    // Roughly: history body should not be longer than the cap + marker
    // + the surrounding iteration scaffolding; assert we're close to
    // the cap, not still carrying 10k chars.
    assert!(
        out.user_message.len() < 6_000,
        "user_message grew to {} chars; cap should hold",
        out.user_message.len()
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
