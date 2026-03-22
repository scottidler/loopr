# Design Document: Unified Tool System

**Author:** Scott A. Idler
**Date:** 2026-03-04
**Status:** Implemented
**Review Passes Completed:** 5/5
**Supersedes:** `docs/design/2026-03-04-native-tool-use.md` (sections on ToolRunner/ToolExecutor coexistence)

## Summary

Loopr currently has two parallel execution concepts: `ToolRunner` (project-specific build/test/lint commands, subprocess-based) and a proposed `ToolExecutor` (14 LLM-facing built-in tools using Anthropic's `tool_use` protocol). This design unifies them into a single `ToolExecutor` with one `Tool` trait, one registry, and one execution path. The key insight: a tool running in a git worktree and a tool running in CWD are identical — the only difference is the `working_dir` path. This eliminates the artificial split between "project tools" and "LLM tools," and between "thinking plane" and "action plane" at the tool layer.

## Problem Statement

### Background

The original tool-use design doc (`2026-03-04-native-tool-use.md`) proposes adding native Anthropic `tool_use` support with 14 built-in tools. It explicitly keeps the existing `ToolRunner` as a separate system and adds a `shell` built-in that "can delegate to ToolRunner for configured project tools." This creates two tool registries, two dispatch paths, and a wrapper-around-a-wrapper indirection.

Meanwhile, `ToolRunner` (`src/tools/mod.rs`) already does:
- Registry: `HashMap<String, ToolEntry>` — name → command + timeout + worktree flag
- Auto-detection: `detect_or_default()` from project marker files
- Execution: `sh -c <command>` with timeout, output truncation, SIGTERM→SIGKILL
- Lookup: `available_tools()`, `get_tool()`

This is 80% of what `ToolExecutor` needs. The missing 20% is: a `Tool` trait with `input_schema()` for the Anthropic API, and structured `ToolResult` return values.

### Problem

Two tool systems that do the same thing (registry + dispatch + execute) create:
1. **Confusion** — which system handles what? Where does `cargo test` live vs `grep`?
2. **Indirection** — `shell` built-in wrapping `ToolRunner` wrapping `sh -c` is three layers
3. **Inconsistency** — built-in tools use `ToolContext` with sandbox validation; `ToolRunner` has its own path handling
4. **Maintenance burden** — two sets of timeout, truncation, and error handling logic

### Goals

- Single `Tool` trait that all tools implement — built-in (read, write, grep) and configured (test, lint, build)
- Single `ToolExecutor` registry that exports Anthropic API tool definitions and dispatches execution
- `ToolContext` as the universal sandbox for all tool execution
- Configured project tools from `loopr.yml` become first-class `Tool` impls with auto-generated schemas
- Eliminate the standalone `ToolRunner` struct
- Preserve `detect_or_default()` project auto-detection within the unified system

### Non-Goals

- Changing the `loopr.yml` configuration format (existing `[[tools]]` entries work as-is)
- Removing the action-based agent system (that's a separate, gradual migration)
- MCP server integration
- User approval UX for tool execution

## Proposed Solution

### Overview

Replace `ToolRunner` and the proposed `ToolExecutor` with a single unified `ToolExecutor` that holds:
1. **Built-in tools** — `read`, `write`, `edit`, `list`, `grep`, `find`, `shell`, etc. Each is a struct implementing the `Tool` trait.
2. **Configured tools** — each `ToolEntry` from `loopr.yml` (or auto-detected from project markers) is wrapped in a `ConfiguredTool` struct that also implements `Tool`. The LLM sees `test`, `lint`, `build` as first-class tools with schemas.

No `shell` indirection needed for project tools. `cargo test` shows up directly as a `test` tool in the Anthropic API's tool list. A generic `shell` tool remains only for ad-hoc commands.

### Key Insight: A Worktree Is Just a Path

A tool running in a git worktree and a tool running in CWD are the same thing — the only difference is the `working_dir: PathBuf` passed to the tool. The tool doesn't know or care whether that path is a worktree, a repo root, or any other directory. This means:

- **Implementer** agent → `ToolContext { working_dir: "/tmp/worktrees/work-42" }`
- **Researcher** agent → `ToolContext { working_dir: "/home/user/repos/myproject" }`
- **TUI Chat** → `ToolContext { working_dir: "/home/user/repos/myproject" }`

Same `ReadTool`, same `GrepTool`, same `ConfiguredTool`. The "thinking plane vs action plane" distinction is not a tool concern — it's a context construction concern that happens before any tool executes.

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ ToolExecutor                                                     │
│                                                                  │
│  tools: HashMap<String, Box<dyn Tool>>                          │
│                                                                  │
│  ┌─────────────────────┐  ┌──────────────────────────────────┐ │
│  │ Built-in Tools       │  │ Configured Tools (from config)   │ │
│  │                      │  │                                   │ │
│  │  ReadTool            │  │  ConfiguredTool { "test",         │ │
│  │  WriteTool           │  │    "cargo test", 300s }           │ │
│  │  EditTool            │  │  ConfiguredTool { "lint",         │ │
│  │  ListTool            │  │    "cargo clippy", 120s }         │ │
│  │  GrepTool            │  │  ConfiguredTool { "build",        │ │
│  │  FindTool            │  │    "cargo build", 300s }          │ │
│  │  ShellTool           │  │                                   │ │
│  │  FetchTool           │  │  (auto-detected from markers      │ │
│  │  SearchTool          │  │   or explicit in loopr.yml)      │ │
│  │  ...                 │  │                                   │ │
│  └─────────────────────┘  └──────────────────────────────────┘ │
│                                                                  │
│  definitions() → Vec<ToolDefinition>   (for Anthropic API)      │
│  execute(&ToolCall, &ToolContext) → ToolResult                  │
│  available_tools() → Vec<&str>                                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
                     ┌──────────────────┐
                     │ ToolContext       │
                     │  working_dir: Path│  ← worktree, repo root, or CWD
                     │  read_files: Set  │
                     │  sandbox: bool    │
                     └──────────────────┘
```

### Data Model

#### Tool Trait (unchanged from original design)

```rust
// src/tools/traits.rs
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}
```

#### ConfiguredTool — the bridge

This is the key new type. It wraps a `ToolEntry` (from `loopr.yml` or auto-detection) and implements `Tool`:

```rust
// src/tools/configured.rs
pub struct ConfiguredTool {
    entry: ToolEntry,
}

impl ConfiguredTool {
    pub fn new(entry: ToolEntry) -> Self {
        Self { entry }
    }
}

#[async_trait]
impl Tool for ConfiguredTool {
    fn name(&self) -> &str {
        &self.entry.name
    }

    fn description(&self) -> &str {
        &self.entry.command
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Additional arguments to pass to the command"
                }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> ToolResult {
        let args: Vec<String> = input
            .get("args")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let full_command = if args.is_empty() {
            self.entry.command.clone()
        } else {
            format!("{} {}", self.entry.command, args.join(" "))
        };

        // All tools run in ctx.working_dir — whether that's a worktree, repo root, or CWD.
        // The ToolEntry.worktree field is now irrelevant at execution time; the caller
        // already set the right working_dir when constructing the ToolContext.
        match execute_shell_command(&full_command, &ctx.working_dir, self.entry.timeout_secs).await {
            Ok(output) => ToolResult {
                content: format_command_output(&output),
                is_error: !output.status.success(),
            },
            Err(e) => ToolResult {
                content: format!("command failed: {}", e),
                is_error: true,
            },
        }
    }
}
```

The subprocess execution logic (`sh -c`, timeout, SIGTERM→SIGKILL, truncation to 32KB, `kill_on_drop`) is extracted into a shared `execute_shell_command()` utility used by both `ConfiguredTool` and the `ShellTool` built-in.

#### ToolContext (refined from original design)

The original design called this field `worktree`, but since the same context works for worktrees, repo roots, and CWD, we name it `working_dir`:

```rust
// src/tools/context.rs
pub struct ToolContext {
    /// The directory tools operate in. Could be a git worktree, repo root, or CWD.
    pub working_dir: PathBuf,
    pub exec_id: String,
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub sandbox_enabled: bool,
}

impl ToolContext {
    pub fn new(working_dir: PathBuf, exec_id: String) -> Self;
    pub async fn track_read(&self, path: &Path);
    pub async fn was_read(&self, path: &Path) -> bool;
    pub fn validate_path(&self, path: &str) -> Result<PathBuf>;
}
```

Construction sites:
- **Agent with worktree** (Implementer): `ToolContext::new(worktree_path, session_id)`
- **Agent without worktree** (Researcher, Coordinator): `ToolContext::new(repo_root, session_id)`
- **TUI Chat**: `ToolContext::new(cwd, "tui-chat")`

#### Unified ToolExecutor

```rust
// src/tools/executor.rs
pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolExecutor {
    /// Create with all built-in tools plus configured project tools.
    pub fn standard(configured: &[ToolEntry]) -> Self {
        let mut tools = Self::builtins();
        for entry in configured {
            tools.insert(entry.name.clone(), Box::new(ConfiguredTool::new(entry.clone())));
        }
        Self { tools }
    }

    /// Auto-detect project type and register appropriate configured tools.
    /// This absorbs ToolRunner::detect_or_default().
    pub fn detect_or_configured(worktree: &Path, configured: &[ToolEntry]) -> Self {
        let project_tools = detect_project_tools(worktree, configured);
        Self::standard(&project_tools)
    }

    /// Subset for TUI chat (all built-ins, no agent-only tools like plan).
    pub fn chat(configured: &[ToolEntry]) -> Self {
        let mut exec = Self::standard(configured);
        exec.tools.remove("plan"); // agent-only
        exec
    }

    /// Export tool definitions for the Anthropic API.
    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        }).collect()
    }

    /// List available tool names.
    pub fn available_tools(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Execute a tool call.
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> ToolResult {
        match self.tools.get(&call.name) {
            Some(tool) => tool.execute(call.input.clone(), ctx).await,
            None => ToolResult {
                content: format!("unknown tool: {}", call.name),
                is_error: true,
            },
        }
    }
}
```

#### Project Auto-Detection (ported from ToolRunner)

```rust
// src/tools/detect.rs
const MARKER_ORDER: &[(&str, fn() -> Vec<ToolEntry>)] = &[
    ("package.json", js_preset),
    ("pyproject.toml", python_preset),
    // Cargo.toml → use configured defaults (already Rust)
];

pub fn detect_project_tools(worktree: &Path, configured: &[ToolEntry]) -> Vec<ToolEntry> {
    for (marker, preset_fn) in MARKER_ORDER {
        if worktree.join(marker).exists() {
            return preset_fn();
        }
    }
    // Cargo.toml or no markers → use config
    configured.to_vec()
}
```

### API Design

No changes to the `LlmClient` trait extension or the agentic loop proposed in the original design doc. The only difference is that `ToolExecutor::standard()` now includes configured project tools alongside built-ins, so the LLM sees everything in one `tools` array.

#### What the LLM Sees

Before (original design): 14 built-in tools. Project tools hidden behind a `shell` built-in.

After (unified): 14 built-in tools + N configured tools. For a Rust project:

```json
{
  "tools": [
    { "name": "read", "description": "Read file with line numbers", "input_schema": {...} },
    { "name": "write", "description": "Write/create file", "input_schema": {...} },
    { "name": "edit", "description": "Exact string replacement", "input_schema": {...} },
    { "name": "grep", "description": "Search file contents with regex", "input_schema": {...} },
    ...
    { "name": "test", "description": "cargo test", "input_schema": {"type":"object","properties":{"args":...}} },
    { "name": "lint", "description": "cargo clippy -- -D warnings", "input_schema": {"type":"object","properties":{"args":...}} },
    { "name": "build", "description": "cargo build", "input_schema": {"type":"object","properties":{"args":...}} }
  ]
}
```

The LLM calls `test` directly — no `{"name": "shell", "input": {"command": "cargo test"}}` indirection.

### What Dies

| Current | Fate |
|---------|------|
| `ToolRunner` struct (`src/tools/mod.rs`) | Replaced by `ToolExecutor` |
| `ToolRunner::run()` | Logic extracted to `execute_shell_command()`, used by `ConfiguredTool` and `ShellTool` |
| `ToolRunner::detect_or_default()` | Absorbed into `ToolExecutor::detect_or_configured()` |
| `ToolRunner::available_tools()` | Replaced by `ToolExecutor::available_tools()` |
| `AgentContext.tool_runner: Arc<ToolRunner>` | Replaced by `Arc<ToolExecutor>` |
| `executor.rs` per-session tool detection | Uses `ToolExecutor::detect_or_configured()` |
| `AgentAction::RunTool` | During migration: dispatch maps to `ToolExecutor::execute()`. Post-migration: removed with the action system. |
| Proposed `shell` built-in (original design) | Kept only for ad-hoc commands. Project tools are `ConfiguredTool` instances. |

### What Stays

| Current | Reason |
|---------|--------|
| `ToolEntry` config struct | Configuration format is stable; `ConfiguredTool` wraps it |
| `tools::ToolResult` (existing, subprocess output) | Replaced by `traits::ToolResult { content, is_error }`. The old struct's fields (`exit_code`, `stdout`, `stderr`, `duration_ms`, `truncated`) are formatted into the `content` string by `ConfiguredTool::execute()`. |
| `tools:` list in `loopr.yml` | User-facing config unchanged |
| JS/Python/Rust preset functions | Moved to `detect.rs`, same logic |

### Implementation Plan

#### Phase 1: Tool Trait & Context Infrastructure

**Files created:**
- `src/tools/traits.rs` — `Tool` trait, `ToolResult`
- `src/tools/context.rs` — `ToolContext` (sandbox, read tracking, path validation)
- `src/tools/types.rs` — `ToolDefinition`, `ToolCall`, `ContentBlock`, `StopReason`, `Message`, `CompletionResponse`
- `src/tools/error.rs` — `ToolError` enum
- `src/tools/shell.rs` — shared `execute_shell_command()` utility (extracted from current `ToolRunner::run()`)

**Files modified:**
- `src/tools/mod.rs` — re-export new modules

#### Phase 2: ConfiguredTool & Unified ToolExecutor

**Files created:**
- `src/tools/configured.rs` — `ConfiguredTool` impl
- `src/tools/detect.rs` — project auto-detection (ported from `ToolRunner::detect_or_default()`)
- `src/tools/executor.rs` — `ToolExecutor` (unified registry)

**Files modified:**
- `src/tools/mod.rs` — remove `ToolRunner` struct, re-export `ToolExecutor`
- `src/agents/mod.rs` — change `AgentContext.tool_runner` to `AgentContext.tool_executor: Arc<ToolExecutor>`
- `src/agents/executor.rs` — per-session tool detection uses `ToolExecutor::detect_or_configured()`
- `src/agents/executor.rs` — `execute_action` for `RunTool` delegates to `ToolExecutor::execute()`

#### Phase 3: Built-in Tools

**Files created (one per tool):**
- `src/tools/builtin/mod.rs`
- `src/tools/builtin/read.rs`
- `src/tools/builtin/write.rs`
- `src/tools/builtin/edit.rs`
- `src/tools/builtin/list.rs`
- `src/tools/builtin/tree.rs`
- `src/tools/builtin/glob.rs`
- `src/tools/builtin/grep.rs`
- `src/tools/builtin/find.rs`
- `src/tools/builtin/shell.rs` — generic shell for ad-hoc commands (delegates to `execute_shell_command()`)
- `src/tools/builtin/slash.rs`
- `src/tools/builtin/fetch.rs`
- `src/tools/builtin/search.rs`
- `src/tools/builtin/todo.rs`
- `src/tools/builtin/plan.rs`

#### Phase 4: LLM Client Extension & Agentic Loop

**Files modified:**
- `src/agents/implementer.rs` — add `call_with_tools()` to `LlmClient` trait
- `src/agents/llm_client.rs` — implement `call_with_tools()` with extended SSE parsing

**Files created:**
- `src/tools/agentic_loop.rs` — `run_tool_loop()` function

#### Phase 5: TUI Chat Integration

**Files modified:**
- `src/tui/run.rs` — replace `call_with_history()` with `run_tool_loop()` for chat
- `src/tui/views/chat.rs` — display tool invocations in chat history

#### Phase 6: Agent Migration (gradual)

**Files modified (per agent):**
- `src/agents/researcher.rs` — replace custom SearchCode/ReadFile/ListDirectory with native tools via agentic loop
- `src/agents/implementer.rs` — replace ReadFile/WriteFile/RunTool actions with native tools
- `src/agents/coordinator.rs` — add tool use for orchestration

## Alternatives Considered

### Alternative 1: Keep Two Systems (original design doc approach)

- **Description:** `ToolRunner` for project tools, `ToolExecutor` for LLM-facing tools. `shell` built-in wraps `ToolRunner`.
- **Pros:** Lower migration risk. No changes to existing `ToolRunner` consumers.
- **Cons:** Two registries. Wrapper indirection. Inconsistent sandbox enforcement. LLM can't call project tools directly — must go through generic `shell`.
- **Why not chosen:** The systems are structurally identical. Keeping both is unnecessary complexity.

### Alternative 2: Make ToolRunner Implement Tool Trait

- **Description:** Add `input_schema()` and `description()` methods to `ToolRunner`, keep it as the executor.
- **Pros:** Minimal refactoring.
- **Cons:** `ToolRunner` operates on string names and `ToolEntry`; adding trait object dispatch to it creates a Frankenstein struct that does two things. Built-in tools (read, edit) don't have `ToolEntry` configs.
- **Why not chosen:** Cleaner to build the right abstraction (`ToolExecutor` with `Box<dyn Tool>`) and port `ToolRunner`'s logic into it via `ConfiguredTool`.

### Alternative 3: No Unified Type — Duck Typing via Enum

- **Description:** Use an enum `ToolKind::Builtin(BuiltinTool)` / `ToolKind::Configured(ToolEntry)` instead of trait objects.
- **Pros:** No dynamic dispatch. Compile-time exhaustiveness.
- **Cons:** Every new built-in tool requires modifying the enum. Can't extend with external tools later (MCP). Less ergonomic for 14+ variants.
- **Why not chosen:** Trait objects are the right fit for an open-ended tool registry.

## Technical Considerations

### Dependencies

No new crate dependencies beyond what the original design doc specifies. `ConfiguredTool` reuses existing `tokio::process::Command` infrastructure.

### Performance

- `ConfiguredTool::execute()` has identical performance to `ToolRunner::run()` — same subprocess mechanism
- Tool definition export adds ~2K tokens per API request (same as original design)
- Adding N configured tools to the Anthropic `tools` array adds ~100 tokens per tool (negligible)
- Dynamic dispatch via `Box<dyn Tool>` is a single vtable indirection — not measurable

### Security

- `ToolContext.validate_path()` enforces sandbox for all tools, including configured tools. Currently `ToolRunner` has no sandbox — it trusts the command. This is an improvement.
- Configured tools still run via `sh -c`, so command injection via malicious `ToolEntry.command` is possible — but `ToolEntry` comes from `loopr.yml`, not from the LLM. The LLM can only pass `args`, which are appended as string arguments, not interpolated into the shell command.
- The `shell` built-in (for ad-hoc commands) is the highest-risk tool — the LLM controls the full command string. Same risk as the original design. Mitigated by worktree sandboxing.

### Testing Strategy

- **ConfiguredTool tests:** Execute with tempdir, verify subprocess behavior (exit code, stdout, stderr, timeout, truncation). These are the existing `ToolRunner` tests, rewritten against the `Tool` trait.
- **ToolExecutor tests:** Registration (builtins + configured), dispatch, unknown tool, `definitions()` export format.
- **Detection tests:** Port existing `test_detect_js_project`, `test_detect_python_project`, etc.
- **Integration:** Agentic loop with mock LLM that calls configured tools (e.g., `test`) and built-in tools (e.g., `read`) in the same conversation.
- **Backward compat:** `execute_action` for `AgentAction::RunTool` dispatches through `ToolExecutor` and produces identical results.

### Rollout Plan

1. **Phase 1-2:** Create new infrastructure alongside existing `ToolRunner`. No behavioral changes.
2. **Phase 2 gate:** Swap `AgentContext.tool_runner` → `AgentContext.tool_executor`. `execute_action` for `RunTool` delegates to new executor. All existing tests must pass.
3. **Phase 3:** Add built-in tools. Still no behavioral change to agents (they use action system).
4. **Phase 4-5:** LLM client extension + TUI chat. First user-visible change.
5. **Phase 6:** Gradual agent migration. Action system remains as fallback.
6. **Cleanup:** Remove `ToolRunner` struct, `ActionResult::ToolRun(ToolResult)` → unified result type.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `ConfiguredTool` behavioral divergence from `ToolRunner` | Medium | Medium | Port tests verbatim; diff subprocess output |
| Tool name collision (built-in `test` vs configured `test`) | Low | High | Configured tools override built-ins; log warning |
| Too many tools in Anthropic API (14 + N configured) | Low | Low | Claude handles 20+ tools well; can filter per-agent |
| `AgentContext.tool_runner` → `tool_executor` churn | Medium | Medium | Single rename; search-and-replace; tests catch misses |
| Configured tool args injection | Low | High | Args are appended, not interpolated; shell escaping |

## Edge Cases

### Tool Name Collisions

If a `loopr.yml` entry has `name = "read"`, it collides with the built-in `ReadTool`. Resolution: configured tools override built-ins. The `ToolExecutor::standard()` inserts builtins first, then configured tools — later insertions win. A log warning is emitted.

### Empty Configured Tools

If `loopr.yml` has no `tools:` section and no project markers are detected, `ToolExecutor::standard(&[])` contains only the 14 built-ins. The LLM has no project-specific tools but can use `shell` for ad-hoc commands.

### ToolContext Construction by Caller

The `ToolContext.working_dir` is set by the caller, not the tool:

| Caller | `working_dir` | `sandbox_enabled` |
|--------|--------------|-------------------|
| Implementer agent | worktree path (`/tmp/worktrees/work-42`) | true |
| Researcher agent | repo root | true |
| Coordinator agent | repo root | true |
| TUI Chat | CWD | true |

The `ToolEntry.worktree` config field becomes a hint for documentation/UX only — at execution time, every tool just uses `ctx.working_dir`. This eliminates the special-case handling currently in `ToolRunner::run()` where `worktree: false` tools ignore the working directory.

### Researcher Sandbox Denylist

The Researcher currently has a custom `validate_path()` in `researcher.rs` that denies access to `.env`, `.key`, `.pem`, `credentials`, and `secret` files. With unified tools, the `ToolContext.validate_path()` must absorb this denylist. Two options:

1. **Global denylist in ToolContext** — all tools, all agents, all callers get the same denylist. Simplest. Slightly restrictive for Implementer (which may need to write `.env.example`).
2. **Per-context denylist** — `ToolContext::new()` accepts an optional denylist. Researcher and Chat get the security denylist; Implementer gets a relaxed one.

Recommendation: option 2 — the `ToolContext` constructor takes `deny_patterns: &[&str]` defaulting to the security set. Implementer can override.

### ToolEntry.worktree Field

The `worktree: bool` field on `ToolEntry` in `loopr.yml` currently controls whether `ToolRunner::run()` sets `current_dir`. With the unified model, `current_dir` is always `ctx.working_dir`. The field becomes:
- **Config schema:** kept for backward compatibility, but ignored at execution time.
- **Documentation:** serves as a hint that this tool operates on project files (vs. a system command).
- **Future:** can be removed in a config version bump.

### ActionResult::ToolRun Migration

`ActionResult::ToolRun(tools::ToolResult)` currently carries the subprocess-specific struct (`exit_code`, `stdout`, `stderr`, `duration_ms`, `truncated`). During the migration:
- Phase 2: `execute_action` for `RunTool` calls `ToolExecutor::execute()`, gets `traits::ToolResult { content, is_error }`, wraps it in a new `ActionResult::ToolRun` that carries the formatted string.
- Phase 6 (cleanup): `ActionResult::ToolRun` variant is replaced or removed entirely as agents move to the agentic loop.

## Open Questions

- [ ] Should configured tool descriptions be richer than just the command string? (e.g., auto-generated from the tool name: "Run the project's test suite")
- [ ] Should there be a per-agent tool filter? (Researcher: read-only tools only. Coordinator: orchestration tools only.)
- [ ] What's the right collision policy — configured overrides builtin, or error?
- [ ] Should `ToolEntry.worktree` be deprecated explicitly or just ignored silently?

## References

- `docs/design/2026-03-04-native-tool-use.md` — original tool-use design (this doc supersedes the two-system architecture)
- `docs/architecture-process-vs-async-task.md` — current runtime architecture
- `src/tools/mod.rs` — existing `ToolRunner` implementation
- `src/agents/mod.rs` — `AgentAction` enum (the action system being migrated from)
- `src/agents/executor.rs` — `execute_action()` dispatch
- `src/config.rs` — `ToolEntry` struct
