use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tracing::instrument;

use crate::builtin::path::{PathError, resolve};
use crate::error::ToolError;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Read a file with line numbers. Defaults to the first 500 lines; \
use offset and limit to paginate through larger files.";

const DEFAULT_LIMIT: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub path: PathBuf,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub content: String,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path escapes sandbox: {0}")]
    SandboxViolation(String),
    #[error("path denied: {0}")]
    PathDenied(String),
    #[error("failed to read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        match e {
            Error::SandboxViolation(s) => Self::SandboxViolation(s),
            Error::PathDenied(s) => Self::PathDenied(s),
            Error::Io { path, source } => Self::Io(std::io::Error::new(
                source.kind(),
                format!("{}: {}", path.display(), source),
            )),
        }
    }
}

#[instrument(
    name = "tool.read",
    level = "debug",
    skip_all,
    fields(
        tool_name = "read",
        lane = "local",
        path = %input.path.display(),
        offset = input.offset.unwrap_or(0),
        limit = input.limit.unwrap_or(DEFAULT_LIMIT as u64),
        working_dir = %ctx.working_dir.display(),
    ),
    err,
)]
pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, Error> {
    let resolved = resolve(&input.path, ctx).map_err(|e| match e {
        PathError::Escape(s) => Error::SandboxViolation(s),
        PathError::Denied(s) => Error::PathDenied(s),
    })?;

    let bytes = tokio::fs::read(&resolved).await.map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;
    let content_full = String::from_utf8_lossy(&bytes).into_owned();
    let lines_total = content_full.lines().count();

    let offset = input.offset.unwrap_or(0) as usize;
    let limit = input.limit.map(|l| l as usize).unwrap_or(DEFAULT_LIMIT);

    let selected: Vec<(usize, &str)> = content_full.lines().enumerate().skip(offset).take(limit).collect();

    let lines_shown = selected.len();
    let truncated = offset + lines_shown < lines_total;

    let mut numbered = String::new();
    for (idx, line) in selected {
        numbered.push_str(&format!("{:6}\t{}\n", idx + 1, line));
    }

    Ok(Output {
        content: numbered,
        lines_shown,
        lines_total,
        truncated,
    })
}

#[cfg(test)]
mod tests;
