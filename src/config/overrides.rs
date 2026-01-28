//! Execution overrides (Layer 3).
//!
//! Runtime overrides applied when spawning a specific loop.

use serde::{Deserialize, Serialize};

/// Configuration overrides for a specific loop execution.
///
/// These override both global config and loop type definition.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ConfigOverrides {
    /// Override maximum iterations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,

    /// Override maximum turns per iteration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,

    /// Override validation command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_command: Option<String>,

    /// Override iteration timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration_timeout_ms: Option<u64>,

    /// Override max tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Override tools list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,

    /// Custom prompt (completely replaces template).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

impl ConfigOverrides {
    /// Create empty overrides (no overrides applied).
    pub fn none() -> Self {
        Self::default()
    }

    /// Check if any overrides are set.
    pub fn is_empty(&self) -> bool {
        self.max_iterations.is_none()
            && self.max_turns.is_none()
            && self.validation_command.is_none()
            && self.iteration_timeout_ms.is_none()
            && self.max_tokens.is_none()
            && self.tools.is_none()
            && self.prompt.is_none()
    }

    /// Create overrides with just max_iterations.
    pub fn with_max_iterations(max_iterations: u32) -> Self {
        Self {
            max_iterations: Some(max_iterations),
            ..Default::default()
        }
    }

    /// Create overrides with just validation_command.
    pub fn with_validation_command(cmd: impl Into<String>) -> Self {
        Self {
            validation_command: Some(cmd.into()),
            ..Default::default()
        }
    }
}

/// Builder for ConfigOverrides.
#[derive(Debug, Default)]
pub struct ConfigOverridesBuilder {
    overrides: ConfigOverrides,
}

impl ConfigOverridesBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set max_iterations override.
    pub fn max_iterations(mut self, value: u32) -> Self {
        self.overrides.max_iterations = Some(value);
        self
    }

    /// Set max_turns override.
    pub fn max_turns(mut self, value: u32) -> Self {
        self.overrides.max_turns = Some(value);
        self
    }

    /// Set validation_command override.
    pub fn validation_command(mut self, cmd: impl Into<String>) -> Self {
        self.overrides.validation_command = Some(cmd.into());
        self
    }

    /// Set iteration_timeout_ms override.
    pub fn iteration_timeout_ms(mut self, ms: u64) -> Self {
        self.overrides.iteration_timeout_ms = Some(ms);
        self
    }

    /// Set max_tokens override.
    pub fn max_tokens(mut self, tokens: u32) -> Self {
        self.overrides.max_tokens = Some(tokens);
        self
    }

    /// Set tools override.
    pub fn tools(mut self, tools: Vec<String>) -> Self {
        self.overrides.tools = Some(tools);
        self
    }

    /// Set prompt override.
    pub fn prompt(mut self, prompt: impl Into<String>) -> Self {
        self.overrides.prompt = Some(prompt.into());
        self
    }

    /// Build the ConfigOverrides.
    pub fn build(self) -> ConfigOverrides {
        self.overrides
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_overrides() {
        let overrides = ConfigOverrides::none();
        assert!(overrides.is_empty());
    }

    #[test]
    fn test_with_max_iterations() {
        let overrides = ConfigOverrides::with_max_iterations(25);
        assert!(!overrides.is_empty());
        assert_eq!(overrides.max_iterations, Some(25));
    }

    #[test]
    fn test_builder() {
        let overrides = ConfigOverridesBuilder::new()
            .max_iterations(10)
            .validation_command("cargo test")
            .max_tokens(4096)
            .build();

        assert_eq!(overrides.max_iterations, Some(10));
        assert_eq!(overrides.validation_command, Some("cargo test".to_string()));
        assert_eq!(overrides.max_tokens, Some(4096));
        assert!(overrides.max_turns.is_none());
    }

    #[test]
    fn test_serialize() {
        let overrides = ConfigOverrides::with_max_iterations(25);
        let json = serde_json::to_string(&overrides).unwrap();
        assert!(json.contains("25"));
        // Empty fields should be skipped
        assert!(!json.contains("max_turns"));
    }

    #[test]
    fn test_deserialize() {
        let json = r#"{"max_iterations": 50, "validation_command": "make test"}"#;
        let overrides: ConfigOverrides = serde_json::from_str(json).unwrap();
        assert_eq!(overrides.max_iterations, Some(50));
        assert_eq!(overrides.validation_command, Some("make test".to_string()));
    }
}
