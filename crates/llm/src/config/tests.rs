#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn default_values_match_scope_memo() {
    let cfg = LlmConfig::default();
    assert_eq!(cfg.model, "claude-sonnet-4-6");
    assert_eq!(cfg.max_tokens, 8192);
    assert!((cfg.temperature - 0.3).abs() < f32::EPSILON);
    assert_eq!(cfg.api_key_env, "ANTHROPIC_API_KEY");
    assert_eq!(cfg.api_base_url, "https://api.anthropic.com");
}

#[test]
fn kebab_case_wire_form_roundtrips() {
    let yaml = "\
model: claude-sonnet-4-6
max-tokens: 8192
temperature: 0.3
api-key-env: ANTHROPIC_API_KEY
api-base-url: https://api.anthropic.com
";
    let cfg: LlmConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.model, "claude-sonnet-4-6");
    assert_eq!(cfg.max_tokens, 8192);
    assert_eq!(cfg.api_key_env, "ANTHROPIC_API_KEY");
    assert_eq!(cfg.api_base_url, "https://api.anthropic.com");

    let written = serde_yaml::to_string(&cfg).unwrap();
    assert!(
        written.contains("max-tokens:"),
        "expected kebab-case on disk: {written}"
    );
    assert!(
        written.contains("api-key-env:"),
        "expected kebab-case on disk: {written}"
    );
    assert!(
        written.contains("api-base-url:"),
        "expected kebab-case on disk: {written}"
    );
}

#[test]
fn deny_unknown_fields_rejects_typo() {
    let yaml = "\
model: claude-sonnet-4-6
max-tokens: 8192
temperature: 0.3
api-key-env: ANTHROPIC_API_KEY
api-base-url: https://api.anthropic.com
max-token: 1
";
    let err = serde_yaml::from_str::<LlmConfig>(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown field") || msg.contains("max-token"),
        "expected unknown-field rejection: {msg}"
    );
}

#[test]
fn missing_required_field_fails_loudly() {
    let yaml = "\
max-tokens: 8192
temperature: 0.3
api-key-env: ANTHROPIC_API_KEY
api-base-url: https://api.anthropic.com
";
    let err = serde_yaml::from_str::<LlmConfig>(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("model"),
        "expected missing-field error to mention `model`: {msg}"
    );
}

#[test]
fn local_api_base_url_roundtrips() {
    let yaml = "\
model: claude-sonnet-4-6
max-tokens: 8192
temperature: 0.3
api-key-env: ANTHROPIC_API_KEY
api-base-url: http://localhost:8080
";
    let cfg: LlmConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.api_base_url, "http://localhost:8080");
}
