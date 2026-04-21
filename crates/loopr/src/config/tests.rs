use tempfile::TempDir;

use super::{Config, PLACEHOLDER_API_KEY, resolve_api_key};

#[test]
fn config_load_missing_file_returns_default() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6");
    assert_eq!(cfg.llm.api_base_url, "https://api.anthropic.com");
    assert_eq!(cfg.llm.api_key_env, "ANTHROPIC_API_KEY");
}

#[test]
fn config_load_parses_llm_section() {
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
llm:
  model: claude-opus-4-1
  max-tokens: 4096
  temperature: 0.5
  api-key-env: MY_KEY
  api-base-url: https://example.com
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-opus-4-1");
    assert_eq!(cfg.llm.max_tokens, 4096);
    assert_eq!(cfg.llm.temperature, 0.5);
    assert_eq!(cfg.llm.api_key_env, "MY_KEY");
    assert_eq!(cfg.llm.api_base_url, "https://example.com");
}

#[test]
fn config_load_unknown_field_rejected() {
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
llm:
  model: claude-sonnet-4-6
  max-tokens: 8192
  temperature: 0.3
  api-key-env: ANTHROPIC_API_KEY
  api-base-url: https://api.anthropic.com
  typo-field: nope
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let err = Config::load(dir.path()).expect_err("unknown field rejected");
    let msg = err.to_string();
    assert!(msg.contains("typo-field") || msg.contains("unknown"), "got: {msg}");
}

#[test]
fn resolve_api_key_uses_env_when_present() {
    // Use a unique env-var name so parallel tests can't collide on
    // `ANTHROPIC_API_KEY`. SAFETY: setting/removing env vars is
    // process-global; within this test we use a unique name and
    // remove it on exit.
    let var = "LOOPR_TEST_CUSTOM_KEY_VAR";
    unsafe {
        std::env::set_var(var, "sk-real-value");
    }
    let llm = llm::LlmConfig {
        api_key_env: var.to_string(),
        ..llm::LlmConfig::default()
    };
    let key = resolve_api_key(&llm);
    assert_eq!(key, "sk-real-value");
    unsafe {
        std::env::remove_var(var);
    }
}

#[test]
fn resolve_api_key_falls_back_to_placeholder_when_unset() {
    let var = "LOOPR_TEST_DEFINITELY_UNSET_VAR";
    // Ensure unset even if a prior run leaked.
    unsafe {
        std::env::remove_var(var);
    }
    let llm = llm::LlmConfig {
        api_key_env: var.to_string(),
        ..llm::LlmConfig::default()
    };
    let key = resolve_api_key(&llm);
    assert_eq!(key, PLACEHOLDER_API_KEY);
}
