use super::*;

#[test]
fn tier_names_resolve_to_configured_models() {
    let tiers = ModelTiers::default();
    assert_eq!(tiers.resolve("primary"), "claude-sonnet-4-6");
    assert_eq!(tiers.resolve("lightweight"), "claude-haiku-4-5");
    assert_eq!(tiers.resolve("advisor"), "claude-opus-4-7");
}

#[test]
fn literal_model_id_passes_through_unchanged() {
    let tiers = ModelTiers::default();
    assert_eq!(tiers.resolve("claude-opus-4-7"), "claude-opus-4-7");
    assert_eq!(tiers.resolve("some-future-model-99"), "some-future-model-99");
}

#[test]
fn custom_table_overrides_resolution() {
    let yaml = "primary: my-primary\nlightweight: my-light\nadvisor: my-advisor\n";
    let tiers: ModelTiers = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(tiers.resolve("primary"), "my-primary");
    assert_eq!(tiers.resolve("advisor"), "my-advisor");
    // A literal still passes through even with a custom table.
    assert_eq!(tiers.resolve("claude-sonnet-4-6"), "claude-sonnet-4-6");
}

#[test]
fn partial_table_keeps_defaults_for_unspecified_tiers() {
    // `default` on the struct means an absent key falls back to the
    // default model, not an error.
    let tiers: ModelTiers = serde_yaml::from_str("primary: only-primary\n").unwrap();
    assert_eq!(tiers.resolve("primary"), "only-primary");
    assert_eq!(tiers.resolve("advisor"), "claude-opus-4-7");
}

#[test]
fn resolve_checked_accepts_known_tier_names() {
    let tiers = ModelTiers::default();
    assert_eq!(tiers.resolve_checked("primary").unwrap(), "claude-sonnet-4-6");
    assert_eq!(tiers.resolve_checked("lightweight").unwrap(), "claude-haiku-4-5");
    assert_eq!(tiers.resolve_checked("advisor").unwrap(), "claude-opus-4-7");
}

#[test]
fn resolve_checked_accepts_a_claude_prefixed_literal() {
    let tiers = ModelTiers::default();
    assert_eq!(tiers.resolve_checked("claude-opus-4-7").unwrap(), "claude-opus-4-7");
}

// Break-to-prove: on the OLD lenient `resolve`, this same typo'd tier
// name silently becomes a literal "model" (see
// `literal_model_id_passes_through_unchanged` above, which asserts
// exactly that pass-through for a non-"claude-" string). `resolve_checked`
// is the new fail-closed seam Phase 13 adds for per-role model config.
#[test]
fn resolve_checked_rejects_an_unknown_tier_name() {
    let tiers = ModelTiers::default();
    let err = tiers.resolve_checked("lightwieght").unwrap_err();
    assert_eq!(err.reference, "lightwieght");
    assert!(err.to_string().contains("lightwieght"));
}
