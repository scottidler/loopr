pub mod builtin;
pub mod config;
pub mod denylist;
pub mod error;
pub mod lane;
pub mod router;
pub mod sandbox;
pub mod schema;
pub mod shell;
pub mod spawn;
pub mod tool;

pub use builtin::{Bash, Edit, Glob, Grep, Read, Write};
pub use config::{DenyEntryConfig, ToolsConfig};
pub use denylist::{BashDenylist, DenyPattern, TokenMatcher};
pub use error::ToolError;
pub use lane::{Lane, LanePolicy, classify};
pub use router::{LaneRouter, RouterInitError};
pub use sandbox::{SandboxMode, bwrap_command, detect_bwrap_functional};
pub use schema::ToolSchema;
pub use shell::sh_command;
pub use spawn::{KillStrategy, MAX_INLINE_OUTPUT, PersistConfig, SpawnResult, spawn_with_process_group};
pub use tool::{Tool, ToolContext};

/// Dispatch a tool-use call from Anthropic's wire format into the selected
/// tool's typed `Input`, execute it, and re-serialize the typed `Output`.
///
/// Per the design doc (D1): a free function with a `match name` that fans out
/// to a generic `run::<T: Tool>` helper. Adding a new builtin is a five-edit
/// commit: unit struct in `builtin.rs`, `Tool` impl for it, a `match` arm
/// here, an entry in `all_schemas`, and the per-tool module file.
pub async fn dispatch(name: &str, input: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
    match name {
        "read" => run::<Read>(input, ctx).await,
        "write" => run::<Write>(input, ctx).await,
        "edit" => run::<Edit>(input, ctx).await,
        "bash" => run::<Bash>(input, ctx).await,
        "grep" => run::<Grep>(input, ctx).await,
        "glob" => run::<Glob>(input, ctx).await,
        other => Err(ToolError::UnknownTool(other.to_string())),
    }
}

async fn run<T: Tool>(input: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
    let typed: T::Input = serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
    let output = T::execute(typed, ctx).await.map_err(Into::into)?;
    serde_json::to_value(output).map_err(|e| ToolError::SerializeOutput(e.to_string()))
}

/// Return the full schema set for prompt rendering by `agents::ContextBuilder`.
pub fn all_schemas() -> Vec<ToolSchema> {
    vec![
        Read::schema(),
        Write::schema(),
        Edit::schema(),
        Bash::schema(),
        Grep::schema(),
        Glob::schema(),
    ]
}

pub fn schema_for(name: &str) -> Option<ToolSchema> {
    all_schemas().into_iter().find(|s| s.name == name)
}
