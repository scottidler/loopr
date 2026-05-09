use std::path::PathBuf;
use std::time::Instant;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tracing::{debug, instrument};

use crate::builtin::path::{PathError, resolve};
use crate::error::ToolError;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Write content to a file, creating parent directories as needed. \
Overwrites the file if it already exists.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub path: PathBuf,
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: PathBuf,
    pub bytes_written: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path escapes sandbox: {0}")]
    SandboxViolation(String),
    #[error("path denied: {0}")]
    PathDenied(String),
    #[error("failed to write {path}: {source}")]
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
    name = "tool.write",
    level = "debug",
    skip_all,
    fields(
        tool_name = "write",
        lane = "local",
        path = %input.path.display(),
        bytes = input.content.len(),
        working_dir = %ctx.working_dir.display(),
    ),
    err,
)]
pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, Error> {
    let started = Instant::now();
    let resolved = resolve(&input.path, ctx).map_err(|e| match e {
        PathError::Escape(s) => Error::SandboxViolation(s),
        PathError::Denied(s) => Error::PathDenied(s),
    })?;

    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|source| Error::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let bytes = input.content.as_bytes();
    tokio::fs::write(&resolved, bytes).await.map_err(|source| Error::Io {
        path: resolved.clone(),
        source,
    })?;

    debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        bytes = bytes.len(),
        "tool: ok"
    );
    Ok(Output {
        path: resolved,
        bytes_written: bytes.len(),
    })
}

#[cfg(test)]
mod tests;
