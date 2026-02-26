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
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            repo_path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "loopr".to_string(),
            debug: false,
            daemon: DaemonConfig::default(),
            project: ProjectConfig::default(),
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
}
