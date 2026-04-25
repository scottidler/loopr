//! Canary test for the baked `.pmt` tree: enumerates every template
//! the production code paths depend on by name, asserting each is
//! registered under the baked-only loader. Fails fast if a `.pmt`
//! file is renamed, deleted, or its parent directory restructured
//! without updating the corresponding Rust call sites.
//!
//! This is the lightweight Phase 5 CI guard described in the design
//! doc `docs/design/2026-04-24-prompts-on-disk.md`. A full v4-style
//! placeholder cross-checker (parse `{{var}}` from `.pmt` against
//! Rust render-context struct fields) is not implemented; handlebars
//! strict mode catches missing-variable mismatches at render time
//! (Phase 2 loader test) and the per-template render tests in
//! `implementer/tests.rs`, `reviewer/tests.rs`, and the decomposer
//! prompt tests exercise the production contexts against their
//! templates end-to-end. That coverage subsumes a static checker.

#![allow(clippy::unwrap_used, clippy::type_complexity)]

use context::PromptLoader;
use serde_json::json;

type RequiredTemplate = (&'static str, fn() -> serde_json::Value);

/// Every template name a production caller renders by string. Adding
/// a new caller in any crate REQUIRES adding the corresponding name
/// here so this canary catches a missing baked file at CI time
/// rather than at first runtime invocation.
const REQUIRED_TEMPLATES: &[RequiredTemplate] = &[
    ("agents/implementer/system.pmt", || json!({"tools": []})),
    ("agents/implementer/user.pmt", || {
        json!({
            "work_id": "wk-test",
            "work_title": "test",
            "worktree_path": "/tmp/wt",
            "iteration": 1,
            "acceptance_criteria": [],
            "rejected_bundle_reason": null,
            "prior_iterations": [],
        })
    }),
    ("agents/reviewer/system.pmt", || json!({})),
    ("agents/reviewer/user.pmt", || {
        json!({
            "work_title": "test",
            "work_id": "wk-test",
            "acceptance_criteria": [],
            "bundle": {
                "id": "bd-test",
                "branch_name": "loopr/test",
                "head_commit_display": "(none)",
                "paths_display": "(none)",
                "loc_changed_display": "0",
                "force_proposed": false,
                "claims": []
            },
            "evidence_section": "### Diff\n(empty)\n",
        })
    }),
    ("decompose/work/system.pmt", || json!({"tree": "(empty)"})),
    (
        "decompose/work/user.pmt",
        || json!({"goal": "test", "prev_error": null}),
    ),
];

#[test]
fn every_required_baked_template_renders() {
    let loader = PromptLoader::new(None, None).expect("baked tree must compile");
    for (name, ctx_fn) in REQUIRED_TEMPLATES {
        let ctx = ctx_fn();
        let rendered = loader
            .render(name, &ctx)
            .unwrap_or_else(|e| panic!("baked template {name} failed to render with stub ctx: {e}"));
        assert!(
            !rendered.trim().is_empty(),
            "baked template {name} rendered to empty string"
        );
    }
}

#[test]
fn tools_list_partial_resolves_from_baked() {
    // The implementer system prompt depends on the `tools-list`
    // partial. If the partial were renamed or moved, registration
    // would silently succeed but the system prompt's `{{> tools-list}}`
    // would fail at render time — caught here.
    let loader = PromptLoader::new(None, None).expect("baked tree must compile");
    let out = loader
        .render(
            "agents/implementer/system.pmt",
            &json!({"tools": [{"name": "bash", "description": "Run a bash command"}]}),
        )
        .unwrap();
    assert!(
        out.contains("- bash: Run a bash command"),
        "tools-list partial did not render its body; got:\n{out}"
    );
}
