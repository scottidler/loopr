# Tool System Specification

**Author:** Scott A. Idler
**Date:** 2026-01-15 (updated 2026-01-25)
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Tools provide file system access, command execution, and coordination capabilities to Ralph loops. Each loop gets a `ToolContext` scoped to its git worktree - tools cannot escape the worktree sandbox.

---

## Core Architecture

```
+-----------------------------------------------------------------+
| Loop Iteration                                                   |
|                                                                  |
|  +--------------+     +--------------+     +------------------+ |
|  | LLM Response |---->| ToolExecutor |---->| ToolContext      | |
|  | (tool_calls) |     |              |     | - worktree: Path | |
|  +--------------+     +------+-------+     | - read_files: Set| |
|                              |             | - exec_id: String| |
|                              v             +------------------+ |
|                       +--------------+                          |
|                       | Tool Result  |                          |
|                       | (JSON)       |                          |
|                       +--------------+                          |
+-----------------------------------------------------------------+
```

---

## ToolContext

Each loop gets its own `ToolContext` that scopes all operations:

```rust
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;
use std::sync::Arc;

/// Execution context for tools - scoped to a single loop
#[derive(Clone)]
pub struct ToolContext {
    /// Git worktree path - all file ops constrained here
    pub worktree: PathBuf,

    /// Loop execution ID (for coordination)
    pub exec_id: String,

    /// Files read this iteration (for edit validation)
    read_files: Arc<Mutex<HashSet<PathBuf>>>,

    /// Whether sandbox mode is enabled (default: true)
    pub sandbox_enabled: bool,
}

impl ToolContext {
    pub fn new(worktree: PathBuf, exec_id: String) -> Self {
        Self {
            worktree,
            exec_id,
            read_files: Arc::new(Mutex::new(HashSet::new())),
            sandbox_enabled: true,
        }
    }

    /// Track that a file was read (enables edit validation)
    pub async fn track_read(&self, path: &Path) {
        let mut read_files = self.read_files.lock().await;
        read_files.insert(self.normalize_path(path));
    }

    /// Check if a file was read (required before edit)
    pub async fn was_read(&self, path: &Path) -> bool {
        let read_files = self.read_files.lock().await;
        read_files.contains(&self.normalize_path(path))
    }

    /// Clear read tracking (called at iteration start)
    pub async fn clear_reads(&self) {
        let mut read_files = self.read_files.lock().await;
        read_files.clear();
    }

    /// Normalize a path relative to worktree
    fn normalize_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.worktree.join(path)
        }
    }

    /// Validate path is within worktree (sandbox enforcement)
    pub fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        let normalized = self.normalize_path(path);
        let canonical = normalized.canonicalize()
            .unwrap_or_else(|_| normalized.clone());

        if !self.sandbox_enabled {
            return Ok(canonical);
        }

        let worktree_canonical = self.worktree.canonicalize()?;
        if canonical.starts_with(&worktree_canonical) {
            Ok(canonical)
        } else {
            Err(ToolError::SandboxViolation {
                path: path.to_path_buf(),
                worktree: self.worktree.clone(),
            }.into())
        }
    }
}
```

---

## Tool Trait

```rust
use async_trait::async_trait;
use serde_json::Value;

/// A tool that can be called by the LLM
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool name (matches LLM tool_use name)
    fn name(&self) -> &'static str;

    /// Human-readable description
    fn description(&self) -> &'static str;

    /// JSON Schema for input parameters
    fn input_schema(&self) -> Value;

    /// Execute the tool
    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult>;
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}
```

---

## Tool Definitions

Converting to Anthropic API format:

```rust
#[derive(Debug, Clone, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    pub fn to_anthropic_schema(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "input_schema": self.input_schema,
        })
    }
}
```

---

## Built-in Tools

### File System Tools

#### read_file

```rust
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str { "read_file" }

    fn description(&self) -> &'static str {
        "Read a file's contents with line numbers. Required before editing."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to read (default: 2000)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = input["path"].as_str()
            .ok_or_else(|| eyre!("path is required"))?;
        let offset = input["offset"].as_u64().unwrap_or(1) as usize;
        let limit = input["limit"].as_u64().unwrap_or(2000) as usize;

        let full_path = ctx.validate_path(Path::new(path))?;

        let content = tokio::fs::read_to_string(&full_path).await
            .map_err(|e| ToolError::FileNotFound {
                path: path.to_string(),
                source: e,
            })?;

        // Track read for edit validation
        ctx.track_read(&full_path).await;

        // Format with line numbers (cat -n style)
        let lines: Vec<_> = content.lines()
            .skip(offset.saturating_sub(1))
            .take(limit)
            .enumerate()
            .map(|(i, line)| {
                let line_num = offset + i;
                let truncated = if line.len() > 2000 {
                    format!("{}...", &line[..2000])
                } else {
                    line.to_string()
                };
                format!("{:>6}|{}", line_num, truncated)
            })
            .collect();

        Ok(ToolResult::success(lines.join("\n")))
    }
}
```

#### write_file

```rust
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str { "write_file" }

    fn description(&self) -> &'static str {
        "Write content to a file. Creates parent directories if needed."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = input["path"].as_str()
            .ok_or_else(|| eyre!("path is required"))?;
        let content = input["content"].as_str()
            .ok_or_else(|| eyre!("content is required"))?;

        let full_path = ctx.validate_path(Path::new(path))?;

        // Create parent directories
        if let Some(parent) = full_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&full_path, content).await?;

        Ok(ToolResult::success(format!("Wrote {} bytes to {}", content.len(), path)))
    }
}
```

#### edit_file

```rust
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str { "edit_file" }

    fn description(&self) -> &'static str {
        "Replace a specific string in a file. Requires prior read_file call."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path relative to worktree"
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact string to find and replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement string"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)"
                }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = input["path"].as_str()
            .ok_or_else(|| eyre!("path is required"))?;
        let old_string = input["old_string"].as_str()
            .ok_or_else(|| eyre!("old_string is required"))?;
        let new_string = input["new_string"].as_str()
            .ok_or_else(|| eyre!("new_string is required"))?;
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);

        let full_path = ctx.validate_path(Path::new(path))?;

        // Must read file first
        if !ctx.was_read(&full_path).await {
            return Ok(ToolResult::error(
                "Must read_file before editing. Read the file first to see current content."
            ));
        }

        let content = tokio::fs::read_to_string(&full_path).await?;

        // Verify old_string exists
        if !content.contains(old_string) {
            return Ok(ToolResult::error(
                "old_string not found in file. Make sure it matches exactly including whitespace."
            ));
        }

        // Verify uniqueness (unless replace_all)
        if !replace_all {
            let count = content.matches(old_string).count();
            if count > 1 {
                return Ok(ToolResult::error(format!(
                    "old_string found {} times. Use replace_all=true or provide more context.",
                    count
                )));
            }
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        tokio::fs::write(&full_path, &new_content).await?;

        let replacements = if replace_all {
            content.matches(old_string).count()
        } else {
            1
        };

        Ok(ToolResult::success(format!(
            "Replaced {} occurrence(s) in {}",
            replacements, path
        )))
    }
}
```

#### list_directory

```rust
pub struct ListDirectoryTool;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &'static str { "list_directory" }

    fn description(&self) -> &'static str {
        "List files and directories in a path."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path relative to worktree (default: .)"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = input["path"].as_str().unwrap_or(".");
        let full_path = ctx.validate_path(Path::new(path))?;

        let mut entries = Vec::new();
        let mut dir = tokio::fs::read_dir(&full_path).await?;

        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().await?;

            let suffix = if metadata.is_dir() { "/" } else { "" };
            entries.push(format!("{}{}", name, suffix));
        }

        entries.sort();

        Ok(ToolResult::success(entries.join("\n")))
    }
}
```

#### glob

```rust
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str { "glob" }

    fn description(&self) -> &'static str {
        "Find files matching a glob pattern (e.g., **/*.rs)"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match"
                },
                "path": {
                    "type": "string",
                    "description": "Base directory (default: worktree root)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str()
            .ok_or_else(|| eyre!("pattern is required"))?;
        let base = input["path"].as_str().unwrap_or(".");

        let base_path = ctx.validate_path(Path::new(base))?;
        let full_pattern = base_path.join(pattern);

        let matches: Vec<_> = glob::glob(full_pattern.to_str().unwrap())?
            .filter_map(|r| r.ok())
            .filter(|p| {
                // Sandbox check
                p.starts_with(&ctx.worktree)
            })
            .map(|p| {
                p.strip_prefix(&ctx.worktree)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string()
            })
            .take(1000) // Limit results
            .collect();

        if matches.is_empty() {
            Ok(ToolResult::success("No matches found"))
        } else {
            Ok(ToolResult::success(matches.join("\n")))
        }
    }
}
```

### Search Tool

#### grep

```rust
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str { "grep" }

    fn description(&self) -> &'static str {
        "Search file contents with regex. Returns matching lines with context."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: worktree)"
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob to filter files (e.g., *.rs)"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context around matches (default: 2)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = input["pattern"].as_str()
            .ok_or_else(|| eyre!("pattern is required"))?;
        let path = input["path"].as_str().unwrap_or(".");
        let file_pattern = input["file_pattern"].as_str();
        let context_lines = input["context"].as_u64().unwrap_or(2) as usize;

        let search_path = ctx.validate_path(Path::new(path))?;

        // Use ripgrep for performance
        let mut cmd = tokio::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg(format!("--context={}", context_lines))
            .arg("--max-count=100")  // Limit matches
            .arg(pattern)
            .arg(&search_path)
            .current_dir(&ctx.worktree);

        if let Some(fp) = file_pattern {
            cmd.arg("--glob").arg(fp);
        }

        let output = cmd.output().await?;

        if output.status.success() || output.status.code() == Some(1) {
            // 0 = matches found, 1 = no matches
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.is_empty() {
                Ok(ToolResult::success("No matches found"))
            } else {
                Ok(ToolResult::success(stdout.to_string()))
            }
        } else {
            Ok(ToolResult::error(String::from_utf8_lossy(&output.stderr).to_string()))
        }
    }
}
```

### Command Execution Tool

#### run_command

```rust
pub struct RunCommandTool;

#[async_trait]
impl Tool for RunCommandTool {
    fn name(&self) -> &'static str { "run_command" }

    fn description(&self) -> &'static str {
        "Execute a shell command in the worktree. Use for git, build tools, tests."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Shell command to execute"
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let command = input["command"].as_str()
            .ok_or_else(|| eyre!("command is required"))?;
        let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(120_000);

        let output = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&ctx.worktree)
                .output()
        ).await??;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let result = if stdout.is_empty() && !stderr.is_empty() {
            stderr.to_string()
        } else if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{}\n\nSTDERR:\n{}", stdout, stderr)
        };

        // Truncate long output
        let truncated = if result.len() > 30_000 {
            format!("{}...\n[truncated, {} chars total]", &result[..30_000], result.len())
        } else {
            result
        };

        if output.status.success() {
            Ok(ToolResult::success(truncated))
        } else {
            Ok(ToolResult::error(format!(
                "Exit code: {}\n{}",
                output.status.code().unwrap_or(-1),
                truncated
            )))
        }
    }
}
```

### Completion Signal Tool

#### complete_task

```rust
pub struct CompleteTaskTool;

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &'static str { "complete_task" }

    fn description(&self) -> &'static str {
        "Signal that the current task/phase is complete. Only use when validation passes."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Brief summary of what was accomplished"
                }
            },
            "required": ["summary"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let summary = input["summary"].as_str()
            .ok_or_else(|| eyre!("summary is required"))?;

        // The loop engine checks this flag and exits if validation passes
        Ok(ToolResult::success(format!(
            "Task marked complete: {}",
            summary
        )))
    }
}
```

---

## ToolExecutor

Manages tool execution for a loop:

```rust
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    /// Create executor with standard tools
    pub fn standard() -> Self {
        let mut tools: HashMap<String, Box<dyn Tool>> = HashMap::new();

        // File system
        tools.insert("read_file".into(), Box::new(ReadFileTool));
        tools.insert("write_file".into(), Box::new(WriteFileTool));
        tools.insert("edit_file".into(), Box::new(EditFileTool));
        tools.insert("list_directory".into(), Box::new(ListDirectoryTool));
        tools.insert("glob".into(), Box::new(GlobTool));

        // Search
        tools.insert("grep".into(), Box::new(GrepTool));

        // Command
        tools.insert("run_command".into(), Box::new(RunCommandTool));

        // Completion
        tools.insert("complete_task".into(), Box::new(CompleteTaskTool));

        Self { tools }
    }

    /// Get tool definitions for LLM
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Execute a tool call
    pub async fn execute(
        &self,
        tool_call: &ToolCall,
        ctx: &ToolContext,
    ) -> ToolResult {
        match self.tools.get(&tool_call.name) {
            Some(tool) => {
                match tool.execute(tool_call.input.clone(), ctx).await {
                    Ok(result) => result,
                    Err(e) => ToolResult::error(format!("Tool error: {}", e)),
                }
            }
            None => ToolResult::error(format!("Unknown tool: {}", tool_call.name)),
        }
    }

    /// Execute multiple tool calls
    pub async fn execute_all(
        &self,
        tool_calls: &[ToolCall],
        ctx: &ToolContext,
    ) -> Vec<(String, ToolResult)> {
        let mut results = Vec::with_capacity(tool_calls.len());

        for call in tool_calls {
            let result = self.execute(call, ctx).await;
            results.push((call.id.clone(), result));
        }

        results
    }
}
```

---

## Loop Type Tool Configuration

Each loop type specifies which tools are available:

```yaml
# In loop definition
phase:
  tools:
    - read_file
    - write_file
    - edit_file
    - list_directory
    - glob
    - grep
    - run_command
    - complete_task

# Plan loop might exclude command execution
plan:
  tools:
    - read_file
    - write_file
    - glob
    - grep
    - complete_task
```

---

## Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("Path {path} escapes worktree {worktree}")]
    SandboxViolation {
        path: PathBuf,
        worktree: PathBuf,
    },

    #[error("File not found: {path}")]
    FileNotFound {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Must read file before editing: {path}")]
    EditWithoutRead { path: String },

    #[error("Command timed out after {timeout_ms}ms")]
    CommandTimeout { timeout_ms: u64 },

    #[error("Tool not found: {name}")]
    UnknownTool { name: String },
}
```

---

## Dependencies

```toml
[dependencies]
glob = "0.3"
async-trait = "0.1"
tokio = { version = "1", features = ["process", "fs", "time", "sync"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and storage
- [loop-config.md](loop-config.md) - Tool configuration per loop type
- [llm-client.md](llm-client.md) - LLM integration and tool calls
