use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use tracing::instrument;

use crate::builtin::path::{PathError, resolve};
use crate::error::ToolError;
use crate::lane::Lane;
use crate::spawn::PersistConfig;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Search file contents using `grep -rn` (recursive, line numbers). \
Pattern is a POSIX regular expression. Optional `path` scopes the search; defaults to the \
working directory.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub glob: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub matches: Vec<String>,
    /// D17: stderr from the grep subprocess. grep emits here on
    /// `Permission denied` for unreadable files, binary-file notices, etc.
    /// Surfacing it lets agents reason about WHY a search returned no hits.
    pub stderr: String,
    /// D17: arrival-order interleave of stdout and stderr, mirroring the
    /// SpawnResult shape. Useful when a grep run produces both matches and
    /// permission errors and the agent needs the causal order.
    pub combined_output: String,
    pub exit_code: i32,
    pub truncated: bool,
    pub persisted_output_path: Option<PathBuf>,
}

#[instrument(
    name = "tool.grep",
    level = "debug",
    skip_all,
    fields(
        tool_name = "grep",
        lane = "local",
        pattern = %input.pattern,
        path = ?input.path,
        glob = ?input.glob,
        working_dir = %ctx.working_dir.display(),
    ),
    err,
)]
pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, ToolError> {
    let search_path = match input.path {
        Some(p) => resolve(&p, ctx).map_err(map_path_err)?,
        None => ctx.working_dir.clone(),
    };

    // Build Command::new("grep") directly. No sh -c wrap - the arg vector
    // survives to exec() verbatim, closing v3/v4's string-concatenation
    // shell-injection vector.
    let mut cmd = tokio::process::Command::new("grep");
    cmd.arg("-rn").arg(&input.pattern);
    if let Some(g) = input.glob.as_ref() {
        cmd.arg(format!("--include={g}"));
    }
    cmd.arg(&search_path);
    cmd.current_dir(&ctx.working_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let persist = PersistConfig {
        base: ctx.persist_base.as_deref(),
        invocation_id: ctx.invocation_id,
    };
    let result = ctx
        .router
        .spawn(cmd, Lane::Local, &ctx.working_dir, None, persist)
        .await?;

    let matches = result.stdout.lines().map(|l| l.to_string()).collect::<Vec<_>>();

    Ok(Output {
        matches,
        stderr: result.stderr,
        combined_output: result.combined_output,
        exit_code: result.exit_code,
        truncated: result.truncated,
        persisted_output_path: result.persisted_output_path,
    })
}

fn map_path_err(e: PathError) -> ToolError {
    match e {
        PathError::Escape(s) => ToolError::SandboxViolation(s),
        PathError::Denied(s) => ToolError::PathDenied(s),
    }
}

#[cfg(test)]
mod tests;
