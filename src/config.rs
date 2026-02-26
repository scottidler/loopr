use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Daemon-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("loopr");
        Self {
            socket_path: base.join("daemon.sock"),
            pid_path: base.join("daemon.pid"),
        }
    }
}

/// Project-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub repo_path: PathBuf,
    pub worktree_dir: PathBuf,
}

/// Integrator configuration — validation commands run during tick validation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IntegratorConfig {
    pub validation_commands: Vec<String>,
}

/// Agent system configuration — LLM agents (Implementer, Reviewer) running as Tokio tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub auto_start_implementer: bool,
    pub auto_start_reviewer: bool,
    pub implementer: AgentRoleConfig,
    pub reviewer: AgentRoleConfig,
    pub tools: Vec<ToolEntry>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start_implementer: false,
            auto_start_reviewer: false,
            implementer: AgentRoleConfig::default_implementer(),
            reviewer: AgentRoleConfig::default_reviewer(),
            tools: vec![
                ToolEntry {
                    name: "test".to_string(),
                    command: "cargo test".to_string(),
                    timeout_secs: 300,
                    worktree: true,
                },
                ToolEntry {
                    name: "clippy".to_string(),
                    command: "cargo clippy -- -D warnings".to_string(),
                    timeout_secs: 120,
                    worktree: true,
                },
                ToolEntry {
                    name: "fmt-check".to_string(),
                    command: "cargo fmt --check".to_string(),
                    timeout_secs: 30,
                    worktree: true,
                },
                ToolEntry {
                    name: "fmt".to_string(),
                    command: "cargo fmt".to_string(),
                    timeout_secs: 30,
                    worktree: true,
                },
                ToolEntry {
                    name: "build".to_string(),
                    command: "cargo build".to_string(),
                    timeout_secs: 300,
                    worktree: true,
                },
            ],
        }
    }
}

/// Per-role agent configuration (Implementer or Reviewer).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentRoleConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub max_iterations: u32,
    pub pool_size: u32,
    pub temperature: f32,
}

impl Default for AgentRoleConfig {
    fn default() -> Self {
        Self::default_implementer()
    }
}

impl AgentRoleConfig {
    pub fn default_implementer() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 8192,
            max_iterations: 20,
            pool_size: 2,
            temperature: 0.3,
        }
    }

    pub fn default_reviewer() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            max_iterations: 5,
            pool_size: 2,
            temperature: 0.1,
        }
    }
}

/// A configured tool available to agents.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
    pub worktree: bool,
}

/// Doc Validator configuration — LLM-powered document validation for quality gates.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.0,
        }
    }
}

impl Default for IntegratorConfig {
    fn default() -> Self {
        Self {
            validation_commands: vec![
                "cargo fmt --check".to_string(),
                "cargo clippy -- -D warnings".to_string(),
                "cargo test".to_string(),
            ],
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            repo_path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worktree_dir: PathBuf::from(".worktrees"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub debug: bool,
    pub daemon: DaemonConfig,
    pub project: ProjectConfig,
    pub integrator: IntegratorConfig,
    pub validator: ValidatorConfig,
    pub agents: AgentConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "loopr".to_string(),
            debug: false,
            daemon: DaemonConfig::default(),
            project: ProjectConfig::default(),
            integrator: IntegratorConfig::default(),
            validator: ValidatorConfig::default(),
            agents: AgentConfig::default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.name, "loopr");
        assert!(!config.debug);
    }

    #[test]
    fn test_config_load_defaults_when_no_file() {
        let config = Config::load(None).expect("should load defaults");
        assert_eq!(config.name, "loopr");
    }

    #[test]
    fn test_daemon_config_default() {
        let dc = DaemonConfig::default();
        assert!(dc.socket_path.ends_with("daemon.sock"));
        assert!(dc.pid_path.ends_with("daemon.pid"));
    }

    #[test]
    fn test_project_config_default() {
        let pc = ProjectConfig::default();
        assert!(pc.repo_path.is_absolute() || pc.repo_path == std::path::Path::new("."));
    }

    #[test]
    fn test_config_has_daemon_and_project() {
        let config = Config::default();
        assert!(config.daemon.socket_path.ends_with("daemon.sock"));
        assert!(!config.project.repo_path.as_os_str().is_empty());
    }

    #[test]
    fn test_integrator_config_default() {
        let ic = IntegratorConfig::default();
        assert!(!ic.validation_commands.is_empty());
        assert!(ic.validation_commands.iter().any(|c| c.contains("cargo test")));
    }

    #[test]
    fn test_config_has_integrator() {
        let config = Config::default();
        assert!(!config.integrator.validation_commands.is_empty());
    }

    #[test]
    fn test_agent_config_default_disabled() {
        let ac = AgentConfig::default();
        assert!(!ac.enabled);
        assert!(!ac.auto_start_implementer);
        assert!(!ac.auto_start_reviewer);
    }

    #[test]
    fn test_agent_config_default_tools() {
        let ac = AgentConfig::default();
        assert_eq!(ac.tools.len(), 5);
        let names: Vec<&str> = ac.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"clippy"));
        assert!(names.contains(&"fmt-check"));
        assert!(names.contains(&"fmt"));
        assert!(names.contains(&"build"));
    }

    #[test]
    fn test_agent_role_config_implementer_defaults() {
        let rc = AgentRoleConfig::default_implementer();
        assert_eq!(rc.max_iterations, 20);
        assert_eq!(rc.pool_size, 2);
        assert_eq!(rc.max_tokens, 8192);
        assert!((rc.temperature - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn test_agent_role_config_reviewer_defaults() {
        let rc = AgentRoleConfig::default_reviewer();
        assert_eq!(rc.max_iterations, 5);
        assert_eq!(rc.pool_size, 2);
        assert_eq!(rc.max_tokens, 4096);
        assert!((rc.temperature - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_tool_entry_fields() {
        let tool = ToolEntry {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            timeout_secs: 300,
            worktree: true,
        };
        assert_eq!(tool.name, "test");
        assert_eq!(tool.command, "cargo test");
        assert_eq!(tool.timeout_secs, 300);
        assert!(tool.worktree);
    }

    #[test]
    fn test_config_has_agents() {
        let config = Config::default();
        assert!(!config.agents.enabled);
        assert!(!config.agents.tools.is_empty());
    }

    #[test]
    fn test_agent_config_deserialize_from_yaml() {
        let yaml = r#"
enabled: true
auto_start_implementer: true
auto_start_reviewer: false
implementer:
  model: "claude-sonnet-4-6"
  api_key_env: "MY_KEY"
  max_tokens: 4096
  max_iterations: 10
  pool_size: 3
  temperature: 0.5
reviewer:
  model: "claude-sonnet-4-6"
  api_key_env: "MY_KEY"
  max_tokens: 2048
  max_iterations: 3
  pool_size: 1
  temperature: 0.0
tools:
  - name: "test"
    command: "cargo test"
    timeout_secs: 60
    worktree: true
"#;
        let ac: AgentConfig = serde_yaml::from_str(yaml).expect("should parse agent config");
        assert!(ac.enabled);
        assert!(ac.auto_start_implementer);
        assert!(!ac.auto_start_reviewer);
        assert_eq!(ac.implementer.pool_size, 3);
        assert_eq!(ac.reviewer.max_iterations, 3);
        assert_eq!(ac.tools.len(), 1);
        assert_eq!(ac.tools[0].name, "test");
    }
}
