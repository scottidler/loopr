//! Configuration resolution (3-layer merge).
//!
//! Resolves effective LoopConfig from:
//! 1. GlobalConfig (defaults)
//! 2. LoopTypeDefinition (type-specific)
//! 3. ConfigOverrides (execution-specific)

use std::collections::HashMap;

use super::{ConfigOverrides, GlobalConfig, LoopConfig, LoopTypeDefinition};

/// Configuration resolver that merges all three layers.
#[derive(Debug)]
pub struct ConfigResolver {
    global: GlobalConfig,
    loop_types: HashMap<String, LoopTypeDefinition>,
}

impl ConfigResolver {
    /// Create a new resolver with the given global config and loop types.
    pub fn new(global: GlobalConfig, loop_types: HashMap<String, LoopTypeDefinition>) -> Self {
        Self { global, loop_types }
    }

    /// Resolve the effective configuration for a loop.
    ///
    /// Resolution order:
    /// 1. Start with compiled-in defaults
    /// 2. Apply global config values
    /// 3. Apply loop type definition (with inheritance)
    /// 4. Apply execution overrides
    pub fn resolve(&self, loop_type: &str, overrides: &ConfigOverrides) -> eyre::Result<LoopConfig> {
        // Start with defaults
        let mut config = LoopConfig::new(loop_type);

        // Apply global defaults
        config.validation_command = self.global.validation.command.clone();
        config.max_iterations = self.global.validation.max_iterations;
        config.iteration_timeout_ms = self.global.validation.iteration_timeout_ms;
        config.progress_max_entries = self.global.progress.max_entries;
        config.progress_max_chars = self.global.progress.max_output_chars;

        // Apply loop type definition (with inheritance)
        if let Some(type_def) = self.loop_types.get(loop_type) {
            self.apply_type_definition(&mut config, type_def)?;
        }

        // Apply execution overrides
        self.apply_overrides(&mut config, overrides);

        // Validate final config
        config.validate()?;

        Ok(config)
    }

    /// Apply a loop type definition, handling inheritance.
    fn apply_type_definition(&self, config: &mut LoopConfig, type_def: &LoopTypeDefinition) -> eyre::Result<()> {
        // Handle inheritance first
        if let Some(parent_name) = &type_def.extends {
            if let Some(parent_def) = self.loop_types.get(parent_name) {
                // Recursively apply parent (handles multi-level inheritance)
                self.apply_type_definition(config, parent_def)?;
            } else {
                log::warn!(
                    "Loop type '{}' extends '{}' which doesn't exist",
                    type_def.name,
                    parent_name
                );
            }
        }

        // Apply this type's values (overrides parent)
        if !type_def.prompt.is_empty() {
            config.prompt_template = type_def.prompt.clone();
        }
        if let Some(cmd) = &type_def.validation_command {
            config.validation_command = cmd.clone();
        }
        if let Some(code) = type_def.success_exit_code {
            config.success_exit_code = code;
        }
        if let Some(max_iter) = type_def.max_iterations {
            config.max_iterations = max_iter;
        }
        if let Some(max_turns) = type_def.max_turns {
            config.max_turns_per_iteration = max_turns;
        }
        if let Some(timeout) = type_def.iteration_timeout_ms {
            config.iteration_timeout_ms = timeout;
        }
        if let Some(tokens) = type_def.max_tokens {
            config.max_tokens = tokens;
        }
        if let Some(tools) = &type_def.tools {
            config.tools = tools.clone();
        }

        Ok(())
    }

    /// Apply execution overrides.
    fn apply_overrides(&self, config: &mut LoopConfig, overrides: &ConfigOverrides) {
        if let Some(max_iter) = overrides.max_iterations {
            config.max_iterations = max_iter;
        }
        if let Some(max_turns) = overrides.max_turns {
            config.max_turns_per_iteration = max_turns;
        }
        if let Some(cmd) = &overrides.validation_command {
            config.validation_command = cmd.clone();
        }
        if let Some(timeout) = overrides.iteration_timeout_ms {
            config.iteration_timeout_ms = timeout;
        }
        if let Some(tokens) = overrides.max_tokens {
            config.max_tokens = tokens;
        }
        if let Some(tools) = &overrides.tools {
            config.tools = tools.clone();
        }
        if let Some(prompt) = &overrides.prompt {
            config.prompt_template = prompt.clone();
        }
    }

    /// Get the global config.
    pub fn global(&self) -> &GlobalConfig {
        &self.global
    }

    /// Get a loop type definition by name.
    pub fn get_loop_type(&self, name: &str) -> Option<&LoopTypeDefinition> {
        self.loop_types.get(name)
    }

    /// List all available loop types.
    pub fn loop_type_names(&self) -> Vec<&str> {
        self.loop_types.keys().map(|s| s.as_str()).collect()
    }
}

/// Source tracking for configuration values (for introspection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Compiled-in default value.
    Default,
    /// From global configuration file.
    Global,
    /// From loop type definition.
    LoopType,
    /// From execution override.
    Override,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::Default => write!(f, "default"),
            ConfigSource::Global => write!(f, "global"),
            ConfigSource::LoopType => write!(f, "loop type"),
            ConfigSource::Override => write!(f, "override"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_resolver() -> ConfigResolver {
        let global = GlobalConfig::default();
        let loop_types: HashMap<_, _> = LoopTypeDefinition::builtins()
            .into_iter()
            .map(|t| (t.name.clone(), t))
            .collect();
        ConfigResolver::new(global, loop_types)
    }

    #[test]
    fn test_resolve_ralph() {
        let resolver = test_resolver();
        let config = resolver.resolve("ralph", &ConfigOverrides::none()).unwrap();

        assert_eq!(config.loop_type, "ralph");
        assert_eq!(config.max_iterations, 100);
        assert_eq!(config.max_turns_per_iteration, 50);
    }

    #[test]
    fn test_resolve_plan_has_different_limits() {
        let resolver = test_resolver();
        let config = resolver.resolve("plan", &ConfigOverrides::none()).unwrap();

        assert_eq!(config.loop_type, "plan");
        assert_eq!(config.max_iterations, 10); // Plan-specific
        assert_eq!(config.max_turns_per_iteration, 30);
    }

    #[test]
    fn test_resolve_with_overrides() {
        let resolver = test_resolver();
        let overrides = ConfigOverrides::with_max_iterations(5);
        let config = resolver.resolve("ralph", &overrides).unwrap();

        assert_eq!(config.max_iterations, 5); // Override wins
    }

    #[test]
    fn test_resolve_phase_inherits_from_ralph() {
        let resolver = test_resolver();
        let config = resolver.resolve("phase", &ConfigOverrides::none()).unwrap();

        // Phase extends ralph, so should have ralph's base values
        // but phase-specific overrides
        assert_eq!(config.loop_type, "phase");
        assert_eq!(config.max_iterations, 50); // Phase-specific
    }

    #[test]
    fn test_resolve_unknown_type_uses_defaults() {
        let resolver = test_resolver();
        let config = resolver.resolve("unknown", &ConfigOverrides::none()).unwrap();

        assert_eq!(config.loop_type, "unknown");
        // Should use global defaults
        assert_eq!(config.validation_command, "otto ci");
    }

    #[test]
    fn test_override_validation_command() {
        let resolver = test_resolver();
        let overrides = ConfigOverrides::with_validation_command("cargo test");
        let config = resolver.resolve("ralph", &overrides).unwrap();

        assert_eq!(config.validation_command, "cargo test");
    }

    #[test]
    fn test_loop_type_names() {
        let resolver = test_resolver();
        let names = resolver.loop_type_names();

        assert!(names.contains(&"ralph"));
        assert!(names.contains(&"plan"));
        assert!(names.contains(&"spec"));
        assert!(names.contains(&"phase"));
        assert!(names.contains(&"explore"));
    }

    #[test]
    fn test_config_source_display() {
        assert_eq!(ConfigSource::Default.to_string(), "default");
        assert_eq!(ConfigSource::Global.to_string(), "global");
        assert_eq!(ConfigSource::LoopType.to_string(), "loop type");
        assert_eq!(ConfigSource::Override.to_string(), "override");
    }
}
