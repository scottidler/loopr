use std::sync::Mutex;

use tempfile::TempDir;
use worktree::AttemptCleanupPolicy;

use super::{Config, PLACEHOLDER_API_KEY, WORKTREE_CLEANUP_ENV, resolve_api_key};

// All Config::load calls read LOOPR_WORKTREE_CLEANUP_POLICY from the
// process env. Since env vars are process-global, any test that calls
// Config::load must hold this lock so it doesn't see a value injected
// by a concurrently-running env-mutation test.
static LOAD_MUTEX: Mutex<()> = Mutex::new(());

#[test]
fn config_load_missing_file_returns_default() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6");
    assert_eq!(cfg.llm.api_base_url, "https://api.anthropic.com");
    assert_eq!(cfg.llm.api_key_env, "ANTHROPIC_API_KEY");
}

#[test]
fn config_load_parses_llm_section() {
    let _g = LOAD_MUTEX.lock().unwrap();
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
    assert_eq!(cfg.llm.temperature, Some(0.5));
    assert_eq!(cfg.llm.api_key_env, "MY_KEY");
    assert_eq!(cfg.llm.api_base_url, "https://example.com");
}

#[test]
fn config_load_unknown_field_rejected() {
    let _g = LOAD_MUTEX.lock().unwrap();
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
    // Uses a unique var name that never conflicts with the cleanup-policy env
    // var; no LOAD_MUTEX needed (does not call Config::load).
    let var = "LOOPR_TEST_CUSTOM_KEY_VAR";
    unsafe { std::env::set_var(var, "sk-real-value") };
    let llm = llm::LlmConfig {
        api_key_env: var.to_string(),
        ..llm::LlmConfig::default()
    };
    let key = resolve_api_key(&llm);
    assert_eq!(key, "sk-real-value");
    unsafe { std::env::remove_var(var) };
}

#[test]
fn resolve_api_key_falls_back_to_placeholder_when_unset() {
    let var = "LOOPR_TEST_DEFINITELY_UNSET_VAR";
    unsafe { std::env::remove_var(var) };
    let llm = llm::LlmConfig {
        api_key_env: var.to_string(),
        ..llm::LlmConfig::default()
    };
    let key = resolve_api_key(&llm);
    assert_eq!(key, PLACEHOLDER_API_KEY);
}

#[test]
fn config_load_parses_tools_section() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
tools:
  sandbox: preferred
  path-deny-patterns:
    - "secret.txt"
  bash-denylist-extend:
    - tokens: ["./deploy.sh"]
      reason: "deploys are a human action"
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.tools.sandbox, tools::SandboxMode::Preferred);
    assert_eq!(cfg.tools.path_deny_patterns, vec!["secret.txt"]);
    assert_eq!(cfg.tools.bash_denylist_extend.len(), 1);
    assert_eq!(cfg.tools.bash_denylist_extend[0].tokens, vec!["./deploy.sh"]);
}

#[test]
fn config_tools_default_is_required_sandbox() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.tools.sandbox, tools::SandboxMode::Required);
}

#[test]
fn config_worktree_default_is_on_work_terminal() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.worktree.cleanup_policy, AttemptCleanupPolicy::OnWorkTerminal);
}

#[test]
fn config_worktree_parses_from_yml() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir");
    std::fs::write(
        loopr_dir.join("config.yml"),
        "worktree:\n  cleanup-policy: on-run-end\n",
    )
    .expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.worktree.cleanup_policy, AttemptCleanupPolicy::OnRunEnd);
}

#[test]
fn env_overrides_config_for_worktree_cleanup() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir");
    std::fs::write(loopr_dir.join("config.yml"), "worktree:\n  cleanup-policy: immediate\n").expect("write");

    unsafe { std::env::set_var(WORKTREE_CLEANUP_ENV, "never") };
    let result = Config::load(dir.path());
    unsafe { std::env::remove_var(WORKTREE_CLEANUP_ENV) };

    let cfg = result.expect("load");
    assert_eq!(
        cfg.worktree.cleanup_policy,
        AttemptCleanupPolicy::Never,
        "ENV must override config"
    );
}

#[test]
fn config_transport_default_values() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.transport.client_request_secs, 10);
    assert_eq!(cfg.transport.server_idle_secs, 15);
    assert_eq!(cfg.transport.server_write_secs, 10);
    assert_eq!(cfg.transport.daemon_startup_secs, 60);
}

#[test]
fn config_transport_round_trip_yaml() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir");
    let yml = "
transport:
  client-request-secs: 5
  server-idle-secs: 7
  server-write-secs: 3
  daemon-startup-secs: 30
";
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.transport.client_request_secs, 5);
    assert_eq!(cfg.transport.server_idle_secs, 7);
    assert_eq!(cfg.transport.server_write_secs, 3);
    assert_eq!(cfg.transport.daemon_startup_secs, 30);

    let serialized = serde_yaml::to_string(&cfg.transport).expect("serialize");
    assert!(serialized.contains("client-request-secs: 5"));
    assert!(serialized.contains("server-idle-secs: 7"));
    assert!(serialized.contains("server-write-secs: 3"));
    assert!(serialized.contains("daemon-startup-secs: 30"));
}

#[test]
fn config_transport_unknown_field_rejected() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir");
    let yml = "
transport:
  client-request-secs: 10
  bogus-field: 99
";
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");
    let err = Config::load(dir.path()).expect_err("unknown field rejected");
    let msg = err.to_string();
    assert!(msg.contains("bogus-field") || msg.contains("unknown"), "got: {msg}");
}

#[test]
fn env_invalid_value_errors_cleanly() {
    let _g = LOAD_MUTEX.lock().unwrap();
    let dir = TempDir::new().expect("tempdir");

    unsafe { std::env::set_var(WORKTREE_CLEANUP_ENV, "not-a-valid-policy") };
    let result = Config::load(dir.path());
    unsafe { std::env::remove_var(WORKTREE_CLEANUP_ENV) };

    assert!(
        result.is_err(),
        "invalid ENV value should produce a DaemonStartup error"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("LOOPR_WORKTREE_CLEANUP_POLICY"),
        "error must name the env var"
    );
}
