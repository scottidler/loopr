use std::path::PathBuf;
use std::time::Instant;

use ::glob::{MatchOptions, Pattern, glob_with};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tracing::{debug, instrument};

use crate::error::ToolError;
use crate::sandbox::SandboxMode;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Find paths matching a glob pattern, relative to the working \
directory. Supports `**` recursive segments and `?` / `[abc]` character classes.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub pattern: String,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid glob pattern: {0}")]
    InvalidPattern(String),
    #[error("glob traversal error: {0}")]
    Traversal(String),
    #[error("path escapes sandbox: {0}")]
    SandboxViolation(String),
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        match e {
            Error::InvalidPattern(s) => Self::InvalidInput(s),
            Error::Traversal(s) => Self::ExecutionFailed(s),
            Error::SandboxViolation(s) => Self::SandboxViolation(s),
        }
    }
}

#[instrument(
    name = "tool.glob",
    level = "debug",
    skip_all,
    fields(
        tool_name = "glob",
        lane = "local",
        pattern = %input.pattern,
        working_dir = %ctx.working_dir.display(),
    ),
    err,
)]
pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, Error> {
    let started = Instant::now();
    // Validate the pattern parses before walking.
    let _ = Pattern::new(&input.pattern).map_err(|e| Error::InvalidPattern(e.to_string()))?;

    let rooted = if PathBuf::from(&input.pattern).is_absolute() {
        input.pattern.clone()
    } else {
        ctx.working_dir.join(&input.pattern).to_string_lossy().into_owned()
    };

    let opts = MatchOptions {
        case_sensitive: true,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    };

    let entries = glob_with(&rooted, opts).map_err(|e| Error::InvalidPattern(e.to_string()))?;

    let working_canonical = ctx
        .working_dir
        .canonicalize()
        .unwrap_or_else(|_| ctx.working_dir.clone());

    let mut paths = Vec::new();
    for entry in entries {
        let p = entry.map_err(|e| Error::Traversal(e.to_string()))?;
        let abs = p.canonicalize().unwrap_or(p.clone());

        if !matches!(ctx.sandbox, SandboxMode::Off) && !abs.starts_with(&working_canonical) {
            return Err(Error::SandboxViolation(abs.display().to_string()));
        }

        let rel = abs.strip_prefix(&working_canonical).unwrap_or(&abs).to_path_buf();
        paths.push(rel);
    }
    debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        match_count = paths.len(),
        "tool: ok"
    );
    Ok(Output { paths })
}

#[cfg(test)]
mod tests;
