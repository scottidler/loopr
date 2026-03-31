# Claude Code Tool Architecture - Deep Dive Report

**Author:** Scott A. Idler (via Claude Code analysis)
**Date:** 2026-03-30
**Status:** Research
**Source:** `@anthropic-ai/claude-code@2.1.88` npm package (`cli.js` 16,667 lines bundled JS + `sdk-tools.d.ts` 117KB)

---

## 1. Process Model: `detached: true` + SIGTERM -> SIGKILL Escalation

**The biggest steal for Loopr.** Claude Code spawns Bash commands with `detached: true` (line 2625), which makes the child a **process group leader** on Unix. Their kill escalation (line 748, line 128):

```javascript
// Timeout fires:
child.kill("SIGTERM");
timeout = setTimeout(() => child.kill("SIGKILL"), 5000);
```

The general-purpose kill wrapper (line 128) applies this universally:
```javascript
var forceKillAfterTimeout = 5000;  // 5s grace
// If signal is SIGTERM and forceKillAfterTimeout !== false:
setTimeout(() => process.kill("SIGKILL"), forceKillAfterTimeout)
```

**Loopr gap:** We use `kill_on_drop(true)` + single-PID SIGTERM. No process group, no timed escalation. A `cargo build` that spawns rustc subprocesses would orphan them.

**Steal:** Spawn with `setsid()` (Rust equivalent of `detached: true`), then `killpg(pgid, SIGTERM)` -> 5s timeout -> `killpg(pgid, SIGKILL)`.

---

## 2. Sandbox: bubblewrap (Linux) / Seatbelt (macOS)

Claude Code uses **real OS-level sandboxing** (lines 1561-1575, 778-782):

- **Linux:** `bubblewrap (bwrap)` - lightweight container tech. Commands get wrapped with bwrap before execution.
- **macOS:** `seatbelt` - built-in macOS sandbox framework
- **Network isolation:** `socat` for socket proxying + `seccomp` filters to block unix sockets
- **Filesystem:** `denyOnly` patterns (read restrictions), `allowOnly` patterns (write restrictions), `allowWithinDeny`/`denyWithinAllow` exceptions
- **Sandbox modes:** Strict (must sandbox or deny), Auto-allow (try sandbox, fall back), Disabled

The sandbox wrapping happens at command execution time - `M7.wrapWithSandbox(command, shell, ...)` (line 2625) wraps the command string before spawning.

**Loopr gap:** We have path validation in `ToolContext` but no OS-level isolation. For the heavy runner lane, this is the exact tech we should use.

**Steal:** `bwrap` wrapping for the `no-net` lane is a direct port. The `no-net` runner can literally prefix commands with bwrap args to block network access.

---

## 3. Context Management: Server-Side Compaction (Not Client-Side)

This is a paradigm shift. Claude Code uses the **Anthropic API's `context_management` beta** (lines 12671, 15568):

```javascript
context_management: {
  edits: [{ type: "compact_20260112" }]
}
```

The **server** automatically compacts old messages when context approaches limits. The client just preserves compaction blocks in the conversation. No client-side summarization loop needed.

**Loopr consideration:** We currently do client-side microcompact + auto-compact (LLM-assisted summarization). We should switch to server-side compaction when it's GA - it's simpler, cheaper, and the server has better visibility into what to keep.

---

## 4. Tool Registration: Zod Schemas + Map Dispatch

Tools are registered with:
- Name constant (e.g., `var Cq = "Read"`)
- Description string
- Input schema validated via **Zod** (`inputSchema.safeParse(input)`) at runtime
- A `run()` method

Dispatch is **map lookup by name** (line 38):
```javascript
let tool = tools.find(t => ("name" in t ? t.name : t.mcp_server_name) === toolUse.name);
```

**Already done in Loopr:** Our `HashMap<String, Box<dyn Tool>>` lookup is equivalent. Our `Tool` trait's `input_schema()` returns JSON Schema, validated at execution time.

**Possible steal:** Zod-like compile-time schema validation could be done with `serde_json` + custom validation in Rust, but we already handle this fine.

---

## 5. Bash Tool Features Worth Stealing

From `BashInput`/`BashOutput` types:

| Feature | Claude Code | Loopr |
|---------|------------|-------|
| `run_in_background` | Yes - returns `backgroundTaskId` | No |
| `dangerouslyDisableSandbox` | Explicit escape hatch | No sandbox to disable |
| `description` | Human-readable command description | No |
| `persistedOutputPath` | Large outputs written to disk, path returned | No - we truncate |
| `assistantAutoBackgrounded` | Auto-backgrounds long-running commands | No |
| `returnCodeInterpretation` | Semantic meaning for non-error exit codes | No |

**Steal:** `run_in_background` is huge for builds/tests. Instead of blocking the agentic loop waiting for `cargo build`, background it and poll. `persistedOutputPath` for large outputs is also smart - return a file path instead of truncating.

---

## 6. Agent Spawning: Conversation Threads, Not Processes

Claude Code's `Agent` tool creates **new conversation threads** (not OS processes):
- Each agent gets its own `agentId`, message history, tool set
- `subagent_type` controls which tools are available (e.g., `Plan` agent has read-only tools)
- `isolation: "worktree"` creates a git worktree for filesystem isolation
- `mode` controls permission level (`bypassPermissions`, `dontAsk`, `plan`)
- Multiple agents can run **in parallel** from a single LLM turn

**Loopr parallel:** Our Coordinator/Implementer/Reviewer are already separate tokio tasks with independent conversations. The `isolation: "worktree"` concept maps directly to our worktree-per-loop model.

---

## 7. Permission Model: 3-Level Filtering

Claude Code's permission system (from source map paths: `permissions.ts`, `toolHooks.ts`):

1. **PreToolUse hooks** - shell scripts receive JSON, return `permissionDecision` (allow/deny/ask)
2. **Rule matching** - pattern-based: `Bash(git status:*)`, `Edit(src/**/*.rs)`, `WebFetch(domain:github.com)`
3. **Safety checks** - hardcoded checks that **override** hook allow decisions (deny is immune to hook allow)

Denial tracking accumulates across invocations to prevent permission spam.

**Loopr relevance:** For autonomous agents, we don't prompt users. But for chat mode, a permission model would add safety. Low priority for the runner lane design doc but worth noting.

---

## 8. `pause_turn` Stop Reason

A new stop reason (lines 12699, 15885): when the server hits an **internal iteration limit** (not output tokens), it returns `pause_turn`. The client just re-sends the conversation and the model resumes.

**Loopr:** We should handle this in our agentic loop alongside `end_turn`, `tool_use`, and `max_tokens`.

---

## Summary: What to Steal for the Runner Lane Design Doc

| Pattern | Priority | Why |
|---------|----------|-----|
| **`setsid()` + `killpg()` + SIGTERM->5s->SIGKILL** | Critical | Prevents orphaned child processes from builds |
| **`bwrap` wrapping for no-net lane** | High | Real OS-level network isolation, proven tech |
| **`run_in_background` for heavy tools** | High | Don't block the agentic loop on `cargo build` |
| **`persistedOutputPath` for large outputs** | Medium | Better than truncation for build logs |
| **Server-side `context_management` compaction** | Medium | Replace our client-side compaction |
| **`pause_turn` handling** | Low | API robustness |
| **Permission hooks (PreToolUse/PostToolUse)** | Low | Not needed for autonomous agents, useful for chat |

---

## Appendix: Claude Code Tool Inventory (from sdk-tools.d.ts)

### Input Types

| Tool | Key Fields |
|------|-----------|
| `AgentInput` | `description`, `prompt`, `subagent_type?`, `model?`, `run_in_background?`, `name?`, `mode?`, `isolation?` |
| `BashInput` | `command`, `timeout?`, `description?`, `run_in_background?`, `dangerouslyDisableSandbox?` |
| `FileEditInput` | (edit tool - exact string replacement) |
| `FileReadInput` | (read tool - file contents) |
| `FileWriteInput` | (write tool - create/overwrite files) |
| `GlobInput` | (file pattern matching) |
| `GrepInput` | (content search via ripgrep) |
| `WebFetchInput` | (fetch and analyze web pages) |
| `WebSearchInput` | (web search) |
| `AskUserQuestionInput` | (prompt user for clarification) |
| `TodoWriteInput` | (structured task tracking) |
| `NotebookEditInput` | (Jupyter notebook editing) |
| `EnterWorktreeInput` | (create isolated git worktree) |
| `ExitWorktreeInput` | (leave and clean up worktree) |
| `ExitPlanModeInput` | (transition from plan to execution) |

### Output Types

| Tool | Key Fields |
|------|-----------|
| `AgentOutput` | `agentId`, `content[]`, `totalToolUseCount`, `totalDurationMs`, `totalTokens`, `usage`, `status` |
| `BashOutput` | `stdout`, `stderr`, `interrupted`, `backgroundTaskId?`, `persistedOutputPath?`, `persistedOutputSize?`, `returnCodeInterpretation?` |

### Notable Architectural Details

- **Shell snapshot system**: Claude Code creates shell environment snapshots at session start (capturing aliases, PATH, etc.) and sources them before each command execution (lines 2620-2625)
- **CWD tracking**: After each command, `pwd -P` is written to a temp file and the working directory is updated for the next command
- **`sed` interception**: The Edit tool intercepts `sed -i` commands and converts them to native string replacements (line 2630 area) - avoids subprocess for simple edits
- **Ripgrep bundled**: Claude Code ships its own `rg` binary in `vendor/ripgrep/` - no system dependency
- **Ripgrep EAGAIN retry**: On Linux, if ripgrep hits EAGAIN, it retries with `-j 1` (single-threaded mode) (line 750)
- **Shell detection**: Prefers zsh > bash, detects via `$SHELL` and path probing, validates with `--version` (line 2628 area)
- **PowerShell support**: Full PowerShell provider alongside bash/zsh - separate command wrapping, CWD tracking via `Get-Location`, encoded commands

---

## Appendix: Source Map File Structure (from cli.js.map)

The 60MB source map reveals the original TypeScript structure. Key files relevant to tool architecture:

- `src/utils/permissions/permissions.ts` - Permission logic (1400+ lines)
- `src/services/tools/toolHooks.ts` - Hook execution (500+ lines)
- `src/utils/hooks.ts` - Hook framework (3500+ lines)
- `src/utils/messages.ts` - Message management (5513 lines)
- `src/types/hooks.ts` - Hook type definitions
- `src/services/tools/toolExecution.ts` - Tool orchestration
