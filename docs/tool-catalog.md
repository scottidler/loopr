# Tool Catalog

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

The tool catalog (`catalog.toml`) defines all available tools, their runner lane assignments, timeouts, and parameters. This is the single source of truth for tool configuration.

---

## File Location

```
~/.config/loopr/catalog.toml    # User catalog (overrides)
<repo>/.loopr/catalog.toml      # Project catalog (overrides)
<builtin>                        # Default catalog (in binary)
```

Catalogs merge with later entries overriding earlier ones.

---

## Schema

```toml
[tools.<name>]
lane = "no-net" | "net" | "heavy"
timeout_ms = <milliseconds>
max_output_bytes = <bytes>
description = "<human readable>"
enabled = true | false

# Optional: parameter schema
[tools.<name>.params]
required = ["param1", "param2"]
optional = ["param3"]

[tools.<name>.params.param1]
type = "string" | "integer" | "boolean" | "path"
description = "<help text>"
default = <value>
```

---

## Default Catalog

```toml
# File System Tools

[tools.read_file]
lane = "no-net"
timeout_ms = 10000
max_output_bytes = 100000
description = "Read file contents with line numbers. Required before editing."

[tools.read_file.params]
required = ["path"]
optional = ["offset", "limit"]

[tools.read_file.params.path]
type = "path"
description = "File path relative to worktree"

[tools.read_file.params.offset]
type = "integer"
description = "Start line (1-indexed)"
default = 1

[tools.read_file.params.limit]
type = "integer"
description = "Max lines to read"
default = 2000

# ---

[tools.write_file]
lane = "no-net"
timeout_ms = 10000
max_output_bytes = 1000
description = "Write content to file. Creates parent directories."

[tools.write_file.params]
required = ["path", "content"]

[tools.write_file.params.path]
type = "path"
description = "File path"

[tools.write_file.params.content]
type = "string"
description = "Content to write"

# ---

[tools.edit_file]
lane = "no-net"
timeout_ms = 10000
max_output_bytes = 1000
description = "Replace string in file. Requires prior read_file."

[tools.edit_file.params]
required = ["path", "old_string", "new_string"]
optional = ["replace_all"]

[tools.edit_file.params.path]
type = "path"

[tools.edit_file.params.old_string]
type = "string"
description = "Exact string to find"

[tools.edit_file.params.new_string]
type = "string"
description = "Replacement string"

[tools.edit_file.params.replace_all]
type = "boolean"
description = "Replace all occurrences"
default = false

# ---

[tools.list_directory]
lane = "no-net"
timeout_ms = 5000
max_output_bytes = 50000
description = "List directory contents"

[tools.list_directory.params]
optional = ["path"]

[tools.list_directory.params.path]
type = "path"
default = "."

# ---

[tools.glob]
lane = "no-net"
timeout_ms = 30000
max_output_bytes = 50000
description = "Find files matching glob pattern"

[tools.glob.params]
required = ["pattern"]
optional = ["path"]

[tools.glob.params.pattern]
type = "string"
description = "Glob pattern (e.g., **/*.rs)"

[tools.glob.params.path]
type = "path"
description = "Base directory"
default = "."

# ---

[tools.grep]
lane = "no-net"
timeout_ms = 60000
max_output_bytes = 100000
description = "Search file contents with regex"

[tools.grep.params]
required = ["pattern"]
optional = ["path", "file_pattern", "context"]

[tools.grep.params.pattern]
type = "string"
description = "Regex pattern"

[tools.grep.params.path]
type = "path"
description = "File or directory to search"
default = "."

[tools.grep.params.file_pattern]
type = "string"
description = "Glob to filter files (e.g., *.rs)"

[tools.grep.params.context]
type = "integer"
description = "Context lines"
default = 2

# Command Execution

[tools.run_command]
lane = "net"
timeout_ms = 120000
max_output_bytes = 100000
description = "Execute shell command"

[tools.run_command.params]
required = ["command"]
optional = ["timeout_ms"]

[tools.run_command.params.command]
type = "string"
description = "Shell command to execute"

[tools.run_command.params.timeout_ms]
type = "integer"
description = "Override timeout"

# ---

[tools.run_command_isolated]
lane = "no-net"
timeout_ms = 120000
max_output_bytes = 100000
description = "Execute command without network access"

[tools.run_command_isolated.params]
required = ["command"]

# Web Tools

[tools.web_fetch]
lane = "net"
timeout_ms = 30000
max_output_bytes = 500000
description = "Fetch URL content"

[tools.web_fetch.params]
required = ["url"]
optional = ["prompt"]

[tools.web_fetch.params.url]
type = "string"
description = "URL to fetch"

[tools.web_fetch.params.prompt]
type = "string"
description = "What to extract from the page"

# ---

[tools.web_search]
lane = "net"
timeout_ms = 30000
max_output_bytes = 50000
description = "Web search via API"

[tools.web_search.params]
required = ["query"]

[tools.web_search.params.query]
type = "string"
description = "Search query"

# Heavy Tools

[tools.build]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 1000000
description = "Run build command (e.g., cargo build)"

[tools.build.params]
optional = ["command"]

[tools.build.params.command]
type = "string"
description = "Build command"
default = "cargo build"

# ---

[tools.test]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 1000000
description = "Run test suite"

[tools.test.params]
optional = ["command"]

[tools.test.params.command]
type = "string"
description = "Test command"
default = "cargo test"

# ---

[tools.validate]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 500000
description = "Run validation command"

[tools.validate.params]
optional = ["command"]

[tools.validate.params.command]
type = "string"
description = "Validation command"
default = "otto ci"

# Completion

[tools.complete_task]
lane = "no-net"
timeout_ms = 1000
max_output_bytes = 1000
description = "Signal task completion"

[tools.complete_task.params]
required = ["summary"]

[tools.complete_task.params.summary]
type = "string"
description = "What was accomplished"
```

---

## Loading Catalog

```rust
pub struct ToolCatalog {
    tools: HashMap<String, ToolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolConfig {
    pub lane: RunnerLane,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub params: Option<ParamSchema>,
}

impl ToolCatalog {
    pub fn load() -> Result<Self> {
        let mut catalog = Self::default_catalog();

        // Merge user catalog
        let user_path = dirs::config_dir()
            .unwrap()
            .join("loopr/catalog.toml");
        if user_path.exists() {
            catalog.merge_from_file(&user_path)?;
        }

        // Merge project catalog
        let project_path = Path::new(".loopr/catalog.toml");
        if project_path.exists() {
            catalog.merge_from_file(&project_path)?;
        }

        Ok(catalog)
    }

    pub fn get_lane(&self, tool_name: &str) -> Result<RunnerLane> {
        self.tools.get(tool_name)
            .map(|t| t.lane)
            .ok_or_else(|| eyre!("Unknown tool: {}", tool_name))
    }

    pub fn get_timeout(&self, tool_name: &str) -> u64 {
        self.tools.get(tool_name)
            .map(|t| t.timeout_ms)
            .unwrap_or(60000)
    }

    pub fn is_enabled(&self, tool_name: &str) -> bool {
        self.tools.get(tool_name)
            .map(|t| t.enabled)
            .unwrap_or(false)
    }
}
```

---

## Disabling Tools

To disable a tool for a project:

```toml
# .loopr/catalog.toml
[tools.web_fetch]
enabled = false

[tools.web_search]
enabled = false
```

---

## Custom Tools

Add project-specific tools:

```toml
# .loopr/catalog.toml
[tools.deploy]
lane = "heavy"
timeout_ms = 300000
max_output_bytes = 100000
description = "Deploy to staging"

[tools.deploy.params]
optional = ["environment"]

[tools.deploy.params.environment]
type = "string"
default = "staging"
```

---

## Lane Summary

| Lane | Network | Concurrency | Tools |
|------|---------|-------------|-------|
| no-net | Blocked | 10 | read_file, write_file, edit_file, glob, grep, run_command_isolated |
| net | Allowed | 5 | run_command, web_fetch, web_search |
| heavy | Allowed | 1 | build, test, validate |

---

## References

- [tools.md](tools.md) - Tool implementation
- [runners.md](runners.md) - Lane execution
