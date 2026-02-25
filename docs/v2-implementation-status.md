# v2 Implementation Status

**Date:** 2026-02-25
**Purpose:** Map docs to what's actually implemented vs. aspirational

---

## Fully Implemented

| Component | Files | Doc |
|-----------|-------|-----|
| Client fork-to-daemon | `src/main.rs` | [v2-proven-patterns.md](v2-proven-patterns.md) |
| Daemon core + select! loop | `src/daemon/mod.rs` | [architecture.md](architecture.md) |
| DaemonContext shared state | `src/daemon/context.rs` | [architecture.md](architecture.md) |
| IPC server (Unix socket, NDJSON) | `src/ipc/server.rs` | [ipc-protocol.md](ipc-protocol.md) |
| IPC client (request/response correlation) | `src/ipc/client.rs` | [ipc-protocol.md](ipc-protocol.md) |
| IPC messages (Request, Response, Event) | `src/ipc/messages.rs` | [ipc-protocol.md](ipc-protocol.md) |
| Codec layer (NDJSON + length-prefixed) | `src/ipc/codec.rs` | [ipc-protocol.md](ipc-protocol.md) |
| Version handshake | `src/daemon/mod.rs`, `src/ipc/client.rs` | [v2-proven-patterns.md](v2-proven-patterns.md) |
| PID file lifecycle | `src/daemon/mod.rs` | [process-model.md](process-model.md) |
| Signal handling (SIGTERM/SIGINT) | `src/daemon/mod.rs` | [process-model.md](process-model.md) |
| TUI app (ratatui, 3 views) | `src/tui/` | [tui.md](tui.md) |
| Chat view + async send | `src/main.rs`, `src/tui/views.rs` | [tui.md](tui.md) |
| Loops view (tree display) | `src/tui/views.rs` | [tui.md](tui.md) |
| Approval view | `src/tui/views.rs` | [tui.md](tui.md) |
| CLI commands via IPC | `src/main.rs`, `src/cli/commands.rs` | [ipc-protocol.md](ipc-protocol.md) |
| Daemon commands (start/stop/status/restart) | `src/main.rs` | [process-model.md](process-model.md) |
| Event broadcast to TUI clients | `src/ipc/server.rs`, `src/daemon/context.rs` | [observability.md](observability.md) |
| Request handler dispatch | `src/daemon/mod.rs`, `src/daemon/handlers/` | [ipc-protocol.md](ipc-protocol.md) |
| Chat handler (LLM integration) | `src/daemon/handlers/chat.rs` | [llm-client.md](llm-client.md) |
| Loop CRUD handlers | `src/daemon/handlers/loops.rs` | [ipc-protocol.md](ipc-protocol.md) |
| Plan approval handlers | `src/daemon/handlers/plan.rs` | [loop-architecture.md](loop-architecture.md) |
| Domain types (Loop struct, enums) | `src/domain/` | [domain-types.md](domain-types.md) |
| Storage layer (TaskStore wrapper) | `src/storage/` | [persistence.md](persistence.md) |
| LLM client (Anthropic API) | `src/llm/` | [llm-client.md](llm-client.md) |
| Tool system (catalog, router, definitions) | `src/tools/` | [tools.md](tools.md) |
| Prompt system (Handlebars templates) | `src/prompt/` | docs mention but no dedicated doc |
| Validation system (composite, format, command) | `src/validation/` | [loop-validation.md](loop-validation.md) |
| Loop manager + spawner | `src/manager/` | [loop-architecture.md](loop-architecture.md) |
| Scheduler (priority-based) | `src/daemon/scheduler.rs` | [scheduler.md](scheduler.md) |
| Tick loop state tracking | `src/daemon/tick.rs` | [execution-model.md](execution-model.md) |
| Crash recovery | `src/daemon/recovery.rs` | [execution-model.md](execution-model.md) |
| Signal coordination | `src/coordination/` | [loop-coordination.md](loop-coordination.md) |
| Worktree management | `src/worktree/` | [worktree-coordination.md](worktree-coordination.md) |
| Artifact parsing | `src/artifact/` | [artifact-tools.md](artifact-tools.md) |
| ID generation (timestamp + random) | `src/id.rs` | [domain-types.md](domain-types.md) |
| Error types | `src/error.rs` | -- |
| Config loading (YAML) | `src/config.rs` | [configuration-reference.md](configuration-reference.md) |

## Designed but Not Fully Wired

| Component | Status | Doc |
|-----------|--------|-----|
| Runner processes (no-net/net/heavy) | `src/runner/mod.rs` exists, types defined, but runners not spawned by daemon | [runners.md](runners.md) |
| Tool execution via runners | Tool router exists but uses local execution, not runner subprocess IPC | [tools.md](tools.md) |
| Network sandboxing | Documented but not implemented | [runners.md](runners.md) |
| Full loop execution cycle | Individual pieces exist but end-to-end loop iteration not wired | [loop.md](loop.md) |
| Rebase-on-merge protocol | Signal types exist but merge coordination not implemented | [worktree-coordination.md](worktree-coordination.md) |
| Invalidation cascade | Signal manager exists but cascade logic not wired | [loop-coordination.md](loop-coordination.md) |
| LLM-as-Judge validation | Validation trait exists but LLM review pass not implemented | [loop-validation.md](loop-validation.md) |
| Streaming LLM to TUI | Event types exist but streaming chunks not flowing end-to-end | [tui.md](tui.md) |
| Plan approval → spec spawning | Handlers exist but spec creation from approved plan not wired | [loop-architecture.md](loop-architecture.md) |
| Rule of Five review passes | Documented but not implemented | [rule-of-five.md](rule-of-five.md) |

## Pure Design Docs (Not Implemented)

| Doc | Summary |
|-----|---------|
| [chatgpt-loopr-architecture-conversation.md](chatgpt-loopr-architecture-conversation.md) | Raw conversation establishing architecture concepts |
| [claude-loopr-mvp-and-fsm-conversation.md](claude-loopr-mvp-and-fsm-conversation.md) | Raw conversation on MVP phasing and FSM design |
| [architecture-comparison-taskdaemon.md](architecture-comparison-taskdaemon.md) | Comparison of Loopr vs TaskDaemon patterns |
| [implementation-phases.md](implementation-phases.md) | 16-phase build plan |
| [implementation-patterns.md](implementation-patterns.md) | Patterns extracted from taskdaemon for reuse |
| [conflicts.md](conflicts.md) | v1→v2 decision log |

---

## Module Map

```
src/
├── main.rs                 # Entry point: TUI, daemon commands, CLI commands
├── lib.rs                  # Public module exports
├── config.rs               # YAML config loading
├── error.rs                # LooprError enum
├── id.rs                   # ID generation (timestamp + random)
│
├── cli/                    # CLI argument parsing
│   ├── mod.rs
│   └── commands.rs         # Clap subcommands
│
├── daemon/                 # Daemon process
│   ├── mod.rs              # Daemon struct, run(), stop(), request dispatch
│   ├── context.rs          # DaemonContext (shared state for handlers)
│   ├── handlers/           # Request handler implementations
│   │   ├── mod.rs
│   │   ├── chat.rs         # chat.send, chat.clear, chat.cancel
│   │   ├── loops.rs        # loop.list, loop.get, loop.create_plan, etc.
│   │   └── plan.rs         # plan.approve, plan.reject, plan.iterate
│   ├── scheduler.rs        # Priority-based loop scheduling
│   ├── tick.rs             # Tick loop config and state
│   └── recovery.rs         # Crash recovery for interrupted loops
│
├── ipc/                    # Inter-process communication
│   ├── mod.rs
│   ├── messages.rs         # DaemonRequest, DaemonResponse, DaemonEvent
│   ├── server.rs           # Unix socket server (daemon side)
│   ├── client.rs           # Unix socket client (TUI/CLI side)
│   └── codec.rs            # NDJSON and length-prefixed codecs
│
├── tui/                    # Terminal UI
│   ├── mod.rs
│   ├── app.rs              # App state, AppConfig, view management
│   ├── input.rs            # Keyboard event handling
│   └── views.rs            # ChatView, LoopsView, ApprovalView
│
├── domain/                 # Core domain types
│   ├── mod.rs
│   ├── loop_record.rs      # Loop struct, LoopType, LoopStatus
│   ├── event.rs            # EventRecord
│   ├── signal.rs           # SignalRecord, SignalType
│   ├── tool_job.rs         # ToolJobRecord
│   └── outcome.rs          # Outcome types
│
├── storage/                # Persistence (TaskStore wrapper)
│   ├── mod.rs
│   └── loops.rs
│
├── llm/                    # LLM client
│   ├── mod.rs
│   ├── client.rs           # LlmClient trait
│   ├── anthropic.rs        # AnthropicClient implementation
│   ├── types.rs            # Message, ToolUse, etc.
│   ├── streaming.rs        # Stream handling
│   └── tool_parser.rs      # Parse tool_use from API responses
│
├── tools/                  # Tool system
│   ├── mod.rs
│   ├── catalog.rs          # ToolCatalog (TOML-based)
│   ├── definition.rs       # ToolDefinition struct
│   └── router.rs           # LocalToolRouter
│
├── prompt/                 # Prompt templates
│   ├── mod.rs
│   ├── loader.rs           # Load .md templates
│   └── render.rs           # Handlebars rendering
│
├── validation/             # Validation system
│   ├── mod.rs
│   ├── traits.rs           # Validator trait
│   ├── format.rs           # Format/syntax validation
│   ├── command.rs          # Command execution validation
│   └── composite.rs        # Multi-layer composite validator
│
├── manager/                # Loop lifecycle management
│   ├── mod.rs
│   ├── loop_manager.rs     # LoopManager (schedule, spawn, reap)
│   └── spawner.rs          # Loop spawning logic
│
├── coordination/           # Loop coordination
│   ├── mod.rs
│   ├── signals.rs          # SignalManager
│   └── invalidate.rs       # Invalidation cascade
│
├── artifact/               # Structured output from LLM
│   ├── mod.rs
│   ├── parser.rs           # Parse artifacts from tool_use
│   ├── spec.rs             # Spec artifact type
│   └── plan.rs             # Plan artifact type
│
├── runner/                 # Tool execution runners
│   └── mod.rs              # Runner types (not fully wired)
│
└── worktree/               # Git worktree management
    ├── mod.rs
    └── manager.rs          # WorktreeManager
```
