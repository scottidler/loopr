# Claude Code Agentic Loop — Research Notes

**Date:** 2026-03-05
**Source:** Reverse-engineered from Claude Code v2.1.70 compiled binary (Bun SEA)
**Purpose:** Understand how Claude Code handles the user → model → tool → model loop so we can align loopr's chat implementation

---

## Overview

Claude Code's core interaction is a **single async generator** (`ry` → `ea1`) that runs a `while(true)` loop. There is no routing, no coordinator agent, no task decomposition engine, and no state machine. The model decides what to do; the loop just executes tool calls and feeds results back.

---

## Architecture

```
User message
    │
    ▼
┌─────────────────────────────────────────┐
│  while(true)  — the agentic turn loop   │
│                                         │
│  1. Context prep (micro/auto compact)   │
│  2. Stream model response               │
│  3. Collect text + tool_use blocks      │
│  4. If no tool_use → done (return)      │
│  5. Execute tools → tool_result msgs    │
│  6. Append to messages, continue loop   │
└─────────────────────────────────────────┘
    │
    ▼
Final text response → rendered to user
```

---

## The Loop in Detail

### Entry: `ry(params)` → `ea1(params)`

`ry` is a thin wrapper that calls `ea1` (the real loop) and marks completed spans. `ea1` is an `async function*` (async generator) that yields events to the UI layer.

**Parameters:**
- `messages` — the full conversation history (mutable array)
- `systemPrompt` — assembled from CLAUDE.md files, git status, tool descriptions, etc.
- `userContext` / `systemContext` — additional context injected into the API call
- `canUseTool` — permission callback (prompts user for approval)
- `toolUseContext` — mutable state bag (tools, app state, abort controller, agent ID, etc.)
- `maxTurns` — optional turn limit
- `querySource` — provenance tag ("sdk", "compact", "session_memory", etc.)
- `fallbackModel` — model to try if primary fails (404, etc.)

**Loop state:**
```
turnCount, autoCompactTracking, maxOutputTokensRecoveryCount,
hasAttemptedReactiveCompact, maxOutputTokensOverride,
pendingToolUseSummary, stopHookActive, transition
```

### Step 1: Context Prep (top of every iteration)

```
microcompact(messages)   → trim/prune messages if needed
autocompact(messages)    → if approaching context limit, summarize old messages
                           yields compact_boundary events to UI
```

Also checks `isAtBlockingLimit` — if messages are too large even after compaction, yields an error and returns.

### Step 2: Call the Model

Single streaming API call via `q.callModel()`:

```javascript
q.callModel({
    messages: OB$(C, L),       // messages + userContext merged
    systemPrompt: d,            // cached system prompt
    thinkingConfig,             // extended thinking settings
    tools,                      // all available tools as JSON schema
    signal,                     // AbortController signal
    model,                      // resolved model (may be fallback)
    fastMode,                   // fast mode state
    toolChoice: undefined,      // no forced tool choice
    queryTracking: {chainId, depth},
    fallbackModel,
    maxOutputTokensOverride,
    mcpTools,                   // MCP server tools
    agentId,                    // subagent ID if applicable
    skipCacheWrite,
    effortValue,
    // ...
})
```

This calls the Anthropic Messages API with streaming. Events flow:
`message_start` → `content_block_start` → `content_block_delta` (repeated) → `content_block_stop` → `message_delta` (with `stop_reason`) → `message_stop`

### Step 3: Collect Response

As SSE events stream in:

- **Text blocks** → yielded to UI immediately for rendering
- **`tool_use` blocks** → collected into array `l`, flag `s = true`
- **Streaming tool execution** (`qoH`): if enabled, tools start executing while the model is still generating. As each `tool_use` block completes in the stream, it's handed to the executor immediately.

The assistant messages are accumulated in array `F`.

### Step 4: Check Stop Condition

After the model finishes streaming, check `stop_reason`:

#### Case A: No tool uses (`s == false`) → turn is potentially done

1. **`max_tokens` recovery**: if the model hit the output token limit and recovery attempts < `sa1` (the max), inject a synthetic user message:
   > "Output token limit hit. Resume directly — no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces."
   Then `continue` the loop.

2. **Reactive compaction**: if prompt-too-long was withheld, attempt reactive compact and retry.

3. **Stop hooks**: run registered stop hooks. If they return blocking errors, inject those as messages and `continue`. If they prevent continuation entirely, return `"stop_hook_prevented"`.

4. **Otherwise**: return `{reason: "completed"}` — the turn is done.

#### Case B: Has tool uses (`s == true`) → execute tools, then loop

1. Execute tools (see next section)
2. Check max turns — if exceeded, yield `max_turns_reached` and return
3. Append tool results to messages
4. `continue` — back to top of `while(true)`

### Step 5: Tool Execution

Two paths depending on whether streaming tool execution is enabled:

#### Standard path: `YR$(toolUseBlocks, assistantMessages, canUseTool, context)`

Groups tool_use blocks by concurrency safety:

```javascript
function Bg1(toolUseBlocks, context) {
    // Reduces blocks into groups:
    // - consecutive concurrency-safe tools → one parallel batch
    // - non-safe tools → sequential execution
    return [{isConcurrencySafe: true, blocks: [...]},
            {isConcurrencySafe: false, blocks: [...]}]
}
```

- **Concurrent tools** (`pg1`): executed in parallel via `ru$` (a parallel async iterator merge). Each tool runs independently; results are yielded as they complete. Context modifications are batched and applied after all concurrent tools finish.

- **Sequential tools** (`mg1`): executed one at a time. Context modifications applied immediately after each tool.

#### Per-tool execution: `QiH(block, assistantMsg, canUseTool, context)`

1. Look up tool by name from `context.options.tools`
2. Validate input: `tool.inputSchema.safeParse(block.input)`
3. **Permission check**: calls `canUseTool(tool, input)` — this is where the user gets prompted to approve/deny
4. Call `tool.call(input)` → returns result content
5. Wrap in `tool_result` message: `{type: "tool_result", tool_use_id, content, is_error}`

If tool not found: `{content: "Error: Tool 'X' not found", is_error: true}`
If tool throws: `{content: "Error: <message>", is_error: true}`

#### Streaming tool executor: `qoH`

When enabled, this executor receives `tool_use` blocks as they complete during streaming (before the model finishes). Tools start running immediately. After streaming ends, `getRemainingResults()` collects any still-executing tools. This overlaps tool execution with model output generation.

### Step 6: Append and Continue

Tool results are formatted as user messages containing `tool_result` blocks. These are appended to the messages array. The loop continues from the top — context prep, call model with the new messages, etc.

---

## Exit Conditions

| Reason | Trigger |
|--------|---------|
| `completed` | Model responds with no tool_use blocks (text-only) |
| `aborted_streaming` | User sends abort/interrupt signal |
| `max_turns_reached` | Turn count exceeds `maxTurns` |
| `model_error` | API error (after fallback attempts) |
| `prompt_too_long` | Context too large even after compaction |
| `stop_hook_prevented` | Stop hook explicitly blocked continuation |
| `image_error` | Image processing error |
| `blocking_limit` | Messages exceed hard blocking token limit |

---

## Model Fallback

If the primary model returns a 404 or specific error class (`tW$`), and a `fallbackModel` is configured:
- Switch to fallback model
- Tombstone any already-yielded assistant messages
- Reset tool state
- Retry from the top of the inner try block

---

## Subagents (the `Agent` tool)

Subagents are **not** a separate orchestration layer. They're just a tool like any other. When the model emits a `tool_use` block with `name: "Agent"`:

1. The Agent tool handler spins up a new `ea1` loop with:
   - Its own messages array (seeded with the agent prompt)
   - Its own system prompt (scoped by agent type)
   - A subset of tools (depending on agent type)
   - Its own abort controller
   - Optionally its own git worktree (isolation)
2. The inner loop runs to completion
3. The result is returned as a `tool_result` to the outer loop

There's no message passing between agents during execution. The outer loop is blocked waiting for the tool result (unless the Agent tool is concurrent-safe, which it appears to be — multiple agents can run in parallel).

---

## IPC / Permission Flow

When a tool needs permission:

1. Tool execution calls `canUseTool(tool, input)`
2. This checks permission config (auto-allow, auto-deny, prompt user)
3. If user must be prompted: permission request is queued via `toolUseConfirmQueue`
4. For subagents: permission requests bubble up to the parent via IPC (`permission_request` / `permission_response` messages)
5. User approves/denies in the TUI
6. Result flows back, tool execution continues or is denied

---

## Context Management Details

### Micro-compaction
Runs every turn. Lightweight trimming — removes redundant content, truncates oversized tool results, etc.

### Auto-compaction
Runs every turn but only triggers when approaching context limits. When it fires:
- Summarizes old messages into a compact form
- Yields a `compact_boundary` system message (so the UI knows compaction happened)
- Resets the messages array to the compacted form
- Tracks compaction metrics (pre/post token counts, etc.)

### Max Output Token Recovery
If the model hits `max_tokens` (output limit), up to N recovery attempts:
- Inject a "resume mid-thought" user message
- Continue the loop
- The model picks up where it left off

---

## Key Takeaways for Loopr

1. **The loop is dead simple**: stream model, execute tools, repeat. No planning phase, no routing, no task decomposition.

2. **All intelligence is in the model**: the system prompt tells the model about tools, conventions, and context. The model decides what to do. The code just executes.

3. **Tool results are user messages**: after tool execution, results go back as `role: user` messages with `tool_result` content blocks. This is just the Anthropic API contract.

4. **Concurrent tool execution is an optimization, not a requirement**: tools are grouped by concurrency safety. Safe tools run in parallel; unsafe ones run sequentially. The streaming executor is a further optimization that overlaps tool execution with model streaming.

5. **Context management is automatic**: compaction happens transparently at the top of every turn. The model never sees the compaction — it just gets a shorter message history.

6. **Subagents are just recursive calls to the same loop**: no special orchestration. An Agent tool call spins up a new `ea1` with scoped tools and messages.

7. **Permission checks are synchronous gates**: tool execution blocks until the user approves. For subagents, permissions bubble up via IPC.

8. **No persistent state between turns**: the only state is the messages array. Everything else is derived.
