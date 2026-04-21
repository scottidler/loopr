use super::*;

#[test]
fn provider_prefixes_are_denied() {
    assert!(is_denied("LOOPR_DEBUG"));
    assert!(is_denied("ANTHROPIC_API_KEY"));
    assert!(is_denied("AWS_SECRET_ACCESS_KEY"));
    assert!(is_denied("GITHUB_TOKEN"));
    assert!(is_denied("GOOGLE_APPLICATION_CREDENTIALS"));
    assert!(is_denied("GCP_PROJECT"));
    assert!(is_denied("AZURE_CLIENT_ID"));
    assert!(is_denied("OPENAI_API_KEY"));
    assert!(is_denied("GEMINI_API_KEY"));
}

#[test]
fn secret_suffixes_are_denied() {
    assert!(is_denied("SLACK_BOT_TOKEN"));
    assert!(is_denied("STRIPE_API_KEY"));
    assert!(is_denied("DEPLOY_SECRET"));
    assert!(is_denied("DB_PASSWORD"));
    assert!(is_denied("OAUTH_CREDENTIALS"));
    assert!(is_denied("MY_AUTH"));
}

#[test]
fn ssh_auth_sock_passes() {
    // _SOCK is not in the suffix list; the _AUTH check requires the string
    // to END in _AUTH. SSH_AUTH_SOCK ends in _SOCK.
    assert!(!is_denied("SSH_AUTH_SOCK"));
}

#[test]
fn pass_substrings_pass() {
    // R3 dropped *_PASS to avoid false-positives. Only *_PASSWORD denies.
    assert!(!is_denied("MULTIPASS_FOO"));
    assert!(!is_denied("BYPASS_CACHE"));
    assert!(!is_denied("LOWPASS_FILTER"));
    assert!(!is_denied("CLI_PASS_ARGS"));
}

#[test]
fn ordinary_vars_pass() {
    assert!(!is_denied("HOME"));
    assert!(!is_denied("PATH"));
    assert!(!is_denied("XDG_CONFIG_HOME"));
    assert!(!is_denied("CARGO_HOME"));
    assert!(!is_denied("RUSTUP_HOME"));
    assert!(!is_denied("LANG"));
    assert!(!is_denied("TERM"));
    assert!(!is_denied("USER"));
}

#[test]
fn prefix_requires_underscore_boundary() {
    // A prefix match is "starts_with('LOOPR_')", not "starts_with('LOOPR')".
    // LOOPRA_FOO does not match.
    assert!(!is_denied("LOOPRA_FOO"));
    assert!(!is_denied("ANTHROPICS_CLUB"));
}

#[test]
fn suffix_requires_underscore_boundary() {
    // *_AUTH requires trailing _AUTH, not any substring "AUTH".
    assert!(!is_denied("AUTHENTIC"));
    assert!(!is_denied("PASSPORT"));
}

#[test]
fn empty_string_is_not_denied() {
    assert!(!is_denied(""));
}
