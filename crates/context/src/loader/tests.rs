use super::*;

use serde::Serialize;
use serde_json::json;
use std::fs;

/// Helper: write a `.pmt` file at `<root>/<rel>`, creating parent dirs.
fn write_pmt(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

#[derive(Serialize)]
struct EmptyCtx {}

#[test]
fn baked_only_renders_implementer_system_prompt() {
    let loader = PromptLoader::new(None, None).unwrap();
    let ctx = json!({"tools": []});
    let out = loader.render("agents/implementer/system.pmt", &ctx).unwrap();
    assert!(out.contains("You are an Implementer agent"));
    assert!(out.contains("(none)"));
}

#[test]
fn baked_only_renders_decompose_work_system_with_tree() {
    let loader = PromptLoader::new(None, None).unwrap();
    let ctx = json!({"tree": "src/main.rs\nCargo.toml"});
    let out = loader.render("decompose/work/system.pmt", &ctx).unwrap();
    assert!(out.contains("software architect"));
    assert!(out.contains("src/main.rs"));
}

#[test]
fn render_missing_template_returns_not_found() {
    let loader = PromptLoader::new(None, None).unwrap();
    let err = loader
        .render("agents/nonexistent/system.pmt", &EmptyCtx {})
        .unwrap_err();
    assert!(matches!(err, PromptError::NotFound { .. }));
}

#[test]
fn user_layer_overrides_baked() {
    let user = tempfile::tempdir().unwrap();
    write_pmt(
        user.path(),
        "agents/implementer/system.pmt",
        "USER LAYER VERSION {{tag}}",
    );
    let loader = PromptLoader::new(None, Some(user.path().to_path_buf())).unwrap();
    let out = loader
        .render("agents/implementer/system.pmt", &json!({"tag": "abc"}))
        .unwrap();
    assert_eq!(out, "USER LAYER VERSION abc");
}

#[test]
fn target_layer_overrides_user_overrides_baked() {
    let user = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    write_pmt(user.path(), "agents/implementer/system.pmt", "FROM USER");
    write_pmt(target.path(), "agents/implementer/system.pmt", "FROM TARGET");
    let loader = PromptLoader::new(Some(target.path().to_path_buf()), Some(user.path().to_path_buf())).unwrap();
    let out = loader.render("agents/implementer/system.pmt", &EmptyCtx {}).unwrap();
    assert_eq!(out, "FROM TARGET");
}

#[test]
fn partial_override_in_user_layer_resolves_for_baked_template() {
    let user = tempfile::tempdir().unwrap();
    write_pmt(user.path(), "partials/tools-list.pmt", "OVERRIDDEN_PARTIAL_MARKER");
    let loader = PromptLoader::new(None, Some(user.path().to_path_buf())).unwrap();
    let out = loader
        .render("agents/implementer/system.pmt", &json!({"tools": []}))
        .unwrap();
    assert!(
        out.contains("OVERRIDDEN_PARTIAL_MARKER"),
        "expected overridden partial marker in rendered output"
    );
}

#[test]
fn strict_mode_rejects_missing_variable() {
    let target = tempfile::tempdir().unwrap();
    write_pmt(target.path(), "agents/strict/test.pmt", "Value: {{value}}");
    let loader = PromptLoader::new(Some(target.path().to_path_buf()), None).unwrap();
    let err = loader.render("agents/strict/test.pmt", &EmptyCtx {}).unwrap_err();
    assert!(matches!(err, PromptError::Render { .. }));
}

#[test]
fn no_html_escape_preserves_angle_brackets() {
    let target = tempfile::tempdir().unwrap();
    write_pmt(target.path(), "agents/escape/test.pmt", "{{snippet}}");
    let loader = PromptLoader::new(Some(target.path().to_path_buf()), None).unwrap();
    let out = loader
        .render("agents/escape/test.pmt", &json!({"snippet": "<a> & <b>"}))
        .unwrap();
    assert_eq!(out, "<a> & <b>");
}

#[test]
fn gitkeep_files_are_skipped() {
    // The baked tree has decompose/plan/.gitkeep — confirm it's not
    // registered as a template.
    let loader = PromptLoader::new(None, None).unwrap();
    let err = loader.render("decompose/plan/.gitkeep", &EmptyCtx {}).unwrap_err();
    assert!(matches!(err, PromptError::NotFound { .. }));
}

#[test]
fn malformed_handlebars_at_construction_returns_parse_error() {
    let target = tempfile::tempdir().unwrap();
    write_pmt(target.path(), "agents/bad/test.pmt", "{{#if unclosed");
    let err = PromptLoader::new(Some(target.path().to_path_buf()), None).unwrap_err();
    assert!(
        matches!(err, PromptError::Parse { .. }),
        "expected Parse error, got {err:?}"
    );
}
