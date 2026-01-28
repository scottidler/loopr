//! Loop type definitions (Layer 2).
//!
//! Loop types are templates that define how a category of loops behaves.
//! Loaded from ~/.config/loopr/loops/*.yml or .loopr/loops/*.yml

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Loop type definition.
///
/// Defines the template for a category of loops (e.g., plan, spec, phase, ralph).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LoopTypeDefinition {
    /// Type name (e.g., "plan", "spec", "phase", "ralph").
    pub name: String,

    /// Description of this loop type.
    #[serde(default)]
    pub description: String,

    /// Prompt template (Handlebars format).
    #[serde(default)]
    pub prompt: String,

    /// Validation command (overrides global).
    #[serde(rename = "validation-command")]
    pub validation_command: Option<String>,

    /// Success exit code.
    #[serde(rename = "success-exit-code")]
    pub success_exit_code: Option<i32>,

    /// Maximum iterations (overrides global).
    #[serde(rename = "max-iterations")]
    pub max_iterations: Option<u32>,

    /// Maximum tool calls per iteration.
    #[serde(rename = "max-turns")]
    pub max_turns: Option<u32>,

    /// Timeout per iteration in milliseconds.
    #[serde(rename = "iteration-timeout-ms")]
    pub iteration_timeout_ms: Option<u64>,

    /// Maximum tokens for LLM.
    #[serde(rename = "max-tokens")]
    pub max_tokens: Option<u32>,

    /// Available tools.
    #[serde(default)]
    pub tools: Option<Vec<String>>,

    /// Parent type to inherit from.
    #[serde(default)]
    pub extends: Option<String>,
}

impl Default for LoopTypeDefinition {
    fn default() -> Self {
        Self {
            name: "ralph".to_string(),
            description: "General-purpose coding loop".to_string(),
            prompt: String::new(),
            validation_command: None,
            success_exit_code: None,
            max_iterations: None,
            max_turns: None,
            iteration_timeout_ms: None,
            max_tokens: None,
            tools: None,
            extends: None,
        }
    }
}

impl LoopTypeDefinition {
    /// Load a loop type definition from a YAML file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content =
            fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.as_ref().display()))?;
        let def: Self =
            serde_yaml::from_str(&content).with_context(|| format!("Failed to parse {}", path.as_ref().display()))?;
        Ok(def)
    }

    /// Get built-in loop type definitions.
    pub fn builtins() -> Vec<Self> {
        vec![
            Self::builtin_ralph(),
            Self::builtin_plan(),
            Self::builtin_spec(),
            Self::builtin_phase(),
            Self::builtin_explore(),
        ]
    }

    fn builtin_ralph() -> Self {
        Self {
            name: "ralph".to_string(),
            description: "General-purpose coding loop".to_string(),
            prompt: r#"You are an autonomous coding agent implementing a task.

## Task
{{task}}

## Previous Iteration Feedback
{{#if feedback}}
The previous attempt failed:
{{feedback}}
{{/if}}

Implement the requirements. Run validation when done."#
                .to_string(),
            validation_command: None, // Use global default
            success_exit_code: Some(0),
            max_iterations: Some(100),
            max_turns: Some(50),
            iteration_timeout_ms: None,
            max_tokens: Some(16384),
            tools: None, // Use default tools
            extends: None,
        }
    }

    fn builtin_plan() -> Self {
        Self {
            name: "plan".to_string(),
            description: "Creates high-level implementation plans".to_string(),
            prompt: r#"You are creating a plan for: {{task}}

## Requirements
- Break down the task into clear phases
- Each phase should be independently testable
- Consider dependencies between phases
- Include validation criteria for each phase

## Previous Iteration Feedback
{{#if feedback}}
The previous plan was rejected:
{{feedback}}
{{/if}}

Create a detailed plan in markdown format."#
                .to_string(),
            validation_command: None,
            success_exit_code: Some(0),
            max_iterations: Some(10), // Plans should converge quickly
            max_turns: Some(30),
            iteration_timeout_ms: None,
            max_tokens: Some(8192),
            tools: Some(vec!["read".to_string(), "list".to_string(), "glob".to_string()]),
            extends: None,
        }
    }

    fn builtin_spec() -> Self {
        Self {
            name: "spec".to_string(),
            description: "Generates detailed specifications from plans".to_string(),
            prompt: r#"You are creating a detailed specification.

## Plan Context
{{plan_context}}

## Spec Name
{{spec_name}}

## Previous Iteration Feedback
{{#if feedback}}
The previous spec was rejected:
{{feedback}}
{{/if}}

Create a detailed specification including:
- Exact requirements
- API contracts
- Test criteria
- Phase breakdown"#
                .to_string(),
            validation_command: None,
            success_exit_code: Some(0),
            max_iterations: Some(25), // Specs need refinement
            max_turns: Some(40),
            iteration_timeout_ms: None,
            max_tokens: Some(8192),
            tools: Some(vec![
                "read".to_string(),
                "write".to_string(),
                "list".to_string(),
                "glob".to_string(),
            ]),
            extends: None,
        }
    }

    fn builtin_phase() -> Self {
        Self {
            name: "phase".to_string(),
            description: "Implements a single phase from a spec".to_string(),
            prompt: r#"You are implementing Phase {{phase_number}} of {{spec_name}}.

## Requirements
{{phase_requirements}}

## Previous Iteration Feedback
{{#if feedback}}
The previous attempt failed:
{{feedback}}
{{/if}}

Implement the requirements. Run validation when done."#
                .to_string(),
            validation_command: None,
            success_exit_code: Some(0),
            max_iterations: Some(50), // Phases are core work
            max_turns: Some(50),
            iteration_timeout_ms: None,
            max_tokens: Some(8192),
            tools: None, // Use default tools (read, write, edit, list, glob, bash)
            extends: Some("ralph".to_string()),
        }
    }

    fn builtin_explore() -> Self {
        Self {
            name: "explore".to_string(),
            description: "Quick discovery and investigation".to_string(),
            prompt: r#"You are exploring the codebase to understand:
{{question}}

Investigate and summarize your findings."#
                .to_string(),
            validation_command: Some("true".to_string()), // Always succeeds
            success_exit_code: Some(0),
            max_iterations: Some(5), // Quick discovery
            max_turns: Some(20),
            iteration_timeout_ms: Some(60_000), // 1 minute
            max_tokens: Some(4096),
            tools: Some(vec![
                "read".to_string(),
                "list".to_string(),
                "glob".to_string(),
                "bash".to_string(), // For git log, etc.
            ]),
            extends: None,
        }
    }

    /// Validate the loop type definition.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            eyre::bail!("Loop type name cannot be empty");
        }
        if let Some(max_iter) = self.max_iterations
            && max_iter == 0
        {
            eyre::bail!("max_iterations must be > 0");
        }
        if let Some(max_turns) = self.max_turns
            && max_turns == 0
        {
            eyre::bail!("max_turns must be > 0");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtins() {
        let builtins = LoopTypeDefinition::builtins();
        assert_eq!(builtins.len(), 5);

        let names: Vec<_> = builtins.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"ralph"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"spec"));
        assert!(names.contains(&"phase"));
        assert!(names.contains(&"explore"));
    }

    #[test]
    fn test_plan_has_lower_iterations() {
        let plan = LoopTypeDefinition::builtin_plan();
        assert_eq!(plan.max_iterations, Some(10));
    }

    #[test]
    fn test_phase_extends_ralph() {
        let phase = LoopTypeDefinition::builtin_phase();
        assert_eq!(phase.extends, Some("ralph".to_string()));
    }

    #[test]
    fn test_parse_yaml() {
        let yaml = r#"
name: custom
description: "A custom loop type"
max-iterations: 25
tools:
  - read
  - write
"#;
        let def: LoopTypeDefinition = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(def.name, "custom");
        assert_eq!(def.max_iterations, Some(25));
        assert_eq!(def.tools, Some(vec!["read".to_string(), "write".to_string()]));
    }

    #[test]
    fn test_validation() {
        let def = LoopTypeDefinition::default();
        assert!(def.validate().is_ok());
    }

    #[test]
    fn test_invalid_empty_name() {
        let def = LoopTypeDefinition {
            name: String::new(),
            ..Default::default()
        };
        assert!(def.validate().is_err());
    }
}
