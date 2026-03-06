# Comparison: Claude vs Gemini Research on Claude Code's Chat Loop

**Date:** 2026-03-05
**Documents compared:**
- `docs/research/2026-03-05-claude-code-agentic-loop-by-claude.md`
- `docs/2026-03-06-research-claude-code-chat-loop-by-gemini.md`

---

## Agreement

Both documents confirm the same core finding:

1. **Local async generator loop** — Claude Code runs a `while(true)` / `for await` loop locally. The model cannot execute tools server-side. Every tool interaction requires a network round-trip: model emits `tool_use`, local CLI executes it, sends `tool_result` back.

2. **`maxTurns` enforced** — both found the turn counting and `max_turns_reached` exit. Gemini showed the telemetry schema with `error_max_turns`; Claude's doc cataloged all 8 exit conditions.

3. **Parallel tool execution matters** — both identify that Claude Code executes multiple tool calls from a single turn in parallel, and that loopr's chat was calling tools sequentially across turns.

4. **Loopr's `run_tool_loop` is architecturally correct** — both conclude the loop itself is not the problem.

5. **Same binary version** — both analyzed v2.1.70 (different minification: Gemini found `MC`, Claude found `ry`/`ea1`).

---

## Differences

### Depth of the core loop mechanics

| Aspect | Gemini | Claude |
|--------|--------|--------|
| Loop function identified | `MC` (name only) | `ry` → `ea1` (two-layer structure, full state vars) |
| `callModel()` parameter shape | No | Yes, with all fields |
| SSE event flow documented | No | Yes (`message_start` → `content_block_delta` → `message_stop`) |
| Streaming tool executor (`qoH`) | No | Yes — tools start executing mid-stream |
| Concurrency grouping logic | No | Yes (`Bg1`, `pg1` parallel, `mg1` sequential) |
| Context management (compact) | No | Yes (microcompact, autocompact, reactive) |
| Max output token recovery | No | Yes (inject "resume mid-thought" message) |
| Model fallback (404 → switch) | No | Yes (tombstone prior messages, retry) |
| Stop hooks | No | Yes |
| Subagent recursion | No | Yes (Agent tool → new `ea1` loop) |
| Exit condition catalog | Partial (1) | Full (8 conditions) |

### Prescriptive recommendations

Gemini gave 3 concrete recommendations for loopr: hard-cap chat iterations to 3, enforce parallel tool strategy via prompting, use delegate for heavy work. Claude's doc was purely descriptive — deeper reference, less actionable.

### Conceptual framing

Gemini opened with a useful "physical constraint" explanation (CLI agent vs cloud sandbox) that Claude's doc skips.

---

## What Neither Document Adequately Covered: Daemon and IPC

This is the critical gap. Both documents focused on the agentic turn loop and largely ignored Claude Code's process architecture and IPC layer. A deeper pass through the binary reveals Claude Code has a **substantial IPC and multi-process architecture** that neither doc properly documented.

### What actually exists in Claude Code

#### 1. SessionsWebSocket (`TrH`)

A persistent WebSocket connection to Anthropic's server at:
```
wss://<BASE_API_URL>/v1/sessions/ws/<sessionId>/subscribe?organization_uuid=<orgUuid>
```

This is **not** just for streaming API responses. It's a bidirectional control plane that handles:
- `control_request` / `control_response` — structured IPC messages
- `permission_request` / `permission_response` — tool approval bubbling
- `sandbox_permission_request` / `sandbox_permission_response` — sandbox network access
- `shutdown_request` / `shutdown_approved` / `shutdown_rejected` — coordinated shutdown
- `idle_notification` — agent idle state with reason, summary, completed task info
- `plan_approval_request` / `plan_approval_response` — plan mode approval
- `mode_set_request` — permission mode changes
- `team_permission_update` — team-wide permission propagation
- Reconnection logic (up to 5 attempts, 2s delay, 30s ping interval)

#### 2. RemoteAgentTask

When an agent runs remotely (via `claude-code` remote sessions or cowork mode), a `RemoteAgentTask` object:
- Opens a `SessionsWebSocket` to the remote session
- Forwards `can_use_tool` permission requests from the remote agent back to the local `ToolUseConfirmQueue` (the user's TUI)
- Auto-approves known-safe tools locally
- Denies permission if no local REPL is registered
- Sets remote permission mode (defaults to `"plan"`)

This is how a remote agent can request permission from the local user — the IPC bridges the gap.

#### 3. RemoteSessionManager (`rHL`)

A higher-level manager that:
- Wraps `SessionsWebSocket`
- Routes `control_request` messages to appropriate handlers
- Tracks `pendingPermissionRequests` by request ID
- Provides `sendMessage()` to push user messages into remote sessions
- Provides `cancelSession()` → sends `interrupt` control request
- Filters out `control_request`/`control_response` from the message stream before passing to UI callbacks

#### 4. Cowork Mode (`CLAUDE_CODE_IS_COWORK`)

Environment variable `CLAUDE_CODE_IS_COWORK` triggers special behavior throughout the codebase:
- `CLAUDE_CODE_EAGER_FLUSH` — forces immediate UI flushing at result boundaries
- Separate settings file: `cowork_settings.json` instead of `settings.json`
- Separate plugin directory: `cowork_plugins` instead of `plugins`
- Telemetry fields: `is_conductor`, `coworker_type`
- The "conductor" is the orchestrating instance; coworkers are spawned agents

#### 5. In-Process Runner / Teammate Mailbox

There's an in-process multi-agent runner with:
- A **mailbox** system (`teammate_mailbox` attachment type) for inter-agent messaging
- Agents read from their mailbox, processing messages in priority order
- `shutdown_request` messages are prioritized over regular mailbox messages
- `idle_notification` messages carry: `idleReason`, `summary`, `completedTaskId`, `completedStatus`, `failureReason`
- `teammate_terminated` notifications
- A leader agent (`s6`) whose messages are prioritized in mailbox reads

#### 6. IPC Message Protocol

The full IPC message type union:
```
permission_request | permission_response
sandbox_permission_request | sandbox_permission_response
shutdown_request | shutdown_approved | shutdown_rejected
team_permission_update
mode_set_request
plan_approval_request | plan_approval_response
idle_notification
```

Each has a Zod schema for validation. The `isControlMessage()` function gates which messages are handled by the control plane vs passed through to the UI.

### What this means for loopr

Claude Code's "simple loop" narrative is incomplete. For the **single-user interactive case** (what we've been focused on), the loop IS simple — `while(true)` stream model, execute tools, repeat. But Claude Code also has:

1. **A WebSocket-based control plane** for remote sessions and permission forwarding
2. **A cowork/conductor architecture** for multi-agent coordination
3. **A mailbox-based IPC system** for in-process teammate communication
4. **Coordinated shutdown** with request/approve/reject handshake

Loopr's daemon + IPC architecture is arguably closer to this reality than either research doc initially acknowledged. The question isn't whether IPC is needed — Claude Code clearly uses it. The question is whether loopr's IPC is being used for the right things at the right layer.

---

## Corrected Summary

| Aspect | Gemini | Claude | Actually in Claude Code |
|--------|--------|--------|------------------------|
| Core agentic loop | Yes | Yes (deeper) | Confirmed |
| Parallel tool execution | Yes | Yes (implementation detail) | Confirmed |
| Streaming tool overlap | No | Yes | Confirmed |
| Context management | No | Yes | Confirmed |
| Daemon / background process | Not addressed | Not addressed | Cowork mode with conductor/coworker roles |
| WebSocket control plane | Not addressed | Not addressed | `SessionsWebSocket` to Anthropic servers |
| Permission IPC | Not addressed | Partial (mentioned, not detailed) | Full bidirectional via WebSocket + local queue |
| Multi-agent mailbox | Not addressed | Not addressed | `teammate_mailbox` with priority reads |
| Coordinated shutdown | Not addressed | Not addressed | `shutdown_request` / `approved` / `rejected` |
| Actionable recommendations | Yes (3 specific) | Minimal | — |
