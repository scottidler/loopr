//! Configuration system for Loopr.
//!
//! Three-layer configuration hierarchy:
//! 1. Global config (~/.config/loopr/loopr.yml or .loopr.yml)
//! 2. Loop type definitions (~/.config/loopr/loops/*.yml or .loopr/loops/*.yml)
//! 3. Execution overrides (runtime per-loop)

// Config APIs are public, will be used by other modules
#![allow(unused_imports)]

use eyre::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

// Re-export main types
pub use self::global::{GlobalConfig, LlmConfig, ResolvedLlmConfig};
pub use self::loop_config::LoopConfig;
pub use self::loop_type::LoopTypeDefinition;
pub use self::overrides::ConfigOverrides;
pub use self::resolution::ConfigResolver;
pub use self::task_manager::TaskManagerConfig;

mod global;
mod loop_config;
mod loop_type;
mod overrides;
mod resolution;
mod task_manager;

/// Legacy Config alias for backwards compatibility during migration.
/// Use GlobalConfig for new code.
pub type Config = GlobalConfig;

/// Default validation command.
pub const DEFAULT_VALIDATION_COMMAND: &str = "otto ci";

/// Default LLM model.
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-20250514";

/// Default tools available to loops.
pub fn default_tools() -> Vec<String> {
    vec![
        "read".to_string(),
        "write".to_string(),
        "edit".to_string(),
        "list".to_string(),
        "glob".to_string(),
        "bash".to_string(),
    ]
}

/// Load configuration from the standard search paths.
///
/// Search order:
/// 1. Explicit path if provided
/// 2. .loopr.yml in current directory (project config)
/// 3. ~/.config/loopr/loopr.yml (user config)
/// 4. Default values
pub fn load_config(explicit_path: Option<&PathBuf>) -> Result<GlobalConfig> {
    GlobalConfig::load(explicit_path)
}

/// Load loop type definitions from standard paths.
///
/// Search order (later overrides earlier):
/// 1. Built-in types (compiled defaults)
/// 2. ~/.config/loopr/loops/ (user types)
/// 3. .loopr/loops/ (project types)
pub fn load_loop_types() -> Result<HashMap<String, LoopTypeDefinition>> {
    let mut types = HashMap::new();

    // Start with built-in types
    for loop_type in LoopTypeDefinition::builtins() {
        types.insert(loop_type.name.clone(), loop_type);
    }

    // Load user types
    if let Some(config_dir) = dirs::config_dir() {
        let user_loops_dir = config_dir.join("loopr").join("loops");
        if user_loops_dir.exists() {
            load_loop_types_from_dir(&user_loops_dir, &mut types)?;
        }
    }

    // Load project types (override user types)
    let project_loops_dir = PathBuf::from(".loopr/loops");
    if project_loops_dir.exists() {
        load_loop_types_from_dir(&project_loops_dir, &mut types)?;
    }

    Ok(types)
}

/// Load loop type definitions from a directory.
fn load_loop_types_from_dir(dir: &Path, types: &mut HashMap<String, LoopTypeDefinition>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("Failed to read dir: {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "yml" || ext == "yaml") {
            match LoopTypeDefinition::load_from_file(&path) {
                Ok(loop_type) => {
                    log::debug!("Loaded loop type '{}' from {}", loop_type.name, path.display());
                    types.insert(loop_type.name.clone(), loop_type);
                }
                Err(e) => {
                    log::warn!("Failed to load loop type from {}: {}", path.display(), e);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_tools() {
        let tools = default_tools();
        assert!(tools.contains(&"read".to_string()));
        assert!(tools.contains(&"write".to_string()));
        assert!(tools.contains(&"bash".to_string()));
    }

    #[test]
    fn test_load_config_default() {
        // Should succeed with defaults when no config file exists
        let config = load_config(None).unwrap();
        assert_eq!(config.llm.timeout_ms, 300_000);
    }
}
