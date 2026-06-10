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
}

#[cfg(test)]
mod tests;
