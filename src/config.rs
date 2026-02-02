use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub log_level: Option<String>,
    pub llm: LlmConfig,
    pub concurrency: ConcurrencyConfig,
    pub validation: ValidationConfig,
    pub git: GitConfig,
    pub storage: StorageConfig,
    pub runners: RunnersConfig,
    pub tui: TuiConfig,
    pub debug: DebugConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub model: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 8192,
            timeout_ms: 300000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConcurrencyConfig {
    pub max_loops: u32,
    pub max_api_calls: u32,
    pub max_worktrees: u32,
    pub per_type: HashMap<String, u32>,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_loops: 50,
            max_api_calls: 10,
            max_worktrees: 50,
            per_type: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ValidationConfig {
    pub command: String,
    pub iteration_timeout_ms: u64,
    pub max_iterations: u32,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            command: "otto ci".to_string(),
            iteration_timeout_ms: 300000,
            max_iterations: 100,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub worktree_base: String,
    pub main_branch: String,
    pub auto_merge: bool,
    pub preserve_failed_branches: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            worktree_base: ".loopr/worktrees".to_string(),
            main_branch: "main".to_string(),
            auto_merge: false,
            preserve_failed_branches: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub taskstore_dir: PathBuf,
    pub jsonl_warn_mb: u32,
    pub jsonl_error_mb: u32,
    pub disk_quota_min_gb: u32,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            taskstore_dir: dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("loopr"),
            jsonl_warn_mb: 100,
            jsonl_error_mb: 500,
            disk_quota_min_gb: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnersConfig {
    pub no_net: RunnerConfig,
    pub net: RunnerConfig,
    pub heavy: RunnerConfig,
}

impl Default for RunnersConfig {
    fn default() -> Self {
        Self {
            no_net: RunnerConfig {
                slots: 10,
                timeout_default_ms: 30000,
                max_output_bytes: 100000,
            },
            net: RunnerConfig {
                slots: 5,
                timeout_default_ms: 60000,
                max_output_bytes: 100000,
            },
            heavy: RunnerConfig {
                slots: 1,
                timeout_default_ms: 600000,
                max_output_bytes: 1000000,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerConfig {
    pub slots: usize,
    pub timeout_default_ms: u64,
    pub max_output_bytes: usize,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            slots: 10,
            timeout_default_ms: 30000,
            max_output_bytes: 100000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub tick_rate_ms: u64,
    pub scroll_page_size: u32,
    pub socket_path: Option<PathBuf>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            tick_rate_ms: 250,
            scroll_page_size: 10,
            socket_path: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DebugConfig {
    pub save_prompts: bool,
    pub save_responses: bool,
    pub trace_tools: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: Some("info".to_string()),
            llm: LlmConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            validation: ValidationConfig::default(),
            git: GitConfig::default(),
            storage: StorageConfig::default(),
            runners: RunnersConfig::default(),
            tui: TuiConfig::default(),
            debug: DebugConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration with fallback chain
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        // If explicit config path provided, try to load it
        if let Some(path) = config_path {
            return Self::load_from_file(path).context(format!("Failed to load config from {}", path.display()));
        }

        // Try primary location: ~/.config/<project>/<project>.yml
        if let Some(config_dir) = dirs::config_dir() {
            let project_name = env!("CARGO_PKG_NAME");
            let primary_config = config_dir.join(project_name).join(format!("{}.yml", project_name));
            if primary_config.exists() {
                match Self::load_from_file(&primary_config) {
                    Ok(config) => return Ok(config),
                    Err(e) => {
                        log::warn!("Failed to load config from {}: {}", primary_config.display(), e);
                    }
                }
            }
        }

        // Try fallback location: ./<project>.yml
        let project_name = env!("CARGO_PKG_NAME");
        let fallback_config = PathBuf::from(format!("{}.yml", project_name));
        if fallback_config.exists() {
            match Self::load_from_file(&fallback_config) {
                Ok(config) => return Ok(config),
                Err(e) => {
                    log::warn!("Failed to load config from {}: {}", fallback_config.display(), e);
                }
            }
        }

        // No config file found, use defaults
        log::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path).context("Failed to read config file")?;

        let config: Self = serde_yaml::from_str(&content).context("Failed to parse config file")?;

        log::info!("Loaded config from: {}", path.as_ref().display());
        Ok(config)
    }
}
