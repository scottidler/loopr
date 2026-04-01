#![deny(clippy::unwrap_used)]

pub mod agents;
pub mod clarity;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod domain;
pub mod error;
pub mod evaluator;
pub mod guidance;
pub mod id;
pub mod ipc;
pub mod manifest;
pub mod prompts;
pub mod session_summary;
pub mod tools;
pub mod tui;
pub mod validator;
pub mod worktree;

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod fsm_correctness_tests;
#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod integration_tests;
#[allow(clippy::unwrap_used)]
#[cfg(test)]
pub mod test_util;

use std::fs;

/// Build-time version from `git describe --tags --always`.
/// This is the single source of truth for version identity across TUI and daemon.
pub fn version() -> &'static str {
    env!("GIT_DESCRIBE")
}
use std::path::PathBuf;

use eyre::Context;
use log::{LevelFilter, info};

use crate::config::Config;

/// Resolve the effective log level from the config < env < CLI hierarchy.
///
/// Priority (highest wins):
/// 1. `cli_log_level` - `--log-level` flag
/// 2. `LOG_LEVEL` env var
/// 3. `config.log_level` - from YAML config
/// 4. Default - `Info`
pub fn resolve_log_level(config: &Config, cli_log_level: Option<&str>) -> LevelFilter {
    let env_level = std::env::var("LOG_LEVEL").ok();
    resolve_log_level_from(config, cli_log_level, env_level.as_deref())
}

fn resolve_log_level_from(config: &Config, cli_log_level: Option<&str>, env_level: Option<&str>) -> LevelFilter {
    // CLI wins
    if let Some(level) = cli_log_level
        && let Some(parsed) = parse_level_filter(level)
    {
        return parsed;
    }

    // Env var overrides config
    if let Some(level) = env_level
        && let Some(parsed) = parse_level_filter(level)
    {
        return parsed;
    }

    // Config baseline
    if let Some(ref config_level) = config.log_level
        && let Some(parsed) = parse_level_filter(config_level)
    {
        return parsed;
    }

    // Default
    LevelFilter::Info
}

fn parse_level_filter(s: &str) -> Option<LevelFilter> {
    match s.to_lowercase().as_str() {
        "trace" => Some(LevelFilter::Trace),
        "debug" => Some(LevelFilter::Debug),
        "info" => Some(LevelFilter::Info),
        "warn" => Some(LevelFilter::Warn),
        "error" => Some(LevelFilter::Error),
        "off" => Some(LevelFilter::Off),
        _ => None,
    }
}

/// Set up logging. When `session_id` is provided (daemon), creates a session
/// directory at `~/.local/share/loopr/sessions/{session_id}/` with `loopr.log`
/// inside it and updates the `latest` symlink. When None (TUI, one-shot CLI),
/// writes to `~/.local/share/loopr/logs/loopr.log` as before.
pub fn setup_logging(config: &Config, cli_log_level: Option<&str>, session_id: Option<&str>) -> eyre::Result<PathBuf> {
    let base_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr");

    let log_file = if let Some(sid) = session_id {
        let session_dir = base_dir.join("sessions").join(sid);
        fs::create_dir_all(session_dir.join("agents")).context("Failed to create session agents directory")?;

        // Update `latest` symlink
        let sessions_dir = base_dir.join("sessions");
        let latest = sessions_dir.join("latest");
        let _ = fs::remove_file(&latest);
        #[cfg(unix)]
        std::os::unix::fs::symlink(sid, &latest).context("Failed to create latest symlink")?;

        session_dir.join("loopr.log")
    } else {
        let log_dir = base_dir.join("logs");
        fs::create_dir_all(&log_dir).context("Failed to create log directory")?;
        log_dir.join("loopr.log")
    };

    let target = Box::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
            .context("Failed to open log file")?,
    );

    let level = resolve_log_level(config, cli_log_level);

    env_logger::Builder::new()
        .filter_level(level)
        .target(env_logger::Target::Pipe(target))
        .format(|buf, record| {
            use std::io::Write;
            let now = chrono::Local::now();
            writeln!(
                buf,
                "[{} {:5} {}] {}",
                now.format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .init();

    info!(
        "Logging initialized (level: {}), writing to: {}",
        level,
        log_file.display()
    );
    Ok(log_file)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod log_level_tests {
    use super::*;
    use log::LevelFilter;

    fn config_with_level(level: Option<&str>) -> Config {
        Config {
            log_level: level.map(String::from),
            ..Config::default()
        }
    }

    #[test]
    fn test_resolve_log_level_default_is_info() {
        let config = config_with_level(None);
        assert_eq!(resolve_log_level_from(&config, None, None), LevelFilter::Info);
    }

    #[test]
    fn test_resolve_log_level_config_debug() {
        let config = config_with_level(Some("debug"));
        assert_eq!(resolve_log_level_from(&config, None, None), LevelFilter::Debug);
    }

    #[test]
    fn test_resolve_log_level_env_overrides_config() {
        let config = config_with_level(Some("debug"));
        assert_eq!(resolve_log_level_from(&config, None, Some("warn")), LevelFilter::Warn);
    }

    #[test]
    fn test_resolve_log_level_cli_overrides_all() {
        let config = config_with_level(Some("error"));
        assert_eq!(
            resolve_log_level_from(&config, Some("trace"), Some("warn")),
            LevelFilter::Trace
        );
    }

    #[test]
    fn test_resolve_log_level_invalid_falls_through() {
        let config = config_with_level(Some("invalid_level"));
        assert_eq!(resolve_log_level_from(&config, None, None), LevelFilter::Info);
    }

    #[test]
    fn test_resolve_log_level_case_insensitive() {
        let config = config_with_level(Some("DEBUG"));
        assert_eq!(resolve_log_level_from(&config, None, None), LevelFilter::Debug);
    }
}
