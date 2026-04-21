use super::*;

#[test]
fn default_is_required_sandbox_and_empty_vecs() {
    let cfg = ToolsConfig::default();
    assert_eq!(cfg.sandbox, SandboxMode::Required);
    assert!(cfg.path_deny_patterns.is_empty());
    assert!(cfg.bash_denylist_extend.is_empty());
}

#[test]
fn parses_kebab_case_keys() {
    let yaml = r#"
sandbox: preferred
path-deny-patterns:
  - ".env"
  - ".key"
bash-denylist-extend:
  - tokens: ["./deploy.sh"]
    reason: "deploys are a human action"
"#;
    let cfg: ToolsConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.sandbox, SandboxMode::Preferred);
    assert_eq!(cfg.path_deny_patterns, vec![".env", ".key"]);
    assert_eq!(cfg.bash_denylist_extend.len(), 1);
    assert_eq!(cfg.bash_denylist_extend[0].tokens, vec!["./deploy.sh"]);
    assert_eq!(cfg.bash_denylist_extend[0].reason, "deploys are a human action");
}

#[test]
fn rejects_snake_case_keys() {
    let yaml = r#"
path_deny_patterns:
  - ".env"
"#;
    let err = serde_yaml::from_str::<ToolsConfig>(yaml);
    assert!(err.is_err(), "snake_case keys must fail parsing");
}

#[test]
fn rejects_unknown_top_level_key() {
    let yaml = r#"
sandbox: off
mystery-key: 42
"#;
    let err = serde_yaml::from_str::<ToolsConfig>(yaml);
    assert!(err.is_err(), "unknown keys must fail parsing");
}

#[test]
fn deny_entry_rejects_unknown_key() {
    let yaml = r#"
tokens: ["foo"]
reason: "bar"
extra: "nope"
"#;
    let err = serde_yaml::from_str::<DenyEntryConfig>(yaml);
    assert!(err.is_err(), "unknown keys in deny entry must fail parsing");
}

#[test]
fn deny_entry_requires_tokens_and_reason() {
    let missing_reason = r#"
tokens: ["foo"]
"#;
    assert!(serde_yaml::from_str::<DenyEntryConfig>(missing_reason).is_err());

    let missing_tokens = r#"
reason: "foo"
"#;
    assert!(serde_yaml::from_str::<DenyEntryConfig>(missing_tokens).is_err());
}

#[test]
fn no_replace_key_accepted_at_top_level() {
    let yaml = r#"
bash-denylist-replace:
  - tokens: ["foo"]
    reason: "bar"
"#;
    let err = serde_yaml::from_str::<ToolsConfig>(yaml);
    assert!(
        err.is_err(),
        "'replace' key is structurally impossible — only extend is accepted"
    );
}
