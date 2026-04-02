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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{test_agent_context, test_agent_context_with_tools, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};
    use crate::config::ToolEntry;

    use crate::test_util::TestDir;

    #[tokio::test]
    async fn test_execute_action_run_tool() {
        let dir = TestDir::new("loopr-exec-test");

        let entries = vec![ToolEntry {
            name: "echo-test".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 10,
            worktree: true,
        }];
        let stores = test_stores(&dir);
        let ctx = test_agent_context_with_tools(&dir, &stores, AgentKind::Implementer, &entries);

        let action = AgentAction::RunTool {
            tool: "echo-test".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::ToolRun(tool_result) = result {
            assert_eq!(tool_result.exit_code, 0);
            assert_eq!(tool_result.stdout.trim(), "hello");
        } else {
            panic!("expected ToolRun result");
        }
    }

    #[tokio::test]
    async fn test_execute_register_tool() {
        let dir = TestDir::new("loopr-exec-regtool");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Researcher);

        let action = AgentAction::RegisterTool {
            name: "my-echo".to_string(),
            command: "echo hello".to_string(),
            timeout_secs: 300,
            worktree: true,
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ToolRegistered(ref n) if n == "my-echo"),
            "Expected ToolRegistered, got: {:?}",
            result
        );

        let rt = stores.read_runtime_tools().unwrap();
        assert!(rt.contains_key("my-echo"));
        assert_eq!(rt["my-echo"].command, "echo hello");
    }
}
