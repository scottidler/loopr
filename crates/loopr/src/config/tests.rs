use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tempfile::TempDir;
use worktree::AttemptCleanupPolicy;

use super::{Config, PLACEHOLDER_API_KEY, WORKTREE_CLEANUP_ENV, resolve_api_key};

// All Config::load calls touch process-global env (the generic
// LOOPR_*__* pass + LOOPR_WORKTREE_CLEANUP_POLICY) AND read the XDG user
// config under $XDG_CONFIG_HOME. Both are process-global, so every
// Config::load test must serialize behind this lock AND isolate
// $XDG_CONFIG_HOME to a private empty tempdir (else it would read the
// developer's real ~/.config/loopr/loopr.yml). `load_guard()` does both.
static LOAD_MUTEX: Mutex<()> = Mutex::new(());

/// Serializes Config::load tests and points $XDG_CONFIG_HOME at a private
/// empty tempdir so the XDG layer reads nothing (or only what the test
/// writes via `xdg_config_path`). Restores the prior value on drop.
struct LoadGuard {
    _lock: MutexGuard<'static, ()>,
    xdg_tmp: TempDir,
    prior_xdg: Option<String>,
}

impl LoadGuard {
    /// Path where a test can plant an XDG-layer config file.
    fn xdg_config_path(&self) -> PathBuf {
        self.xdg_tmp.path().join("loopr").join("loopr.yml")
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        match &self.prior_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }
    }
}

fn load_guard() -> LoadGuard {
    let lock = LOAD_MUTEX.lock().unwrap();
    let prior_xdg = std::env::var("XDG_CONFIG_HOME").ok();
    let xdg_tmp = TempDir::new().expect("xdg tempdir");
    unsafe { std::env::set_var("XDG_CONFIG_HOME", xdg_tmp.path()) };
    LoadGuard {
        _lock: lock,
        xdg_tmp,
        prior_xdg,
    }
}

#[test]
fn config_load_missing_file_returns_default() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6");
    assert_eq!(cfg.llm.api_base_url, "https://api.anthropic.com");
    assert_eq!(cfg.llm.api_key_env, "ANTHROPIC_API_KEY");
}

#[test]
fn config_load_parses_llm_section() {
    let _g = load_guard();
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
    let _g = load_guard();
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
    let _g = load_guard();
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
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.tools.sandbox, tools::SandboxMode::Required);
}

#[test]
fn config_worktree_default_is_on_work_terminal() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.worktree.cleanup_policy, AttemptCleanupPolicy::OnWorkTerminal);
}

#[test]
fn config_worktree_parses_from_yml() {
    let _g = load_guard();
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
    let _g = load_guard();
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
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.transport.client_request_secs, 10);
    assert_eq!(cfg.transport.server_idle_secs, 15);
    assert_eq!(cfg.transport.server_write_secs, 10);
    assert_eq!(cfg.transport.daemon_startup_secs, 60);
}

#[test]
fn config_transport_round_trip_yaml() {
    let _g = load_guard();
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
    let _g = load_guard();
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
fn model_tiers_default_resolves_role_models_to_concrete_ids() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    // Defaults are already concrete; resolution is a no-op identity.
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6");
    assert_eq!(cfg.agents.director.model, "claude-opus-4-7");
}

#[test]
fn model_tiers_resolve_role_references_after_load() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    // The llm model and the director model both reference tiers by name;
    // after load they must be rewritten to the table's concrete ids.
    let yml = r#"
models:
  primary: claude-sonnet-4-6
  lightweight: claude-haiku-4-5
  advisor: claude-opus-4-7
llm:
  model: primary
  max-tokens: 8192
  api-key-env: ANTHROPIC_API_KEY
  api-base-url: https://api.anthropic.com
agents:
  director:
    model: advisor
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6", "primary tier -> concrete");
    assert_eq!(cfg.agents.director.model, "claude-opus-4-7", "advisor tier -> concrete");
}

#[test]
fn model_tiers_literal_model_id_survives_resolution() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
llm:
  model: claude-opus-4-7
  max-tokens: 8192
  api-key-env: ANTHROPIC_API_KEY
  api-base-url: https://api.anthropic.com
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.llm.model, "claude-opus-4-7", "literal id passes through");
}

// Phase 13 of `docs/design/2026-07-11-verified-swarm.md`: per-role model
// routing. Success criterion: an unconfigured run's implementer/reviewer
// calls carry the SAME model id as before this phase (asserted, not
// eyeballed) -- the cheap-worker split is opt-in, never a silent default
// change.
#[test]
fn model_tiers_unconfigured_implementer_and_reviewer_match_llm_model() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(
        cfg.agents.implementer.model, cfg.llm.model,
        "unconfigured implementer.model must resolve to the same concrete id as llm.model"
    );
    assert_eq!(
        cfg.agents.reviewer.model, cfg.llm.model,
        "unconfigured reviewer.model must resolve to the same concrete id as llm.model"
    );
    assert_eq!(cfg.agents.implementer.model, "claude-sonnet-4-6");
    assert_eq!(cfg.agents.reviewer.model, "claude-sonnet-4-6");
}

#[test]
fn model_tiers_opt_in_split_routes_implementer_and_reviewer_independently() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
agents:
  implementer:
    model: lightweight
  reviewer:
    model: primary
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(
        cfg.agents.implementer.model, "claude-haiku-4-5",
        "implementer opted into lightweight"
    );
    assert_eq!(
        cfg.agents.reviewer.model, "claude-sonnet-4-6",
        "reviewer stays on primary"
    );
    assert_ne!(cfg.agents.implementer.model, cfg.agents.reviewer.model);
}

#[test]
fn model_tiers_unknown_tier_name_fails_config_load_with_typed_error() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = "agents:\n  implementer:\n    model: lightwieght\n";
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let err = Config::load(dir.path()).expect_err("unknown tier name must fail config load");
    let msg = err.to_string();
    assert!(msg.contains("agents.implementer.model"), "got: {msg}");
    assert!(msg.contains("lightwieght"), "got: {msg}");
}

#[test]
fn model_tiers_unknown_tier_name_on_reviewer_also_fails_config_load() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = "agents:\n  reviewer:\n    model: bogus-tier\n";
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");

    let err = Config::load(dir.path()).expect_err("unknown tier name must fail config load");
    let msg = err.to_string();
    assert!(msg.contains("agents.reviewer.model"), "got: {msg}");
    assert!(msg.contains("bogus-tier"), "got: {msg}");
}

#[test]
fn budgets_default_to_unlimited() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.budgets.per_run_cost_usd, None);
    assert_eq!(cfg.budgets.per_work_cost_usd, None);
}

#[test]
fn budgets_parse_from_yaml() {
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    let yml = r#"
budgets:
  per-run-cost-usd: 12.50
  per-work-cost-usd: 1.25
"#;
    std::fs::write(loopr_dir.join("config.yml"), yml).expect("write");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.budgets.per_run_cost_usd, Some(12.50));
    assert_eq!(cfg.budgets.per_work_cost_usd, Some(1.25));
}

#[test]
fn env_invalid_value_errors_cleanly() {
    let _g = load_guard();
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

#[test]
fn invalid_legacy_xdg_layer_is_skipped_not_fatal() {
    // The XDG user config is shared across loopr versions; a leftover
    // v3/v4 file (unknown keys under deny_unknown_fields) must be warned-
    // and-skipped, NOT brick the load. Defaults stand.
    let g = load_guard();
    let xdg_path = g.xdg_config_path();
    std::fs::create_dir_all(xdg_path.parent().unwrap()).expect("mkdir xdg");
    std::fs::write(
        &xdg_path,
        "debug: true\nagents:\n  enabled: true\nvalidator:\n  enabled: false\n",
    )
    .expect("write legacy xdg");

    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("legacy XDG config must be skipped, not fatal");
    assert_eq!(cfg.llm.model, "claude-sonnet-4-6", "defaults stand after skipping XDG");
    assert_eq!(cfg.transport.client_request_secs, 10);
}

#[test]
fn invalid_target_file_is_still_fatal() {
    // A target .loopr/config.yml is v5-owned (written by `loopr init`), so
    // it stays strict: an unknown field is a hard error, unlike the shared
    // XDG layer.
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    std::fs::write(loopr_dir.join("config.yml"), "debug: true\n").expect("write");
    assert!(
        Config::load(dir.path()).is_err(),
        "an unknown field in the v5-owned target config must be fatal"
    );
}

#[test]
fn xdg_layer_applied_when_no_target_file() {
    // A key set only in the XDG user layer reaches the loaded config even
    // when the target has no .loopr/config.yml.
    let g = load_guard();
    let xdg_path = g.xdg_config_path();
    std::fs::create_dir_all(xdg_path.parent().unwrap()).expect("mkdir xdg");
    std::fs::write(&xdg_path, "transport:\n  client-request-secs: 42\n").expect("write xdg");

    let dir = TempDir::new().expect("tempdir");
    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.transport.client_request_secs, 42, "XDG layer must apply");
}

#[test]
fn target_layer_deep_merges_over_xdg() {
    // XDG sets two transport keys; the target overrides one and adds a
    // budgets section. Deep-merge must keep the XDG-only key while the
    // target wins on the shared key.
    let g = load_guard();
    let xdg_path = g.xdg_config_path();
    std::fs::create_dir_all(xdg_path.parent().unwrap()).expect("mkdir xdg");
    std::fs::write(
        &xdg_path,
        "transport:\n  client-request-secs: 11\n  server-idle-secs: 99\n",
    )
    .expect("write xdg");

    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    std::fs::write(
        loopr_dir.join("config.yml"),
        "transport:\n  client-request-secs: 22\nbudgets:\n  per-run-cost-usd: 5.0\n",
    )
    .expect("write target");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(cfg.transport.client_request_secs, 22, "target overrides XDG");
    assert_eq!(cfg.transport.server_idle_secs, 99, "XDG-only key survives merge");
    assert_eq!(cfg.budgets.per_run_cost_usd, Some(5.0), "target-only section applies");
}

#[test]
fn generic_env_override_beats_files() {
    // LOOPR_<SECTION>__<KEY> overrides both file layers, with `__` for
    // nesting and `_`->`-` within a segment.
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    std::fs::write(loopr_dir.join("config.yml"), "transport:\n  client-request-secs: 10\n").expect("write");

    unsafe { std::env::set_var("LOOPR_TRANSPORT__CLIENT_REQUEST_SECS", "77") };
    unsafe { std::env::set_var("LOOPR_BUDGETS__PER_RUN_COST_USD", "3.5") };
    let result = Config::load(dir.path());
    unsafe { std::env::remove_var("LOOPR_TRANSPORT__CLIENT_REQUEST_SECS") };
    unsafe { std::env::remove_var("LOOPR_BUDGETS__PER_RUN_COST_USD") };

    let cfg = result.expect("load");
    assert_eq!(cfg.transport.client_request_secs, 77, "env overrides file");
    assert_eq!(cfg.budgets.per_run_cost_usd, Some(3.5), "env sets nested numeric");
}

#[test]
fn generic_env_override_works_with_no_files() {
    // An env override alone (no XDG, no target file) still produces a
    // valid config (other fields fall back to defaults).
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");

    unsafe { std::env::set_var("LOOPR_TRANSPORT__SERVER_WRITE_SECS", "4") };
    let result = Config::load(dir.path());
    unsafe { std::env::remove_var("LOOPR_TRANSPORT__SERVER_WRITE_SECS") };

    let cfg = result.expect("load");
    assert_eq!(cfg.transport.server_write_secs, 4);
    // Untouched field keeps its default.
    assert_eq!(cfg.transport.client_request_secs, 10);
}

#[test]
fn comment_only_target_file_does_not_clobber_xdg_layer() {
    // Phase 12: `loopr init` seeds a documentation-only (all-comment)
    // config.yml. That file parses to `Value::Null`, and a naive
    // deep-merge would previously replace the whole merged tree with
    // Null — silently erasing a real XDG-layer config the moment a
    // fresh target is `init`ed. The Null overlay must be a no-op.
    let g = load_guard();
    let xdg_path = g.xdg_config_path();
    std::fs::create_dir_all(xdg_path.parent().unwrap()).expect("mkdir xdg");
    std::fs::write(&xdg_path, "transport:\n  client-request-secs: 42\n").expect("write xdg");

    let dir = TempDir::new().expect("tempdir");
    let loopr_dir = dir.path().join(".loopr");
    std::fs::create_dir_all(&loopr_dir).expect("mkdir .loopr");
    std::fs::write(
        loopr_dir.join("config.yml"),
        "# nothing configured yet\n# see the docs\n",
    )
    .expect("write target");

    let cfg = Config::load(dir.path()).expect("load");
    assert_eq!(
        cfg.transport.client_request_secs, 42,
        "an all-comment target file must not erase the XDG layer"
    );
}

#[test]
fn loopr_env_without_nesting_marker_is_ignored() {
    // A LOOPR_* var without the `__` marker (e.g. LOOPR_TARGET) is not a
    // config-field override and must not corrupt the config or trip
    // deny_unknown_fields.
    let _g = load_guard();
    let dir = TempDir::new().expect("tempdir");

    unsafe { std::env::set_var("LOOPR_TARGET", "/some/where") };
    let result = Config::load(dir.path());
    unsafe { std::env::remove_var("LOOPR_TARGET") };

    let cfg = result.expect("LOOPR_TARGET must be ignored by the generic pass");
    assert_eq!(cfg.transport.client_request_secs, 10, "defaults intact");
}
