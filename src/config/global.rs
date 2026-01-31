//! Global configuration (Layer 1).
//!
//! Loaded from ~/.config/loopr/loopr.yml or .loopr.yml

use eyre::{Context, Result};
use log::debug;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Global configuration for Loopr.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(rename = "log-level")]
    pub log_level: Option<String>,

    /// LLM provider settings - REQUIRED, no defaults
    pub llm: LlmConfig,

    /// Concurrency limits.
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,

    /// Validation defaults.
    #[serde(default)]
    pub validation: ValidationConfig,

    /// Progress tracking settings.
    #[serde(default)]
    pub progress: ProgressConfig,

    /// Git settings.
    #[serde(default)]
    pub git: GitConfig,

    /// Storage settings.
    #[serde(default)]
    pub storage: StorageConfig,

    /// Loop type search paths.
    #[serde(default)]
    pub loops: LoopsConfig,
}

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            log_level: Some("info".to_string()),
            llm: LlmConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            validation: ValidationConfig::default(),
            progress: ProgressConfig::default(),
            git: GitConfig::default(),
            storage: StorageConfig::default(),
            loops: LoopsConfig::default(),
        }
    }
}

impl GlobalConfig {
    /// Load just the log level from config files (for early logging setup)
    ///
    /// Searches for config files in the standard locations and returns the
    /// log-level value if found. This is called before full config loading
    /// to enable proper logging during startup.
    pub fn load_log_level(config_path: Option<&PathBuf>) -> Option<String> {
        // Note: Cannot use debug! here since logging isn't initialized yet
        // Helper to extract log-level from a file
        fn extract_log_level(path: &Path) -> Option<String> {
            let content = fs::read_to_string(path).ok()?;
            // Quick YAML parse just for log-level
            #[derive(Deserialize)]
            struct LogLevelOnly {
                #[serde(rename = "log-level")]
                log_level: Option<String>,
            }
            let parsed: LogLevelOnly = serde_yaml::from_str(&content).ok()?;
            parsed.log_level
        }

        // If explicit config path provided, try it
        if let Some(path) = config_path
            && let Some(level) = extract_log_level(path)
        {
            return Some(level);
        }

        // Try project-local config: .loopr.yml
        let local_config = PathBuf::from(".loopr.yml");
        if local_config.exists()
            && let Some(level) = extract_log_level(&local_config)
        {
            return Some(level);
        }

        // Try user config: ~/.config/loopr/loopr.yml
        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("loopr").join("loopr.yml");
            if user_config.exists()
                && let Some(level) = extract_log_level(&user_config)
            {
                return Some(level);
            }
        }

        None
    }

    /// Load configuration with fallback chain.
    ///
    /// Search order:
    /// 1. Explicit path if provided
    /// 2. .loopr.yml in current directory
    /// 3. ~/.config/loopr/loopr.yml
    /// 4. Defaults (will fail validation without LLM config)
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        debug!("GlobalConfig::load: config_path={:?}", config_path);

        // Explicit path takes precedence
        if let Some(path) = config_path {
            debug!("GlobalConfig::load: explicit config path provided: {:?}", path);
            return Self::load_from_file(path).context(format!("Failed to load config from {}", path.display()));
        }

        // Try project config
        let project_config = PathBuf::from(".loopr.yml");
        if project_config.exists() {
            debug!("GlobalConfig::load: trying project config: {:?}", project_config);
            match Self::load_from_file(&project_config) {
                Ok(config) => {
                    debug!("GlobalConfig::load: loaded from .loopr.yml");
                    return Ok(config);
                }
                Err(e) => {
                    debug!("GlobalConfig::load: failed to load .loopr.yml: {}", e);
                    log::warn!("Failed to load .loopr.yml: {}", e);
                }
            }
        }

        // Try user config
        if let Some(config_dir) = dirs::config_dir() {
            let user_config = config_dir.join("loopr").join("loopr.yml");
            if user_config.exists() {
                debug!("GlobalConfig::load: trying user config: {:?}", user_config);
                match Self::load_from_file(&user_config) {
                    Ok(config) => {
                        debug!("GlobalConfig::load: loaded from user config");
                        return Ok(config);
                    }
                    Err(e) => {
                        debug!("GlobalConfig::load: failed to load user config: {}", e);
                        log::warn!("Failed to load {}: {}", user_config.display(), e);
                    }
                }
            }
        }

        // Use defaults - will likely fail validation without LLM config
        debug!("GlobalConfig::load: no config file found, using defaults");
        Ok(Self::default())
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        debug!("GlobalConfig::load_from_file: path={:?}", path.as_ref());
        let content = fs::read_to_string(&path).context("Failed to read config file")?;
        let config: Self = serde_yaml::from_str(&content).context("Failed to parse config file")?;
        debug!(
            "GlobalConfig::load_from_file: loaded successfully, llm.default={}",
            config.llm.default
        );
        Ok(config)
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        debug!("GlobalConfig::validate: called");

        if self.concurrency.max_loops == 0 {
            eyre::bail!("concurrency.max_loops must be > 0");
        }
        if self.concurrency.max_api_calls == 0 {
            eyre::bail!("concurrency.max_api_calls must be > 0");
        }
        if self.validation.max_iterations == 0 {
            eyre::bail!("validation.max_iterations must be > 0");
        }

        // Validate LLM config can be resolved
        let _ = self.llm.resolve()?;

        debug!("GlobalConfig::validate: passed");
        Ok(())
    }

    /// Get log level filter
    pub fn log_level_filter(&self) -> log::LevelFilter {
        match self.log_level.as_deref() {
            Some("trace") => log::LevelFilter::Trace,
            Some("debug") => log::LevelFilter::Debug,
            Some("info") => log::LevelFilter::Info,
            Some("warn") => log::LevelFilter::Warn,
            Some("error") => log::LevelFilter::Error,
            _ => log::LevelFilter::Info,
        }
    }
}

/// LLM provider settings.
///
/// The `default` field specifies the provider/model in "provider/model" format.
/// This is REQUIRED - there are no hardcoded defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmConfig {
    /// Default model (provider/model format) - REQUIRED
    pub default: String,

    /// Timeout per LLM call in milliseconds.
    #[serde(rename = "timeout-ms", default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Provider configurations.
    #[serde(default = "default_providers")]
    pub providers: HashMap<String, ProviderConfig>,
}

fn default_timeout_ms() -> u64 {
    300_000 // 5 minutes
}

fn default_providers() -> HashMap<String, ProviderConfig> {
    let mut providers = HashMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderConfig {
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            api_key_file: None,
            base_url: "https://api.anthropic.com".to_string(),
            models: HashMap::new(),
        },
    );
    providers
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            // No default model - MUST be set in config
            default: String::new(),
            timeout_ms: default_timeout_ms(),
            providers: default_providers(),
        }
    }
}

/// Resolved LLM configuration ready for client creation
#[derive(Debug, Clone)]
pub struct ResolvedLlmConfig {
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: Option<String>,
    pub base_url: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

impl ResolvedLlmConfig {
    /// Get the API key from environment variable or file
    pub fn get_api_key(&self) -> Result<String> {
        debug!("ResolvedLlmConfig::get_api_key: api_key_env={}", self.api_key_env);

        // First try environment variable
        if let Ok(key) = std::env::var(&self.api_key_env) {
            debug!("ResolvedLlmConfig::get_api_key: found in environment");
            return Ok(key);
        }

        // Then try file
        if let Some(file_path) = &self.api_key_file {
            let expanded = if file_path.starts_with("~/") {
                dirs::home_dir()
                    .map(|h| h.join(&file_path[2..]))
                    .unwrap_or_else(|| PathBuf::from(file_path))
            } else {
                PathBuf::from(file_path)
            };

            if expanded.exists() {
                let key = fs::read_to_string(&expanded)
                    .context(format!("Failed to read API key from {}", expanded.display()))?
                    .trim()
                    .to_string();
                debug!("ResolvedLlmConfig::get_api_key: found in file {:?}", expanded);
                return Ok(key);
            }
        }

        Err(eyre::eyre!(
            "API key not found. Set the {} environment variable or configure api-key-file in your config",
            self.api_key_env
        ))
    }
}

impl LlmConfig {
    /// Resolve the default provider/model into a flat config ready for client creation
    pub fn resolve(&self) -> Result<ResolvedLlmConfig> {
        debug!("LlmConfig::resolve: default={}", self.default);

        if self.default.is_empty() {
            return Err(eyre::eyre!(
                "No default LLM configured. Set llm.default in your config file (e.g., 'anthropic/claude-sonnet-4-20250514')"
            ));
        }

        let parts: Vec<&str> = self.default.split('/').collect();
        if parts.len() != 2 {
            return Err(eyre::eyre!(
                "Invalid default LLM format '{}'. Expected 'provider/model' (e.g., 'anthropic/claude-sonnet-4-20250514')",
                self.default
            ));
        }

        let provider_name = parts[0];
        let model_name = parts[1];

        let provider = self.providers.get(provider_name).ok_or_else(|| {
            eyre::eyre!(
                "Provider '{}' not found in config. Available: {:?}",
                provider_name,
                self.providers.keys().collect::<Vec<_>>()
            )
        })?;

        // Get model config or use defaults
        let max_tokens = provider.models.get(model_name).map(|m| m.max_tokens).unwrap_or(16384);

        debug!(
            "LlmConfig::resolve: provider={}, model={}, max_tokens={}",
            provider_name, model_name, max_tokens
        );

        Ok(ResolvedLlmConfig {
            provider: provider_name.to_string(),
            model: model_name.to_string(),
            api_key_env: provider.api_key_env.clone(),
            api_key_file: provider.api_key_file.clone(),
            base_url: provider.base_url.clone(),
            max_tokens,
            timeout_ms: self.timeout_ms,
        })
    }

    /// Get the API key (convenience method)
    pub fn get_api_key(&self) -> Result<String> {
        self.resolve()?.get_api_key()
    }
}

/// Provider configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Environment variable for API key.
    #[serde(rename = "api-key-env")]
    pub api_key_env: String,

    /// File path for API key (optional, supports ~ expansion)
    #[serde(rename = "api-key-file")]
    pub api_key_file: Option<String>,

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
        // Default has no model set
        assert!(config.llm.default.is_empty());
    }

    #[test]
    fn test_config_validation_fails_without_llm() {
        let config = GlobalConfig::default();
        // Should fail because no LLM is configured
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_llm_resolve_format_error() {
        let llm = LlmConfig {
            default: "invalid-format".to_string(),
            ..Default::default()
        };
        let result = llm.resolve();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Expected 'provider/model'"));
    }

    #[test]
    fn test_llm_resolve_missing_provider() {
        let llm = LlmConfig {
            default: "openai/gpt-4".to_string(),
            providers: HashMap::new(),
            ..Default::default()
        };
        let result = llm.resolve();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found in config"));
    }

    #[test]
    fn test_llm_resolve_success() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                api_key_file: None,
                base_url: "https://api.anthropic.com".to_string(),
                models: HashMap::new(),
            },
        );

        let llm = LlmConfig {
            default: "anthropic/claude-sonnet-4-20250514".to_string(),
            timeout_ms: 60000,
            providers,
        };

        let resolved = llm.resolve().unwrap();
        assert_eq!(resolved.provider, "anthropic");
        assert_eq!(resolved.model, "claude-sonnet-4-20250514");
        assert_eq!(resolved.timeout_ms, 60000);
    }

    #[test]
    fn test_parse_yaml() {
        let yaml = r#"
log-level: debug
llm:
  default: anthropic/claude-opus-4-5-20251101
  timeout-ms: 60000
  providers:
    anthropic:
      api-key-env: ANTHROPIC_API_KEY
      base-url: https://api.anthropic.com
concurrency:
  max-loops: 25
"#;
        let config: GlobalConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.log_level, Some("debug".to_string()));
        assert_eq!(config.llm.default, "anthropic/claude-opus-4-5-20251101");
        assert_eq!(config.llm.timeout_ms, 60000);
        assert_eq!(config.concurrency.max_loops, 25);
        // Other fields should have defaults
        assert_eq!(config.validation.max_iterations, 100);
    }
}
