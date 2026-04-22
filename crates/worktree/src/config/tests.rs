use super::*;

#[test]
fn default_policy_is_on_work_terminal() {
    assert_eq!(AttemptCleanupPolicy::default(), AttemptCleanupPolicy::OnWorkTerminal);
}

#[test]
fn policy_kebab_serialization() {
    // serde_yaml round-trip per variant — verifies kebab-case on the wire.
    for (variant, wire) in [
        (AttemptCleanupPolicy::Immediate, "immediate"),
        (AttemptCleanupPolicy::OnWorkTerminal, "on-work-terminal"),
        (AttemptCleanupPolicy::OnRunEnd, "on-run-end"),
        (AttemptCleanupPolicy::Never, "never"),
    ] {
        let encoded = serde_yaml::to_string(&variant).expect("encode");
        assert_eq!(encoded.trim(), wire, "variant {variant:?} wire form");

        let decoded: AttemptCleanupPolicy = serde_yaml::from_str(wire).expect("decode");
        assert_eq!(decoded, variant);
    }
}

#[test]
fn config_default_roundtrip() {
    let cfg = WorktreeConfig::default();
    let y = serde_yaml::to_string(&cfg).unwrap();
    let back: WorktreeConfig = serde_yaml::from_str(&y).unwrap();
    assert_eq!(back.cleanup_policy, AttemptCleanupPolicy::OnWorkTerminal);
}

#[test]
fn config_rejects_unknown_fields() {
    let y = "cleanup-policy: immediate\nunknown-field: true\n";
    let result: Result<WorktreeConfig, _> = serde_yaml::from_str(y);
    assert!(result.is_err(), "deny_unknown_fields must reject typos");
}

#[test]
fn config_decodes_kebab_key() {
    let y = "cleanup-policy: on-run-end\n";
    let cfg: WorktreeConfig = serde_yaml::from_str(y).unwrap();
    assert_eq!(cfg.cleanup_policy, AttemptCleanupPolicy::OnRunEnd);
}
