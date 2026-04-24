//! Output formatting for CLI verbs that return structured data.
//!
//! Two formats: JSON and YAML. Default is chosen based on whether stdout
//! is a TTY: YAML for interactive (readable), JSON for pipes (script-friendly).
//! An explicit `--output <fmt>` always wins. See
//! `docs/design/2026-04-23-cli-plumbing-shape.md` §Data Model / Output format.

use std::io::IsTerminal;

use serde::Serialize;

/// Output format for data-returning CLI verbs.
///
/// `clap::ValueEnum` lets `--output json` and `--output yaml` parse directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lower")]
pub enum Format {
    Json,
    Yaml,
}

impl Format {
    /// Resolve the effective format. Explicit flag wins; otherwise the TTY
    /// status of stdout picks: Yaml for interactive, Json for pipes.
    pub fn resolve(explicit: Option<Format>) -> Format {
        resolve_inner(explicit, std::io::stdout().is_terminal())
    }
}

fn resolve_inner(explicit: Option<Format>, stdout_is_tty: bool) -> Format {
    explicit.unwrap_or(if stdout_is_tty { Format::Yaml } else { Format::Json })
}

/// Render a `Serialize` value as either JSON or YAML.
pub fn render<T: Serialize>(value: &T, fmt: Format) -> Result<String, OutputError> {
    match fmt {
        Format::Json => serde_json::to_string_pretty(value).map_err(OutputError::Json),
        Format::Yaml => serde_yaml::to_string(value).map_err(OutputError::Yaml),
    }
}

/// Errors producible by [`render`].
#[derive(thiserror::Error, Debug)]
pub enum OutputError {
    #[error("json render: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml render: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

#[cfg(test)]
mod tests;
