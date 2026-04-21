use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::builtin::path::{PathError, resolve};
use crate::error::ToolError;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Replace a unique occurrence of `old_string` with `new_string` in \
a file. Fails if `old_string` appears zero or more than one time; the agent must include enough \
surrounding context to make the match unique.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub path: PathBuf,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub path: PathBuf,
    pub bytes_written: usize,
    pub replacements: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("path escapes sandbox: {0}")]
    SandboxViolation(String),
    #[error("path denied: {0}")]
    PathDenied(String),
    #[error("failed to read {path}: {source}")]
    ReadIo { path: PathBuf, source: std::io::Error },
    #[error("failed to write {path}: {source}")]
    WriteIo { path: PathBuf, source: std::io::Error },
    #[error("old_string did not match any occurrence in {0}")]
    NoMatch(PathBuf),
    #[error("old_string matched {count} occurrences in {path}; agent must disambiguate")]
    MultipleMatches { path: PathBuf, count: usize },
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        match e {
            Error::SandboxViolation(s) => Self::SandboxViolation(s),
            Error::PathDenied(s) => Self::PathDenied(s),
            Error::ReadIo { path, source } | Error::WriteIo { path, source } => Self::Io(std::io::Error::new(
                source.kind(),
                format!("{}: {}", path.display(), source),
            )),
            Error::NoMatch(p) => {
                Self::ExecutionFailed(format!("old_string did not match any occurrence in {}", p.display()))
            }
            Error::MultipleMatches { path, count } => Self::ExecutionFailed(format!(
                "old_string matched {count} occurrences in {}; agent must disambiguate",
                path.display()
            )),
        }
    }
}

pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, Error> {
    let resolved = resolve(&input.path, ctx).map_err(|e| match e {
        PathError::Escape(s) => Error::SandboxViolation(s),
        PathError::Denied(s) => Error::PathDenied(s),
    })?;

    let bytes = tokio::fs::read(&resolved).await.map_err(|source| Error::ReadIo {
        path: resolved.clone(),
        source,
    })?;
    let before = String::from_utf8_lossy(&bytes).into_owned();

    let count = before.matches(&input.old_string).count();
    match count {
        0 => return Err(Error::NoMatch(resolved)),
        1 => {}
        n => {
            return Err(Error::MultipleMatches {
                path: resolved,
                count: n,
            });
        }
    }

    let after = before.replacen(&input.old_string, &input.new_string, 1);
    let after_bytes = after.as_bytes();
    tokio::fs::write(&resolved, after_bytes)
        .await
        .map_err(|source| Error::WriteIo {
            path: resolved.clone(),
            source,
        })?;

    Ok(Output {
        path: resolved,
        bytes_written: after_bytes.len(),
        replacements: 1,
    })
}

#[cfg(test)]
mod tests;
