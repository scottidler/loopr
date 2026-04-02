use std::path::Path;

use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::result::ActionResult;

/// Handle RunTool action.
pub(super) async fn handle_run_tool(
    ctx: &AgentContext,
    worktree_path: &Path,
    tool: &str,
    args: &[String],
) -> Result<ActionResult> {
    let tool_runner = &*ctx.tool_runner;
    let tool_result = tool_runner
        .run(tool, args, worktree_path)
        .await
        .map_err(|e| eyre!("run_tool '{}': {}", tool, e))?;
    Ok(ActionResult::ToolRun(tool_result))
}

/// Handle RegisterTool action.
pub(super) fn handle_register_tool(
    ctx: &AgentContext,
    worktree_path: &Path,
    name: &str,
    command: &str,
    timeout_secs: u64,
    worktree: bool,
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;
    let params = serde_json::json!({
        "name": name,
        "command": command,
        "timeout_secs": timeout_secs,
        "worktree": worktree,
        "context_dir": worktree_path.to_string_lossy(),
    });
    let resp = bridge.request("tools.register", params);
    if resp.is_error() {
        let msg = resp
            .error
            .map(|e| e.message)
            .unwrap_or_else(|| "unknown error".to_string());
        return Ok(ActionResult::ActionError(format!("register_tool '{}': {}", name, msg)));
    }
    // The tools.register IPC rebuilds the stores' tool_runner/tool_executor.
    // Newly spawned agents will pick up the registered tool automatically.
    Ok(ActionResult::ToolRegistered(name.to_string()))
}
