//! `LlmConfig`: deserialized from `.loopr/config.yml` by `loopr` and
//! passed to `AnthropicClient::new`.
//!
//! The API key is NOT a field here: the config records only the NAME
//! of the env var holding it (`api-key-env`). The daemon resolves the
//! name to a value and hands the string to `AnthropicClient::new`,
//! which installs it as a sensitive `HeaderValue` and drops the
//! `String`. See `anthropic.rs` and the design doc's Security section
//! for the end-to-end contract.

use serde::{Deserialize, Serialize};

/// Configuration for the LLM backend. Loaded from `.loopr/config.yml`
/// under a `llm:` key. Keys on disk are kebab-case; `serde` renames to
/// snake_case for the Rust field names.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LlmConfig {
    /// Literal Anthropic model ID. Stage 7+ will promote this to a
    /// tier name or literal via a small resolver.
    pub model: String,

    /// Upper bound on generation tokens. Anthropic default is model-
    /// specific; we pin to match the scope memo (8192) so the
    /// decomposer's tool-input JSON does not truncate.
    pub max_tokens: u32,

    /// Sampling temperature in [0, 1].
    pub temperature: f32,

    /// Name of the env var holding the API key. NEVER the literal key.
    pub api_key_env: String,

    /// Base URL of the Anthropic-compatible Messages API. Defaults
    /// to the hosted endpoint. Overridable to support local mocks
    /// (testing), corporate proxies, or Anthropic-compatible
    /// gateways (e.g. AWS Bedrock fronted by a shim). The value is
    /// concatenated with `/v1/messages` at call time; must not end
    /// with a slash.
    pub api_base_url: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            temperature: 0.3,
            api_key_env: "ANTHROPIC_API_KEY".into(),
            api_base_url: "https://api.anthropic.com".into(),
        }
    }
}

#[cfg(test)]
mod tests;
