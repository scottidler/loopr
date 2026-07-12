use super::*;

fn config_with_cap(cap: Option<f64>) -> ImplementerConfig {
    ImplementerConfig {
        per_work_cost_cap_usd: cap,
        ..ImplementerConfig::default()
    }
}

#[test]
fn validate_rejects_negative_cap_with_typed_error() {
    // Success criterion: a config with a negative cap fails load with a
    // typed error (not a silent 0-micros cast that escalates every Work).
    let err = config_with_cap(Some(-1.0)).validate().unwrap_err();
    assert_eq!(err, ConfigError::InvalidCostCap(-1.0));
}

#[test]
fn validate_rejects_nan_cap_with_typed_error() {
    let err = config_with_cap(Some(f64::NAN)).validate().unwrap_err();
    // NaN != NaN, so match the variant rather than compare the payload.
    assert!(matches!(err, ConfigError::InvalidCostCap(v) if v.is_nan()));
}

#[test]
fn validate_accepts_none_and_nonnegative_caps() {
    config_with_cap(None).validate().expect("unlimited cap is valid");
    config_with_cap(Some(0.0)).validate().expect("zero cap is valid");
    config_with_cap(Some(2.50)).validate().expect("positive cap is valid");
}

#[test]
fn default_implementer_config_validates() {
    ImplementerConfig::default()
        .validate()
        .expect("the shipped default (None cap) must validate");
}

// Phase 13 (`docs/design/2026-07-11-verified-swarm.md`, per-role model
// routing): the `model` default must be a CONCRETE literal id, not the
// bare tier name `"primary"`. This crate's own test harnesses (and any
// caller that builds these configs directly, bypassing
// `loopr::Config::load`'s tier-resolution pass) rely on the default
// already being a valid, directly-usable model id.
#[test]
fn implementer_config_model_defaults_to_a_concrete_literal_not_a_tier_name() {
    let model = ImplementerConfig::default().model;
    assert_ne!(model, "primary", "default must not be a bare tier name");
    assert!(
        model.starts_with("claude-"),
        "default must be a concrete model id, got {model:?}"
    );
}

#[test]
fn reviewer_config_model_defaults_to_a_concrete_literal_not_a_tier_name() {
    let model = ReviewerConfig::default().model;
    assert_ne!(model, "primary", "default must not be a bare tier name");
    assert!(
        model.starts_with("claude-"),
        "default must be a concrete model id, got {model:?}"
    );
}

#[test]
fn implementer_and_reviewer_default_models_match_llm_defaults_primary_tier() {
    // Behavior-neutral: the literal default must equal what
    // `llm::ModelTiers::default().primary` (and `llm::LlmConfig::default().model`)
    // already resolve to, so an unconfigured target's implementer/reviewer
    // calls carry the same model id as before this phase.
    assert_eq!(ImplementerConfig::default().model, llm::ModelTiers::default().primary);
    assert_eq!(ReviewerConfig::default().model, llm::ModelTiers::default().primary);
}
