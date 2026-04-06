#![deny(clippy::unwrap_used)]
// manual_async_fn is suppressed because our LlmClient/AgenticLlm traits require
// explicit fn -> impl Future + Send + 'a signatures to guarantee Send on futures.
// Using async fn would lose the Send bound and break thread safety.
#![allow(clippy::manual_async_fn)]

pub mod agents;
pub mod clarity;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod decomposer;
pub mod domain;
pub mod error;
pub mod evaluator;
pub mod guidance;
pub mod id;
pub mod ipc;
pub mod prompts;
pub mod session_summary;
pub mod tools;
pub mod tui;
pub mod validator;
pub mod worktree;

#[allow(clippy::unwrap_used)]
#[cfg(test)]
pub mod test_util;
#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;

use std::fs;
use std::path::PathBuf;

use eyre::Context;
use log::LevelFilter;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;

use crate::config::Config;

/// Build-time version from `git describe --tags --always`.
/// This is the single source of truth for version identity across TUI and daemon.
pub fn version() -> &'static str {
    env!("GIT_DESCRIBE")
}

/// Handle returned from `setup_logging`. The `guard` field must remain alive
/// for the duration of the process - dropping it flushes the non-blocking
/// writer and stops log output.
pub struct LogHandle {
    pub log_path: PathBuf,
    /// Must not be dropped before end of process.
    pub guard: WorkerGuard,
}

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
pub fn setup_logging(
    config: &Config,
    cli_log_level: Option<&str>,
    session_id: Option<&str>,
) -> eyre::Result<LogHandle> {
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

    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .context("Failed to open log file")?;

    let level = resolve_log_level(config, cli_log_level);
    let tracing_level = to_tracing_level_filter(level);

    // Bridge: forwards `log` crate events from dependencies into the tracing subscriber.
    tracing_log::LogTracer::init().ok();

    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(tracing_level);

    // `.ok()` prevents panic if called twice (e.g. in test harnesses).
    tracing::subscriber::set_global_default(subscriber).ok();

    info!(
        "Logging initialized (level: {}), writing to: {}",
        level,
        log_file.display()
    );
    Ok(LogHandle {
        log_path: log_file,
        guard,
    })
}

fn to_tracing_level_filter(level: LevelFilter) -> tracing_subscriber::filter::LevelFilter {
    match level {
        LevelFilter::Off => tracing_subscriber::filter::LevelFilter::OFF,
        LevelFilter::Error => tracing_subscriber::filter::LevelFilter::ERROR,
        LevelFilter::Warn => tracing_subscriber::filter::LevelFilter::WARN,
        LevelFilter::Info => tracing_subscriber::filter::LevelFilter::INFO,
        LevelFilter::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
        LevelFilter::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
    }
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
