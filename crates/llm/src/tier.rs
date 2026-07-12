//! Model tier table and resolution.
//!
//! The vision's "Role-to-model mapping": a top-level `models:` block
//! names three tiers (`primary`, `lightweight`, `advisor`); roles
//! reference a tier by name **or** supply a literal model ID. Swapping
//! every role's model version is then a one-line edit in the `models:`
//! block.
//!
//! This crate owns the *resolution* (per the `llm` ABI); the table
//! itself is a top-level config section composed by `loopr`, which
//! resolves each role's model reference against it after load so every
//! downstream consumer (`AnthropicClient`, `ProcessSnapshot`, the
//! per-role agent configs) sees a concrete model ID, never a tier name.

use serde::{Deserialize, Serialize};

/// The three named model tiers. Keys on disk are kebab-case; defaults
/// match the workspace's current concrete model IDs so an absent
/// `models:` block resolves every tier reference to a working model.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ModelTiers {
    /// The everyday model: implementers, reviewers, the decomposer.
    pub primary: String,
    /// The cheap model: tier gates, validation, lightweight delegation.
    pub lightweight: String,
    /// The strongest model: the Director (long-lived supervision).
    pub advisor: String,
}

impl Default for ModelTiers {
    fn default() -> Self {
        Self {
            primary: "claude-sonnet-4-6".to_string(),
            lightweight: "claude-haiku-4-5".to_string(),
            advisor: "claude-opus-4-7".to_string(),
        }
    }
}

impl ModelTiers {
    /// Resolve a model reference to a concrete model ID. A tier name
    /// (`primary` / `lightweight` / `advisor`) maps to that tier's
    /// configured model; anything else is treated as a literal model ID
    /// and returned unchanged. This is the "deserializer tries the table
    /// first, falls back to literal" rule expressed at resolution time
    /// (deserialization has no table in scope, so resolution is where
    /// the lookup happens).
    pub fn resolve(&self, reference: &str) -> String {
        match reference {
            "primary" => self.primary.clone(),
            "lightweight" => self.lightweight.clone(),
            "advisor" => self.advisor.clone(),
            literal => literal.to_string(),
        }
    }

    /// Fail-closed variant of `resolve` (Phase 13 of
    /// `docs/design/2026-07-11-verified-swarm.md`, per-role model
    /// routing). `resolve` treats ANY non-tier string as a literal model
    /// ID, so a typo'd tier name (e.g. `lightwieght`) silently becomes a
    /// "model" that the Anthropic API will reject at call time -- by
    /// then the config-load failure signal is long gone. This method
    /// accepts the same three tier names, and treats a literal as valid
    /// only when it looks like an Anthropic model ID (`claude-` prefix,
    /// matching every model ID in this workspace: `self.primary` /
    /// `self.lightweight` / `self.advisor`'s defaults and every id in
    /// `docs/design/2026-07-11-verified-swarm.md`'s Config block).
    /// Anything else is rejected with a typed error naming the bad
    /// reference, so a config-load-time typo fails loudly instead of
    /// routing a role to a nonsense model. Used only for the
    /// `agents.implementer.model` / `agents.reviewer.model` knobs
    /// (per-role opt-in split); `llm.model` and `agents.director.model`
    /// keep the lenient `resolve` (unchanged by this phase).
    pub fn resolve_checked(&self, reference: &str) -> Result<String, UnknownModelTier> {
        match reference {
            "primary" => Ok(self.primary.clone()),
            "lightweight" => Ok(self.lightweight.clone()),
            "advisor" => Ok(self.advisor.clone()),
            literal if literal.starts_with("claude-") => Ok(literal.to_string()),
            other => Err(UnknownModelTier {
                reference: other.to_string(),
            }),
        }
    }
}

/// Typed rejection from `ModelTiers::resolve_checked`: `reference` is
/// neither a known tier name nor recognizable as a literal Anthropic
/// model ID.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error(
    "unknown model tier {reference:?}: expected one of \"primary\", \"lightweight\", \"advisor\", \
     or a literal model id starting with \"claude-\""
)]
pub struct UnknownModelTier {
    pub reference: String,
}

#[cfg(test)]
mod tests;
