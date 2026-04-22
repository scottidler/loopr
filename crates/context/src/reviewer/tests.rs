#![allow(clippy::unwrap_used)]

use domain::{AcceptanceCriteria, Bundle, BundleStatus, PlanId, Role, Work, WorkId};

use crate::reviewer::REVIEWER_SYSTEM_PROMPT;
use crate::{ContextBuilder, InlineContextBuilder};

fn sample_work() -> Work {
    let mut w = Work::new(PlanId::new(), "add --version flag".to_string());
    w.acceptance_criteria = AcceptanceCriteria(vec![
        "cli parses --version".to_string(),
        "prints GIT_DESCRIBE output".to_string(),
    ]);
    w
}

fn sample_bundle(work_id: WorkId, head_commit: Option<&str>) -> Bundle {
    let mut b = Bundle::new(
        work_id,
        "loopr/wk-00042-1".to_string(),
        vec!["added --version handling".to_string()],
    );
    b.paths = vec!["src/cli.rs".to_string(), "src/main.rs".to_string()];
    b.head_commit = head_commit.map(str::to_string);
    b.loc_changed = Some(17);
    // Advance the Bundle to `Triaged` so it matches a real run; the
    // renderer does not gate on status, but the test mirrors the
    // production call shape.
    b.transition(BundleStatus::Triaged, Role::Coordinator).unwrap();
    b
}

#[test]
fn build_returns_non_empty_assembled_context() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff --git a/foo b/foo\n-x\n+y\n", None)
        .unwrap();
    assert!(!out.system_prompt.is_empty());
    assert!(!out.user_message.is_empty());
    assert!(out.token_estimate > 0);
}

#[test]
fn user_message_renders_work_title_and_ac() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff body", None)
        .unwrap();
    assert!(out.user_message.contains("add --version flag"));
    assert!(out.user_message.contains("cli parses --version"));
    assert!(out.user_message.contains("prints GIT_DESCRIBE"));
}

#[test]
fn user_message_renders_bundle_metadata() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef1234"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff body", None)
        .unwrap();
    assert!(out.user_message.contains("deadbeef1234"), "got: {}", out.user_message);
    assert!(out.user_message.contains("loopr/wk-00042-1"));
    assert!(out.user_message.contains("src/cli.rs, src/main.rs"));
    assert!(out.user_message.contains("loc_changed:    17"));
    assert!(out.user_message.contains("force_proposed: false"));
}

#[test]
fn force_proposed_bundle_is_surfaced() {
    let work = sample_work();
    let mut bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    bundle.force_proposed = true;
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff body", None)
        .unwrap();
    assert!(out.user_message.contains("force_proposed: true"));
}

#[test]
fn diff_section_rendered_when_noop_files_is_none() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let diff = "diff --git a/foo b/foo\n@@ -1,1 +1,1 @@\n-old\n+new\n";
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, diff, None)
        .unwrap();
    assert!(out.user_message.contains("### Diff"));
    assert!(out.user_message.contains("diff --git a/foo b/foo"));
    assert!(!out.user_message.contains("### File Contents"));
}

#[test]
fn file_contents_section_rendered_when_noop_files_is_some() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), None);
    let files = vec![
        ("README.md".to_string(), "# project\n\nsome content\n".to_string()),
        ("src/main.rs".to_string(), "fn main() {}\n".to_string()),
    ];
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "", Some(&files))
        .unwrap();
    assert!(out.user_message.contains("### File Contents"));
    assert!(out.user_message.contains("#### README.md"));
    assert!(out.user_message.contains("# project"));
    assert!(out.user_message.contains("#### src/main.rs"));
    assert!(out.user_message.contains("fn main()"));
    assert!(!out.user_message.contains("### Diff"));
}

#[test]
fn empty_diff_with_head_commit_renders_structural_corruption_marker() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "", None)
        .unwrap();
    assert!(
        out.user_message
            .contains("(empty patch body: structural corruption; see system prompt)"),
        "got: {}",
        out.user_message
    );
}

#[test]
fn empty_diff_without_head_commit_renders_noop_marker() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), None);
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "", None)
        .unwrap();
    assert!(out.user_message.contains("(no diff: noop bundle without head_commit)"));
}

#[test]
fn pre_truncated_diff_passes_through_verbatim() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let truncated =
        "diff --git a/x b/x\n+stuff\n[... diff truncated; original 200000 bytes, shown first 65536 bytes]\n";
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, truncated, None)
        .unwrap();
    assert!(
        out.user_message.contains("[... diff truncated; original 200000 bytes"),
        "truncation marker must pass through verbatim"
    );
}

#[test]
fn bundle_claims_rendered() {
    let work = sample_work();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff", None)
        .unwrap();
    assert!(out.user_message.contains("### Claims"));
    assert!(out.user_message.contains("added --version handling"));
}

#[test]
fn empty_ac_does_not_crash() {
    let mut work = sample_work();
    work.acceptance_criteria = AcceptanceCriteria::default();
    let bundle = sample_bundle(work.id.clone(), Some("deadbeef"));
    let out = InlineContextBuilder::new()
        .build_for_reviewer(&bundle, &work, "diff", None)
        .unwrap();
    assert!(out.user_message.contains("(none specified)"));
}

#[test]
fn system_prompt_contains_tagged_schema_marker() {
    assert!(REVIEWER_SYSTEM_PROMPT.contains(r#""kind": "accept""#));
    assert!(REVIEWER_SYSTEM_PROMPT.contains(r#""kind": "change_requested""#));
    assert!(REVIEWER_SYSTEM_PROMPT.contains(r#""kind": "reject""#));
}

#[test]
fn system_prompt_contains_reasons_requirement() {
    assert!(REVIEWER_SYSTEM_PROMPT.contains("reasons"));
    assert!(REVIEWER_SYSTEM_PROMPT.contains("change_requested"));
    assert!(REVIEWER_SYSTEM_PROMPT.contains("at least one issue"));
}

#[test]
fn system_prompt_mentions_force_proposed_guidance() {
    assert!(REVIEWER_SYSTEM_PROMPT.contains("force_proposed"));
    assert!(REVIEWER_SYSTEM_PROMPT.contains("heightened skepticism"));
}

#[test]
fn system_prompt_mentions_empty_patch_body_structural_corruption() {
    assert!(REVIEWER_SYSTEM_PROMPT.contains("empty patch body"));
    assert!(REVIEWER_SYSTEM_PROMPT.contains("structural"));
}

#[test]
fn system_prompt_mentions_truncation_awareness() {
    assert!(REVIEWER_SYSTEM_PROMPT.contains("truncated"));
    assert!(REVIEWER_SYSTEM_PROMPT.contains("visible portion"));
}
