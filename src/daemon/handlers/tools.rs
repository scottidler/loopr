use std::sync::Arc;

use log::{debug, info};
use serde_json::json;
use tokio::sync::broadcast;

use crate::config::ToolEntry;
use crate::ipc::protocol::{DaemonEvent, DaemonRequest, DaemonResponse, RpcError};
use crate::tools::ToolRunner;

use crate::daemon::context::Stores;

/// Register a runtime tool discovered by an agent. This is Layer 2 in the resolution stack.
/// Runtime tools are session-scoped and not persisted.
pub(super) fn handle_tools_register(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        debug!("handle_tools_register()");

        let name = req
            .params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("name is required"))?
            .to_string();

        if name.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("name must be non-empty"),
            ));
        }

        let command = req
            .params
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("command is required"))?
            .to_string();

        if command.is_empty() {
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params("command must be non-empty"),
            ));
        }

        let timeout_secs = req.params.get("timeout_secs").and_then(|v| v.as_u64()).unwrap_or(300);

        let worktree = req.params.get("worktree").and_then(|v| v.as_bool()).unwrap_or(true);

        let entry = ToolEntry {
            name: name.clone(),
            command: command.clone(),
            timeout_secs,
            worktree,
        };

        // Insert into runtime_tools
        stores.write_runtime_tools()?.insert(name.clone(), entry.clone());

        // Rebuild tool_runner and tool_executor with config + runtime tools
        let runtime_tools = stores.read_runtime_tools()?;
        let mut all_tools: Vec<ToolEntry> = stores.config.agents.tools.clone();
        for (rt_name, rt_entry) in runtime_tools.iter() {
            if !all_tools.iter().any(|t| t.name == *rt_name) {
                all_tools.push(rt_entry.clone());
            }
        }
        drop(runtime_tools);

        *stores
            .tool_runner
            .write()
            .map_err(|_| eyre::eyre!("tool_runner lock poisoned"))? = Arc::new(ToolRunner::new(&all_tools));
        *stores
            .tool_executor
            .write()
            .map_err(|_| eyre::eyre!("tool_executor lock poisoned"))? =
            Arc::new(crate::tools::ToolExecutor::standard(&all_tools));

        info!("Registered runtime tool '{}': {}", name, command);

        let _ = event_tx.send(DaemonEvent::record_created("tool", &name));

        Ok(DaemonResponse::ok(
            req.id,
            json!({
                "name": entry.name,
                "command": entry.command,
                "timeout_secs": entry.timeout_secs,
                "worktree": entry.worktree,
                "source": "runtime",
            }),
        ))
    })
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::daemon::handlers::tests::{test_event_tx, test_integrator_config, test_stores, test_worktree_mgr};
    use crate::ipc::protocol::DaemonRequest;

    #[test]
    fn test_tools_register_success() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "busted --verbose",
                "timeout_secs": 300,
                "worktree": true
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "tools.register failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["name"], "test");
        assert_eq!(result["command"], "busted --verbose");
        assert_eq!(result["source"], "runtime");

        // Verify it's in runtime_tools
        let rt = stores.read_runtime_tools().unwrap();
        assert!(rt.contains_key("test"));
        assert_eq!(rt["test"].command, "busted --verbose");
    }

    #[test]
    fn test_tools_register_empty_name() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "",
                "command": "busted"
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_tools_register_missing_command() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(1, "tools.register", json!({"name": "test"}));
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
    }

    #[test]
    fn test_tools_register_rebuilds_tool_runner() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        // Verify tool doesn't exist yet
        let runner = stores.read_tool_runner().unwrap();
        assert!(runner.get_tool("lua-test").is_none());
        drop(runner);

        // Register it
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "lua-test",
                "command": "busted",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());

        // Verify tool now exists in the rebuilt runner
        let runner = stores.read_tool_runner().unwrap();
        let tool = runner.get_tool("lua-test");
        assert!(tool.is_some(), "lua-test should be in rebuilt ToolRunner");
        assert_eq!(tool.unwrap().command, "busted");
    }

    #[test]
    fn test_tools_register_config_wins() {
        // Set up config with a "test" tool
        let mut config = crate::config::Config::default();
        config.agents.tools = vec![crate::config::ToolEntry {
            name: "test".into(),
            command: "cargo test".into(),
            timeout_secs: 300,
            worktree: true,
        }];

        let mut raw_stores = Stores::new();
        raw_stores.config = config;
        let stores = Arc::new(raw_stores);

        let (tx, _) = broadcast::channel(16);
        let wm = crate::worktree::manager::WorktreeManager::new(
            std::path::PathBuf::from("/tmp"),
            std::path::PathBuf::from("/tmp/wt"),
        );

        // Register a runtime "test" tool (should NOT override config)
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "busted --verbose",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());

        // The config tool should still win in the rebuilt runner
        let runner = stores.read_tool_runner().unwrap();
        let tool = runner.get_tool("test").unwrap();
        assert_eq!(tool.command, "cargo test", "Config tool should win over runtime tool");
    }
}
