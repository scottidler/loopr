//! Effective loop configuration (resolved from all layers).
//!
//! This is the final configuration applied to a running loop.

use serde::{Deserialize, Serialize};

/// Effective configuration for a running loop.
///
/// This is the resolved configuration after merging:
/// 1. Global defaults
/// 2. Loop type definition
/// 3. Execution overrides
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopConfig {
    /// Loop type name.
    pub loop_type: String,

    /// Prompt template (Handlebars format).
    pub prompt_template: String,

    /// Validation command to run.
    pub validation_command: String,

    /// Exit code that indicates success.
    pub success_exit_code: i32,

    /// Maximum iterations before giving up.
    pub max_iterations: u32,

    /// Maximum tool calls per iteration.
    pub max_turns_per_iteration: u32,

    /// Timeout per iteration in milliseconds.
    pub iteration_timeout_ms: u64,

    /// Maximum tokens for LLM response.
    pub max_tokens: u32,

    /// Available tools for this loop.
    pub tools: Vec<String>,

    /// Maximum progress entries to retain.
    pub progress_max_entries: usize,

    /// Maximum characters per progress output.
    pub progress_max_chars: usize,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            loop_type: "ralph".to_string(),
            prompt_template: String::new(),
            validation_command: crate::config::DEFAULT_VALIDATION_COMMAND.to_string(),
            success_exit_code: 0,
            max_iterations: 100,
            max_turns_per_iteration: 50,
            iteration_timeout_ms: 300_000, // 5 minutes
            max_tokens: 16384,
            tools: crate::config::default_tools(),
            progress_max_entries: 5,
            progress_max_chars: 500,
        }
    }
}

impl LoopConfig {
    /// Create a new LoopConfig with the given loop type.
    pub fn new(loop_type: &str) -> Self {
        Self {
            loop_type: loop_type.to_string(),
            ..Default::default()
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> eyre::Result<()> {
        if self.loop_type.is_empty() {
            eyre::bail!("loop_type cannot be empty");
        }
        if self.max_iterations == 0 {
            eyre::bail!("max_iterations must be > 0");
        }
        if self.max_turns_per_iteration == 0 {
            eyre::bail!("max_turns_per_iteration must be > 0");
        }
        if self.validation_command.is_empty() {
            eyre::bail!("validation_command cannot be empty");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_loop_config() {
        let config = LoopConfig::default();
        assert_eq!(config.loop_type, "ralph");
        assert_eq!(config.max_iterations, 100);
        assert_eq!(config.max_turns_per_iteration, 50);
        assert!(!config.tools.is_empty());
    }

    #[test]
    fn test_new_loop_config() {
        let config = LoopConfig::new("phase");
        assert_eq!(config.loop_type, "phase");
    }

    #[test]
    fn test_validation() {
        let config = LoopConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_empty_type() {
        let config = LoopConfig {
            loop_type: String::new(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_invalid_zero_iterations() {
        let config = LoopConfig {
            max_iterations: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
