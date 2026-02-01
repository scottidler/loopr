# Tool System

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/tools.md (adapted for runner execution)

---

## Summary

Tools provide file system access, command execution, and web capabilities to loops. In v2, tools are **routed through runners** based on their lane assignment in `catalog.toml`.

---

## Architecture

```
Loop (in daemon)
    │
    │ tool_call from LLM
    ▼
ToolRouter
    │
    │ determines lane from catalog
    ▼
Runner (no-net | net | heavy)
    │
    │ executes in sandbox
    ▼
ToolResult back to loop
```

---

## Tool Catalog

Tools defined in `~/.config/loopr/catalog.toml`:

```toml
[tools.read_file]
lane = "no-net"
timeout_ms = 10000
max_output_bytes = 100000
description = "Read file contents with line numbers"

[tools.write_file]
lane = "no-net"
timeout_ms = 10000
description = "Write content to file"

[tools.edit_file]
lane = "no-net"
timeout_ms = 10000
description = "Replace string in file (requires prior read)"

[tools.list_directory]
lane = "no-net"
timeout_ms = 5000
description = "List directory contents"

[tools.glob]
lane = "no-net"
timeout_ms = 30000
max_output_bytes = 50000
description = "Find files matching pattern"

[tools.grep]
lane = "no-net"
timeout_ms = 60000
max_output_bytes = 100000
description = "Search file contents with regex"

[tools.run_command]
lane = "net"
timeout_ms = 120000
max_output_bytes = 100000
description = "Execute shell command"

[tools.run_command_isolated]
lane = "no-net"
timeout_ms = 120000
max_output_bytes = 100000
description = "Execute command without network"

[tools.web_fetch]
lane = "net"
timeout_ms = 30000
max_output_bytes = 500000
description = "Fetch URL content"

[tools.web_search]
lane = "net"
timeout_ms = 30000
max_output_bytes = 50000
description = "Web search via API"

[tools.build]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 1000000
description = "Run build command"

[tools.test]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 1000000
description = "Run test suite"

[tools.validate]
lane = "heavy"
timeout_ms = 600000
max_output_bytes = 500000
description = "Run validation command"

[tools.complete_task]
lane = "no-net"
timeout_ms = 1000
description = "Signal task completion"

# Artifact creation tools (see artifact-tools.md)
[tools.create_plan]
lane = "no-net"
timeout_ms = 5000
description = "Create plan with specs (structured output)"

[tools.create_spec]
lane = "no-net"
timeout_ms = 5000
description = "Create spec with phases (structured output)"

[tools.create_phase]
lane = "no-net"
timeout_ms = 5000
description = "Create phase with tasks (structured output)"
```

---

## Tool Definitions (for LLM)

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolCatalog {
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "read_file".into(),
                description: "Read file contents with line numbers. Required before editing.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path relative to worktree" },
                        "offset": { "type": "integer", "description": "Start line (1-indexed)" },
                        "limit": { "type": "integer", "description": "Max lines (default: 2000)" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "write_file".into(),
                description: "Write content to file. Creates parent directories.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "File path" },
                        "content": { "type": "string", "description": "Content to write" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDefinition {
                name: "edit_file".into(),
                description: "Replace string in file. Must read_file first.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "description": "Exact string to find" },
                        "new_string": { "type": "string", "description": "Replacement" },
                        "replace_all": { "type": "boolean", "description": "Replace all occurrences" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            ToolDefinition {
                name: "glob".into(),
                description: "Find files matching pattern (e.g., **/*.rs)".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path": { "type": "string", "description": "Base directory (default: worktree)" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "grep".into(),
                description: "Search file contents with regex".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Regex pattern" },
                        "path": { "type": "string", "description": "File or directory" },
                        "file_pattern": { "type": "string", "description": "Glob filter (e.g., *.rs)" },
                        "context": { "type": "integer", "description": "Context lines" }
                    },
                    "required": ["pattern"]
                }),
            },
            ToolDefinition {
                name: "run_command".into(),
                description: "Execute shell command".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout_ms": { "type": "integer" }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "complete_task".into(),
                description: "Signal task completion".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "summary": { "type": "string", "description": "What was accomplished" }
                    },
                    "required": ["summary"]
                }),
            },
            // Artifact tools - see artifact-tools.md for full schemas
            ToolDefinition {
                name: "create_plan".into(),
                description: "Create plan with specs. Structured output - no markdown parsing.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "overview": { "type": "string" },
                        "specs": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "title": { "type": "string" },
                                    "description": { "type": "string" },
                                    "dependencies": { "type": "array", "items": { "type": "string" } }
                                },
                                "required": ["name", "title", "description"]
                            }
                        }
                    },
                    "required": ["title", "overview", "specs"]
                }),
            },
            ToolDefinition {
                name: "create_spec".into(),
                description: "Create spec with phases. Structured output - no markdown parsing.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "overview": { "type": "string" },
                        "phases": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "title": { "type": "string" },
                                    "description": { "type": "string" },
                                    "validation": { "type": "string" }
                                },
                                "required": ["name", "title", "description"]
                            },
                            "minItems": 3,
                            "maxItems": 7
                        }
                    },
                    "required": ["name", "title", "overview", "phases"]
                }),
            },
            ToolDefinition {
                name: "create_phase".into(),
                description: "Create phase with tasks. Structured output - no markdown parsing.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "objective": { "type": "string" },
                        "tasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "description": { "type": "string" },
                                    "file": { "type": "string" },
                                    "action": { "type": "string", "enum": ["create", "modify", "delete"] }
                                },
                                "required": ["description"]
                            }
                        },
                        "validation_command": { "type": "string" }
                    },
                    "required": ["name", "title", "objective", "tasks", "validation_command"]
                }),
            },
            // ... more tools
        ]
    }
}
```

---

## Tool Router

Routes tool calls to appropriate runners:

```rust
pub struct ToolRouter {
    runners: HashMap<RunnerLane, RunnerConnection>,
    catalog: ToolCatalog,
    pending_jobs: HashMap<String, PendingJob>,
}

impl ToolRouter {
    pub async fn submit(
        &mut self,
        lane: RunnerLane,
        job: ToolJob,
    ) -> Result<ToolResult> {
        let runner = self.runners.get_mut(&lane)
            .ok_or_else(|| eyre!("No runner for lane {:?}", lane))?;

        // Send job
        runner.send_job(&job).await?;

        // Track pending
        let (tx, rx) = oneshot::channel();
        self.pending_jobs.insert(job.job_id.clone(), PendingJob { tx });

        // Wait for result
        let result = rx.await?;
        Ok(result)
    }

    pub async fn handle_result(&mut self, result: ToolResult) -> Result<()> {
        if let Some(pending) = self.pending_jobs.remove(&result.job_id) {
            let _ = pending.tx.send(result);
        }
        Ok(())
    }
}
```

---

## Building Tool Commands

```rust
impl ToolCatalog {
    pub fn build_command(&self, tool_call: &ToolCall) -> Result<String> {
        match tool_call.name.as_str() {
            "read_file" => {
                let path = tool_call.input["path"].as_str().unwrap();
                let offset = tool_call.input["offset"].as_u64().unwrap_or(1);
                let limit = tool_call.input["limit"].as_u64().unwrap_or(2000);
                Ok(format!(
                    "loopr-tool read_file --path {} --offset {} --limit {}",
                    shell_escape(path), offset, limit
                ))
            }
            "write_file" => {
                let path = tool_call.input["path"].as_str().unwrap();
                // Content passed via stdin or temp file
                Ok(format!("loopr-tool write_file --path {}", shell_escape(path)))
            }
            "edit_file" => {
                let path = tool_call.input["path"].as_str().unwrap();
                let replace_all = tool_call.input["replace_all"].as_bool().unwrap_or(false);
                Ok(format!(
                    "loopr-tool edit_file --path {} {}",
                    shell_escape(path),
                    if replace_all { "--replace-all" } else { "" }
                ))
            }
            "glob" => {
                let pattern = tool_call.input["pattern"].as_str().unwrap();
                let path = tool_call.input["path"].as_str().unwrap_or(".");
                Ok(format!(
                    "loopr-tool glob --pattern {} --path {}",
                    shell_escape(pattern), shell_escape(path)
                ))
            }
            "grep" => {
                let pattern = tool_call.input["pattern"].as_str().unwrap();
                Ok(format!("rg --line-number {}", shell_escape(pattern)))
            }
            "run_command" => {
                let command = tool_call.input["command"].as_str().unwrap();
                Ok(command.to_string())
            }
            _ => Err(eyre!("Unknown tool: {}", tool_call.name)),
        }
    }
}
```

---

## Tool Implementations

Tools are implemented in `loopr-tool` binary:

```rust
// loopr-tool/src/main.rs
fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Command::ReadFile { path, offset, limit } => {
            read_file(&path, offset, limit)?;
        }
        Command::WriteFile { path } => {
            let content = io::stdin().read_to_string()?;
            write_file(&path, &content)?;
        }
        Command::EditFile { path, replace_all } => {
            let input: EditInput = serde_json::from_reader(io::stdin())?;
            edit_file(&path, &input.old_string, &input.new_string, replace_all)?;
        }
        Command::Glob { pattern, path } => {
            glob_files(&pattern, &path)?;
        }
    }

    Ok(())
}

fn read_file(path: &Path, offset: usize, limit: usize) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    for (i, line) in content.lines().skip(offset - 1).take(limit).enumerate() {
        let line_num = offset + i;
        let truncated = if line.len() > 2000 {
            format!("{}...", &line[..2000])
        } else {
            line.to_string()
        };
        println!("{:>6}|{}", line_num, truncated);
    }
    Ok(())
}

fn write_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    println!("Wrote {} bytes to {}", content.len(), path.display());
    Ok(())
}

fn edit_file(path: &Path, old: &str, new: &str, replace_all: bool) -> Result<()> {
    let content = std::fs::read_to_string(path)?;

    if !content.contains(old) {
        eprintln!("old_string not found in file");
        std::process::exit(1);
    }

    if !replace_all && content.matches(old).count() > 1 {
        eprintln!("old_string found {} times, use --replace-all", content.matches(old).count());
        std::process::exit(1);
    }

    let new_content = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };

    std::fs::write(path, &new_content)?;
    println!("Edited {}", path.display());
    Ok(())
}
```

---

## Path Validation

Runners enforce path constraints:

```rust
fn validate_path(path: &Path, worktree: &Path) -> Result<PathBuf> {
    let normalized = if path.is_absolute() {
        path.to_path_buf()
    } else {
        worktree.join(path)
    };

    let canonical = normalized.canonicalize()
        .unwrap_or_else(|_| normalized.clone());

    let worktree_canonical = worktree.canonicalize()?;

    if canonical.starts_with(&worktree_canonical) {
        Ok(canonical)
    } else {
        Err(eyre!("Path {} escapes worktree {}", path.display(), worktree.display()))
    }
}
```

---

## Read Tracking (for edit validation)

Daemon tracks which files a loop has read:

```rust
pub struct LoopContext {
    pub worktree: PathBuf,
    pub read_files: HashSet<PathBuf>,
}

impl LoopContext {
    pub fn track_read(&mut self, path: &Path) {
        self.read_files.insert(self.normalize(path));
    }

    pub fn was_read(&self, path: &Path) -> bool {
        self.read_files.contains(&self.normalize(path))
    }

    pub fn clear_reads(&mut self) {
        self.read_files.clear();
    }
}
```

---

## Loop Type Tool Configuration

Each loop type specifies available tools. All loops have access to the full Claude Code toolset, plus artifact tools for structured output.

```yaml
# loopr.yml
loops:
  plan:
    tools:
      - read_file
      - write_file
      - glob
      - grep
      - create_plan      # Artifact tool (structured output)
      - complete_task
    # No run_command for plans

  spec:
    tools:
      - read_file
      - write_file
      - edit_file
      - list_directory
      - glob
      - grep
      - run_command
      - create_spec      # Artifact tool (structured output)
      - complete_task

  phase:
    tools:
      - read_file
      - write_file
      - edit_file
      - list_directory
      - glob
      - grep
      - run_command
      - create_phase     # Artifact tool (structured output)
      - complete_task

  code:
    tools:
      - read_file
      - write_file
      - edit_file
      - list_directory
      - glob
      - grep
      - run_command
      - build
      - test
      - complete_task
```

---

## References

- [artifact-tools.md](artifact-tools.md) - Artifact creation via tool_use
- [runners.md](runners.md) - Runner execution
- [tool-catalog.md](tool-catalog.md) - Catalog format
- [ipc-protocol.md](ipc-protocol.md) - Job protocol
