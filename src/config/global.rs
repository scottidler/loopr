//! Global configuration (Layer 1).
//!
//! Loaded from ~/.config/loopr/loopr.yml or .loopr.yml

use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Global configuration for Loopr.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// LLM provider settings.
    pub llm: LlmConfig,

    /// Concurrency limits.
    pub concurrency: ConcurrencyConfig,

    /// Validation defaults.
    pub validation: ValidationConfig,

    /// Progress tracking settings.
    pub progress: ProgressConfig,

    /// Git settings.
    pub git: GitConfig,

    /// Storage settings.
    pub storage: StorageConfig,

    /// Loop type search paths.
    pub loops: LoopsConfig,
}

impl GlobalConfig {
    /// Load configuration with fallback chain.
    ///
    /// Search order:
    /// 1. Explicit path if provided
    /// 2. .loopr.yml in current directory
    /// 3. ~/.config/loopr/loopr.yml
    /// 4. Defaults
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        // Explicit path takes precedence
        if let Some(path) = config_path {
            return Self::load_from_file(path).context(format!("Failed to load config from {}", path.display()));
        }

        // Try project config
        let project_config = PathBuf::from(".loopr.yml");
        if project_config.exists() {
            match Self::load_from_file(&project_config) {
                Ok(config) => {
                    log::info!("Loaded config from .loopr.yml");
                    return Ok(config);
                }
                Err(e) => {
                    log::warn!("Failed to load .loopr.yml: {}", e);
                }
            }
        }

        // Try user config
        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("loopr").join("loopr.yml");
            if user_config.exists() {
                match Self::load_from_file(&user_config) {
                    Ok(config) => {
                        log::info!("Loaded config from {}", user_config.display());
                        return Ok(config);
                    }
                    Err(e) => {
                        log::warn!("Failed to load {}: {}", user_config.display(), e);
                    }
                }
            }
        }

        // Use defaults
        log::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path).context("Failed to read config file")?;
        let config: Self = serde_yaml::from_str(&content).context("Failed to parse config file")?;
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        if self.concurrency.max_loops == 0 {
            eyre::bail!("concurrency.max_loops must be > 0");
        }
        if self.concurrency.max_api_calls == 0 {
            eyre::bail!("concurrency.max_api_calls must be > 0");
        }
        if self.validation.max_iterations == 0 {
            eyre::bail!("validation.max_iterations must be > 0");
        }
        Ok(())
    }
}

/// LLM provider settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Default model (provider/model format).
    #[serde(rename = "default")]
    pub default_model: String,

    /// Timeout per LLM call in milliseconds.
    #[serde(rename = "timeout-ms")]
    pub timeout_ms: u64,

    /// Provider configurations.
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                models: HashMap::new(),
            },
        );

        Self {
            default_model: crate::config::DEFAULT_MODEL.to_string(),
            timeout_ms: 300_000, // 5 minutes
            providers,
        }
    }
}

/// Provider configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Environment variable for API key.
    #[serde(rename = "api-key-env")]
    pub api_key_env: String,

    /// Base URL for API.
    #[serde(rename = "base-url")]
    pub base_url: String,

    /// Model-specific settings.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
}

/// Model-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Maximum tokens for this model.
    #[serde(rename = "max-tokens")]
    pub max_tokens: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self { max_tokens: 16384 }
    }
}

/// Concurrency limits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ConcurrencyConfig {
    /// Total concurrent loops.
    #[serde(rename = "max-loops")]
    pub max_loops: usize,

    /// Concurrent LLM API calls.
    #[serde(rename = "max-api-calls")]
    pub max_api_calls: usize,

    /// Maximum git worktrees.
    #[serde(rename = "max-worktrees")]
    pub max_worktrees: usize,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_loops: 50,
            max_api_calls: 10,
            max_worktrees: 50,
        }
    }
}

/// Validation defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ValidationConfig {
    /// Default validation command.
    pub command: String,

    /// Timeout per iteration in milliseconds.
    #[serde(rename = "iteration-timeout-ms")]
    pub iteration_timeout_ms: u64,

    /// Maximum iterations per loop.
    #[serde(rename = "max-iterations")]
    pub max_iterations: u32,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            command: crate::config::DEFAULT_VALIDATION_COMMAND.to_string(),
            iteration_timeout_ms: 300_000, // 5 minutes
            max_iterations: 100,
        }
    }
}

/// Progress tracking settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProgressConfig {
    /// Progress tracking strategy.
    pub strategy: String,

    /// Maximum progress entries to retain.
    #[serde(rename = "max-entries")]
    pub max_entries: usize,

    /// Maximum characters per output capture.
    #[serde(rename = "max-output-chars")]
    pub max_output_chars: usize,
}

impl Default for ProgressConfig {
    fn default() -> Self {
        Self {
            strategy: "system-captured".to_string(),
            max_entries: 5,
            max_output_chars: 500,
        }
    }
}

/// Git settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GitConfig {
    /// Directory for git worktrees.
    #[serde(rename = "worktree-dir")]
    pub worktree_dir: PathBuf,

    /// Disk quota for worktrees in GB.
    #[serde(rename = "disk-quota-gb")]
    pub disk_quota_gb: u64,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            worktree_dir: PathBuf::from("/tmp/loopr/worktrees"),
            disk_quota_gb: 100,
        }
    }
}

/// Storage settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageConfig {
    /// TaskStore data directory.
    #[serde(rename = "taskstore-dir")]
    pub taskstore_dir: PathBuf,

    /// JSONL file size warning threshold (MB).
    #[serde(rename = "jsonl-warn-mb")]
    pub jsonl_warn_mb: u64,

    /// JSONL file size error threshold (MB).
    #[serde(rename = "jsonl-error-mb")]
    pub jsonl_error_mb: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        let default_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("loopr");

        Self {
            taskstore_dir: default_dir,
            jsonl_warn_mb: 100,
            jsonl_error_mb: 500,
        }
    }
}

/// Loop type search paths.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LoopsConfig {
    /// Paths to search for loop type definitions.
    pub paths: Vec<String>,
}

impl Default for LoopsConfig {
    fn default() -> Self {
        Self {
            paths: vec![
                "builtin".to_string(),
                "~/.config/loopr/loops".to_string(),
                ".loopr/loops".to_string(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = GlobalConfig::default();
        assert_eq!(config.concurrency.max_loops, 50);
        assert_eq!(config.validation.max_iterations, 100);
        assert_eq!(config.llm.timeout_ms, 300_000);
    }

    #[test]
    fn test_config_validation() {
        let config = GlobalConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_invalid_config() {
        let config = GlobalConfig {
            concurrency: ConcurrencyConfig {
                max_loops: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_parse_yaml() {
        let yaml = r#"
llm:
  default: anthropic/claude-opus-4-5-20250514
  timeout-ms: 60000
concurrency:
  max-loops: 25
"#;
        let config: GlobalConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.llm.timeout_ms, 60000);
        assert_eq!(config.concurrency.max_loops, 25);
        // Other fields should have defaults
        assert_eq!(config.validation.max_iterations, 100);
    }
}
