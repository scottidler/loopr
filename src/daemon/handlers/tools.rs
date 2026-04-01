use std::sync::Arc;

use log::{debug, info, warn};
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

        // Extract the base executable from the command string.
        // "busted --verbose" -> "busted"
        // "/usr/bin/lua test.lua" -> "/usr/bin/lua"
        // "./scripts/test.sh -v" -> "./scripts/test.sh"
        let executable = command.split_whitespace().next().unwrap_or(&command);

        // Optional worktree context for resolving relative paths.
        let context_dir = req
            .params
            .get("context_dir")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from);

        // Three-branch validation based on executable form.
        let exe_valid = if executable.starts_with('/') {
            // Absolute path: check filesystem directly.
            std::path::Path::new(executable).exists()
        } else if executable.contains('/') {
            // Relative path: resolve against worktree if provided.
            match &context_dir {
                Some(dir) => dir.join(executable).exists(),
                None => {
                    warn!(
                        "Cannot validate relative path '{}' without context_dir; accepting on faith",
                        executable,
                    );
                    true
                }
            }
        } else {
            // Bare command: check PATH via command -v.
            let check = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("command -v '{}'", executable.replace('\'', "'\\''")))
                .output();
            matches!(check, Ok(output) if output.status.success())
        };

        if !exe_valid {
            let hint = if executable.contains('/') {
                format!(
                    "File '{}' does not exist{}.",
                    executable,
                    context_dir
                        .as_ref()
                        .map_or(String::new(), |d| format!(" (resolved from {:?})", d)),
                )
            } else {
                format!("Executable '{}' not found in PATH.", executable)
            };
            return Ok(DaemonResponse::err(
                req.id,
                RpcError::invalid_params(&format!(
                    "{} The command '{}' cannot be registered because the base \
                     executable does not exist in this environment. \
                     Use your file search tools to discover what testing \
                     frameworks or tools are actually installed, then \
                     register the correct command.",
                    hint, command
                )),
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

    // --- Bare command: valid ---

    #[test]
    fn test_tools_register_valid_bare_command() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "echo hello",
                "timeout_secs": 300,
                "worktree": true
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "tools.register failed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["name"], "test");
        assert_eq!(result["command"], "echo hello");
        assert_eq!(result["source"], "runtime");

        let rt = stores.read_runtime_tools().unwrap();
        assert!(rt.contains_key("test"));
        assert_eq!(rt["test"].command, "echo hello");
    }

    // --- Bare command: missing (the busted scenario) ---

    #[test]
    fn test_tools_register_missing_bare_command_rejected() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "definitely_not_a_real_command_xyz --verbose",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error(), "should reject missing executable");
        let msg = resp.error.unwrap().message;
        assert!(
            msg.contains("definitely_not_a_real_command_xyz"),
            "error should name the executable"
        );
        assert!(msg.contains("not found in PATH"), "error should explain why");
        assert!(msg.contains("file search tools"), "error should suggest next steps");
    }

    // --- Error message is instructive ---

    #[test]
    fn test_tools_register_error_is_instructive() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "busted --verbose",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error());
        let msg = resp.error.unwrap().message;
        // All three instructive elements from the design doc:
        assert!(msg.contains("busted"), "should name the executable");
        assert!(msg.contains("cannot be registered"), "should explain what failed");
        assert!(msg.contains("discover what testing"), "should guide next steps");
    }

    // --- Absolute path: valid ---

    #[test]
    fn test_tools_register_valid_absolute_path() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "/bin/sh test.sh",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "should accept valid absolute path: {:?}", resp.error);
    }

    // --- Absolute path: missing ---

    #[test]
    fn test_tools_register_missing_absolute_path_rejected() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "/nonexistent/path/tool --flag",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error(), "should reject missing absolute path");
        let msg = resp.error.unwrap().message;
        assert!(msg.contains("/nonexistent/path/tool"));
        assert!(msg.contains("does not exist"));
    }

    // --- Relative path: with context_dir, file exists ---

    #[test]
    fn test_tools_register_relative_path_with_context_dir() {
        let tmp = crate::test_util::TestDir::new("loopr-tool-relpath");
        let scripts_dir = tmp.join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::write(scripts_dir.join("test.sh"), "#!/bin/sh\necho ok").unwrap();

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "./scripts/test.sh --verbose",
                "context_dir": tmp.display().to_string(),
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(
            !resp.is_error(),
            "should accept relative path in context_dir: {:?}",
            resp.error
        );
    }

    // --- Relative path: with context_dir, file missing ---

    #[test]
    fn test_tools_register_relative_path_missing_in_context_dir() {
        let tmp = crate::test_util::TestDir::new("loopr-tool-relpath-miss");

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "./scripts/test.sh",
                "context_dir": tmp.display().to_string(),
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(resp.is_error(), "should reject missing relative path");
        let msg = resp.error.unwrap().message;
        assert!(msg.contains("./scripts/test.sh"));
        assert!(msg.contains("does not exist"));
    }

    // --- Relative path: without context_dir (accepted with warning) ---

    #[test]
    fn test_tools_register_relative_path_no_context_dir_accepted() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "./scripts/test.sh",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(
            !resp.is_error(),
            "should accept relative path without context_dir: {:?}",
            resp.error
        );
    }

    // --- First-token extraction ---

    #[test]
    fn test_tools_register_extracts_first_token() {
        // "lua test_todo.lua --verbose" should check "lua", which exists
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        // Use "echo" as a universally available command with extra args
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "echo test_todo.lua --verbose",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error(), "should validate first token only: {:?}", resp.error);
    }

    // --- Structural validation ---

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
                "command": "echo ok"
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

    // --- Rebuilds tool_runner ---

    #[test]
    fn test_tools_register_rebuilds_tool_runner() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();

        let runner = stores.read_tool_runner().unwrap();
        assert!(runner.get_tool("my-echo").is_none());
        drop(runner);

        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "my-echo",
                "command": "echo",
            }),
        );
        let resp = dispatch(&stores, &tx, &wm, &test_integrator_config(), req);
        assert!(!resp.is_error());

        let runner = stores.read_tool_runner().unwrap();
        let tool = runner.get_tool("my-echo");
        assert!(tool.is_some(), "my-echo should be in rebuilt ToolRunner");
        assert_eq!(tool.unwrap().command, "echo");
    }

    // --- Config wins over runtime ---

    #[test]
    fn test_tools_register_config_wins() {
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

        // Register a runtime "test" tool with a valid command
        let req = DaemonRequest::new(
            1,
            "tools.register",
            json!({
                "name": "test",
                "command": "echo test-runtime",
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
