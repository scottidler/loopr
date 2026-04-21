//! Subprocess environment scrubbing (D12).
//!
//! The daemon process inherits whatever the operator has in the shell's
//! environment at fork time: `ANTHROPIC_API_KEY`, Slack tokens, AWS
//! credentials, GitHub PATs. Without scrubbing, every `cargo build` / `npm
//! install` / agent-generated bash command reads those keys directly from
//! `std::env`. That is an exfiltration channel a hostile LLM output can
//! trigger with one `env > /tmp/dump` call.
//!
//! The design's posture (Architect R1 flip from allowlist to denylist, R2
//! prefix/suffix expansion, R3 `_PASS` removal for false-positives): strip
//! every env var whose name starts with a known-provider prefix OR ends
//! with a known-secret-shape suffix. Everything else passes through so
//! `cargo`/`rustup`/`git`/`npm` can find their config (`SSH_AUTH_SOCK`,
//! `XDG_*`, `RUSTUP_HOME`, `CARGO_HOME`, etc.).
//!
//! The match is strict prefix/suffix — NOT substring — so `SSH_AUTH_SOCK`
//! passes (ends `_SOCK`, not `_AUTH`) and `MULTIPASS_*` / `BYPASS_*` /
//! `CLI_PASS_ARGS` pass (no `_PASSWORD` suffix). `SLACK_BOT_TOKEN` is
//! stripped (ends `_TOKEN`).
//!
//! Denylists are never complete. A new provider's unanticipated env shape
//! can leak; defense-in-depth is that the daemon's env shouldn't hold
//! arbitrary-provider secrets anyway.

/// Provider-prefix denylist. Match is `var.starts_with("X_")` with the
/// underscore included — not a bare-prefix substring — so `LOOPRA_FOO`
/// does not match `LOOPR_`.
const DENY_PREFIXES: &[&str] = &[
    "LOOPR_",
    "ANTHROPIC_",
    "AWS_",
    "GITHUB_",
    "GOOGLE_",
    "GCP_",
    "AZURE_",
    "OPENAI_",
    "GEMINI_",
];

/// Secret-shape suffix denylist. Match is `var.ends_with("_X")` with the
/// leading underscore included so `FOOAUTH` does not match `_AUTH`.
const DENY_SUFFIXES: &[&str] = &["_API_KEY", "_SECRET", "_TOKEN", "_PASSWORD", "_CREDENTIALS", "_AUTH"];

pub(crate) fn is_denied(var_name: &str) -> bool {
    for p in DENY_PREFIXES {
        if var_name.starts_with(p) {
            return true;
        }
    }
    for s in DENY_SUFFIXES {
        if var_name.ends_with(s) {
            return true;
        }
    }
    false
}

/// Apply the denylist to a pre-built `tokio::process::Command` in place.
///
/// The child inherits the parent's full env by default; we can't mutate
/// the parent. Instead we explicitly `env_remove` every matching var on
/// the Command. `Command::env_remove` overrides the inheritance — the
/// child spawns without that var regardless of what the parent has.
///
/// This is applied BEFORE bwrap-wrapping. bwrap itself inherits the
/// scrubbed env; its inner `sh -c` gets the scrubbed view. No flag on
/// bwrap alters this — the env is set at `execve`, after bwrap's own
/// namespace setup.
pub(crate) fn scrub_command(cmd: &mut tokio::process::Command) {
    for (k, _) in std::env::vars_os() {
        if let Some(name) = k.to_str()
            && is_denied(name)
        {
            cmd.env_remove(&k);
        }
    }
}

#[cfg(test)]
mod tests;
