# Design Document: Dynamic Tool Resolution

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 4/4

## Summary

Loopr's tool detection system hardcodes language heuristics into compiled Rust, falls back silently to Rust/cargo tooling for unknown projects, and overrides user-configured tools when a marker file (e.g., `package.json`) is present. This document replaces the current detection-first priority with a 3-layer resolution stack: explicit config > agent-discovered runtime tools > detection heuristics, with loud failure when no tools can be resolved.

## Problem Statement

### Background

Loopr agents (Implementer, Reviewer, Integrator) use tools like `test`, `lint`, `fmt`, and `build` to verify code. These tools are configured as `ToolEntry` structs (`src/config.rs:402-407`) with a name, shell command, timeout, and worktree flag. Two parallel systems manage tools: `ToolRunner` (simple subprocess wrapper) and `ToolExecutor` (unified registry with 14 builtins + configured tools).

When an agent spawns with a worktree, the executor re-detects tools at `src/agents/executor.rs:406-423`:

```rust
ctx.tool_runner = Arc::new(ToolRunner::detect_or_default(worktree, &stores.config.agents.tools));
ctx.tool_executor = Arc::new(ToolExecutor::detect_or_configured(worktree, &stores.config.agents.tools));
```

The detection functions (`src/tools/detect.rs`) check for marker files:
1. `package.json` -> npm preset (test, lint, build)
2. `pyproject.toml` -> Python preset (pytest, ruff)
3. `Cargo.toml` -> falls back to config tools
4. No markers -> falls back to config tools (which default to Rust/cargo in `src/config.rs:302-333`)

### Problem

Three failures stem from this architecture:

1. **Silent wrong-language fallback.** If you throw a Lua, Go, or Elixir project at Loopr, detection finds no markers, falls back to config defaults, and the Implementer runs `cargo test` against a Lua codebase. The pipeline fails silently with confusing errors rather than telling the user "no test tool configured."

2. **Detection overrides user config.** If you define `test: "my-custom-runner"` in `loopr.yml` but the project has a `package.json`, detection overwrites your tool with `npm test`. The user's explicit config is silently discarded.

3. **No runtime adaptability.** A Researcher agent can analyze a project and determine "this uses busted for Lua testing" - but has no mechanism to feed that discovery back into the tool registry. The only path is modifying `loopr.yml` on disk, which pollutes the target repository.

### Goals

- Establish a 3-layer priority stack: user config > runtime agent discovery > detection heuristics
- Add a `tools.register` IPC endpoint so agents can inject tools at runtime without modifying target repos
- Make tool resolution failures loud and actionable
- Remove the silent Rust/cargo fallback for non-Rust projects

### Non-Goals

- Adding `.loopr/tools/` directory convention to target repos (violates "never pollute the target repo")
- Rewriting the ToolRunner/ToolExecutor dual system into one (separate refactor)
- Auto-detecting tools for every possible language (detection remains a best-effort heuristic, not a guarantee)
- Changing how validation_commands work (they're phase-scoped and orthogonal to agent tools)

## Proposed Solution

### Overview

Replace the detection-first tool resolution with a 3-layer priority stack. Add a `runtime_tools` collection to Stores and a `tools.register` IPC handler. Modify agent context initialization to resolve tools in priority order. Add `RegisterTool` to the Researcher's action set so agents can bootstrap tool definitions for unknown projects.

### Architecture

```
Tool Resolution Priority (highest to lowest):

Layer 1: loopr.yml agents.tools[]     <- User config, always wins
Layer 2: Stores.runtime_tools{}       <- Agent-discovered via tools.register IPC
Layer 3: detect_project_tools()       <- Heuristic fallback, loud failure if empty

Resolution function:
  for each tool_name in [test, lint, fmt, build, ...]:
    if config has tool_name -> use config tool
    else if runtime_tools has tool_name -> use runtime tool
    else if detect finds tool_name -> use detected tool
    else -> no tool (not an error per se - some tools are optional)

  if NO tools resolved at all -> fail loudly
```

### Data Model

**Runtime tool entry** - reuses existing `ToolEntry` struct, no new types needed:

```rust
// In Stores (src/daemon/context.rs):
pub runtime_tools: StdRwLock<HashMap<String, ToolEntry>>,
```

Keyed by tool name (e.g., "test", "lint"). Not persisted to TaskStore - these are session-scoped, ephemeral. When the daemon restarts, runtime tools are cleared.

### API Design

**IPC: `tools.register`**

```json
{
  "method": "tools.register",
  "params": {
    "name": "test",
    "command": "busted --verbose",
    "timeout_secs": 300,
    "worktree": true
  }
}
```

Response: success with the registered tool entry, or error if name is empty.

**IPC: `tools.list`** (already exists at `src/daemon/handlers/integrator.rs:606-626`)

Extend to include source information: "config", "runtime", or "detected".

**AgentAction: `RegisterTool`**

```rust
RegisterTool {
    name: String,
    command: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_true")]
    worktree: bool,
},
```

Available to Researcher and Coordinator agents. The executor calls `tools.register` IPC, then rebuilds the agent's ToolRunner/ToolExecutor from the resolved stack.

### Implementation Plan

#### Phase 1: Runtime Tool Registry

**File: `src/daemon/context.rs`**

Add `runtime_tools` to Stores:

```rust
pub runtime_tools: StdRwLock<HashMap<String, ToolEntry>>,
```

Add to `store_accessors!` macro call (the macro generates `read_runtime_tools()` and `write_runtime_tools()` - it only requires `HashMap<String, T>`, not the `Record` trait) and initialize as empty in `Stores::new()`.

Note: `runtime_tools` is NOT persisted to TaskStore. It lives only in the in-memory Stores. This is intentional - runtime tools are session-scoped.

**File: `src/daemon/handlers/mod.rs`**

Add dispatch arm: `"tools.register" => handle_tools_register(stores, event_tx, req)`

**File: `src/daemon/handlers/integrator.rs`** (or new `src/daemon/handlers/tools.rs`)

Implement `handle_tools_register`:
- Parse `name`, `command`, `timeout_secs`, `worktree` from params
- Validate: name must be non-empty, command must be non-empty
- Insert into `stores.runtime_tools`
- Rebuild `stores.tool_runner` and `stores.tool_executor`: call `resolve_tools()` with no worktree (config + runtime only), create new instances, and assign them to stores. Since `tool_runner` and `tool_executor` are `Arc<T>`, this means: `stores.tool_runner = Arc::new(ToolRunner::new(&resolved))`. In-flight agents hold their own Arc clone to the old instance; they're unaffected. Newly spawned agents clone the new Arc from stores.
- Emit `DaemonEvent::record_created("tool", &name)`
- Return the registered tool entry

Note: `tool_runner` and `tool_executor` on Stores are currently `Arc<T>`, not wrapped in RwLock. To swap them atomically, we need to either: (a) wrap them in a RwLock, or (b) use `ArcSwap`. Option (a) is simplest and consistent with other Stores fields. Change their type to `StdRwLock<Arc<ToolRunner>>` and `StdRwLock<Arc<ToolExecutor>>`. Agent code that clones them acquires a read lock briefly.

#### Phase 2: Priority Resolution Function

**File: `src/tools/mod.rs`** (new function)

```rust
pub fn resolve_tools(
    config_tools: &[ToolEntry],
    runtime_tools: &HashMap<String, ToolEntry>,
    worktree: Option<&Path>,
) -> Vec<ToolEntry> {
    let mut resolved: HashMap<String, ToolEntry> = HashMap::new();

    // Layer 3 (lowest): detection heuristics
    if let Some(wt) = worktree {
        for tool in detect_project_tools(wt, &[]) {
            resolved.insert(tool.name.clone(), tool);
        }
    }

    // Layer 2: runtime agent-discovered tools (overrides detection)
    for (name, tool) in runtime_tools {
        resolved.insert(name.clone(), tool.clone());
    }

    // Layer 1 (highest): explicit config (overrides everything)
    for tool in config_tools {
        resolved.insert(tool.name.clone(), tool.clone());
    }

    resolved.into_values().collect()
}
```

**File: `src/agents/executor.rs` (lines 406-423)**

Replace the current detect-or-default calls with the resolution function:

```rust
if let Some(ref wt_path) = ctx.session.worktree_path {
    let worktree = std::path::Path::new(wt_path);
    let runtime_tools = stores.read_runtime_tools()?;
    let resolved = crate::tools::resolve_tools(
        &stores.config.agents.tools,
        &runtime_tools,
        Some(worktree),
    );
    drop(runtime_tools);
    ctx.tool_runner = Arc::new(crate::tools::ToolRunner::new(&resolved));
    ctx.tool_executor = Arc::new(crate::tools::ToolExecutor::standard(&resolved));
}
```

Note: The 3-layer resolution runs per-agent-spawn (when a worktree is available), not at daemon init. Daemon init continues to use config-only tools for the global defaults. This is correct because detection needs a worktree path, which doesn't exist until an agent is assigned work.

Also: `detect_project_tools()` currently accepts `configured: &[ToolEntry]` as a fallback parameter. In the new design, modify its signature to take no fallback - it either detects tools from markers or returns empty. The fallback logic moves into `resolve_tools()`.

#### Phase 3: Loud Failure + Remove Rust Defaults

**File: `src/config.rs` (lines 302-333)**

Remove the hardcoded Rust default tools from `AgentConfig::default()`. Replace with an empty vec. Users who want Rust tools must configure them in `loopr.yml`, or let detection discover them.

**File: `src/tools/detect.rs`**

When detection returns empty (no marker files found), log a warning:

```
warn!("No project tools detected at {:?}. Configure tools in loopr.yml \
       or use a Researcher agent with RegisterTool to bootstrap.", worktree);
```

The resolution function itself does not fail - having zero configured tools is valid (some Work items may not need tools). But the Implementer's run_tool action should return a clear error: "Tool 'test' not found. No test tool is configured for this project."

**File: `src/agents/executor.rs` (RunTool handler)**

The RunTool handler already returns an error when a tool isn't found. Ensure the error message is actionable:

```
"Tool '{}' not found. Available tools: [{}]. \
 You MUST use the register_tool action to define this tool before retrying. \
 Analyze the project to determine the correct command, then register it."
```

#### Phase 4: Agent Bootstrapping

**File: `src/agents/mod.rs`**

Add `RegisterTool` variant to `AgentAction`:

```rust
RegisterTool {
    name: String,
    command: String,
    #[serde(default = "default_tool_timeout")]
    timeout_secs: u64,
    #[serde(default = "default_tool_worktree")]
    worktree: bool,
},
```

**File: `src/agents/executor.rs`**

Add handler for `RegisterTool`:
- Call `bridge.request("tools.register", params)`
- Rebuild the agent's own tool_runner and tool_executor from the new resolved stack
- Return `ActionResult::ToolRegistered(name)`

**File: `prompts/researcher.pmt`**

Add `register_tool` to the Researcher's available actions:

```
N. `register_tool` - Register a project tool (test runner, linter, etc.) for
   use by other agents. Use this when you discover the project's toolchain.
   Required: "name" (e.g., "test"), "command" (shell command).
   Optional: "timeout_secs" (default 300), "worktree" (default true).
```

**File: `prompts/coordinator.pmt`** (or equivalent)

Add guidance: when the Implementer reports "Tool 'test' not found", the Coordinator should dispatch a Researcher to analyze the project and register the appropriate tools.

### Testing Strategy

1. **Unit test - resolve_tools priority:** Config overrides runtime overrides detection
2. **Unit test - resolve_tools empty:** All layers empty returns empty vec
3. **Unit test - tools.register IPC:** Register a tool, verify it appears in runtime_tools
4. **Unit test - tools.register rebuilds executors:** After register, stores.tool_runner includes the new tool
5. **Unit test - RegisterTool action:** Execute RegisterTool, verify tool is available
6. **Unit test - loud failure:** RunTool with missing tool returns actionable error message
7. **Unit test - config wins over detection:** Configure "test" in config, detect different "test" from marker file, verify config wins
8. **Unit test - runtime wins over detection:** Register "test" at runtime, detect different "test" from marker file, verify runtime wins

## Alternatives Considered

### Alternative 1: `.loopr/tools/` Directory in Target Repo

- **Description:** Drop YAML tool definitions into `.loopr/tools/` in the target repository. Daemon scans this directory on startup.
- **Pros:** Simple filesystem convention. Tools persist across sessions.
- **Cons:** Pollutes the target repo's git history with orchestration artifacts. Violates the principle that Loopr should never leave artifacts in repos it orchestrates. Agents committing these files create merge noise.
- **Why not chosen:** Loopr must remain a ghost in the machine. Orchestration config belongs in the orchestrator, not the target.

### Alternative 2: Detection-Only (Fix detect.rs)

- **Description:** Expand detect.rs with more language heuristics (Go, Lua, Elixir, etc.) and fix the priority inversion.
- **Pros:** No IPC changes needed. Self-contained fix.
- **Cons:** Every new language requires a code change and recompilation. Cannot handle custom/unusual tool setups. Detection is inherently fragile - there are always edge cases.
- **Why not chosen:** Language detection belongs in the LLM's reasoning loop, not in hardcoded heuristics. The 3-layer stack lets detection be a fallback rather than the primary mechanism.

### Alternative 3: Persist Runtime Tools to Disk

- **Description:** Same as proposed, but persist runtime_tools to a file (e.g., `~/.local/share/loopr/tools.json`) so they survive daemon restarts.
- **Pros:** Tools don't need to be re-discovered on restart.
- **Cons:** Stale tool configs accumulate. Tools discovered for one project may be wrong for another. Session isolation is cleaner.
- **Why not chosen:** Runtime tools should be ephemeral. If you want persistent tools, put them in `loopr.yml`. The Researcher can re-discover tools quickly on a new session.

## Technical Considerations

### Dependencies

None. Uses existing ToolEntry, ToolRunner, ToolExecutor, Stores, and IPC infrastructure.

### Performance

Negligible. Tool resolution is called once per agent spawn. The resolution function iterates three small collections.

### Backward Compatibility

- Existing `loopr.yml` tool configs continue to work (and gain highest priority)
- Projects with no config and working detection continue to work (Layer 3)
- The only breaking change: removing Rust defaults from `AgentConfig::default()` means Rust projects without explicit tool config will rely on detection (which already handles `Cargo.toml`). If detection fails, they get a clear error instead of silently using cargo.

### Testing Strategy

See Implementation section above for 8 unit tests covering each touch point.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Removing Rust defaults breaks existing users | Medium | Low | Cargo.toml detection still provides Rust tools. Only affects users with no `Cargo.toml` and no config, which is unlikely for Rust projects. |
| Researcher registers wrong tools | Low | Medium | The Implementer will fail when running the tool, triggering a retry or Coordinator intervention. The human can always override via `loopr.yml`. |
| Runtime tools leak between projects when daemon is reused | Low | Medium | Runtime tools are cleared on daemon restart. If the daemon serves multiple projects, the Coordinator should re-register tools per session. Future: scope runtime tools by session or repo path. |
| Tool rebuild after register races with in-flight agent | Very Low | Medium | The rebuild replaces Arc'd tool runners. In-flight agents hold their own Arc clone and are unaffected. New agents pick up the rebuilt version. |

## Open Questions

- [ ] Should runtime tools be scoped by repo path to prevent leaking between projects on the same daemon?
- [ ] Should the Coordinator automatically dispatch a Researcher when an Implementer reports "tool not found", or should this be prompt-level guidance only?
- [ ] Should `tools.list` show all three layers with their sources, or just the resolved result?

## References

- `src/tools/detect.rs` - Current detection heuristics
- `src/tools/mod.rs:112-127` - ToolRunner::detect_or_default()
- `src/tools/executor.rs:17-94` - ToolExecutor with detect_or_configured()
- `src/agents/executor.rs:406-423` - Per-agent tool detection (the priority inversion)
- `src/config.rs:302-333` - Default Rust tool config
- `src/config.rs:402-407` - ToolEntry struct
- `src/daemon/context.rs:321-327` - Daemon tool initialization
- `src/daemon/handlers/mod.rs:104-207` - IPC dispatch routing
- `src/daemon/handlers/integrator.rs:606-626` - Existing tool.list handler
- `bin/e2e-targets/react-todo.yml` - E2E tool config example
- `bin/e2e-targets/python-todo.yml` - E2E tool config example
