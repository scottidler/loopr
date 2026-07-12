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
