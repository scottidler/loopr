# Architecture Comparison: Loopr vs TaskDaemon

**Date:** 2026-02-01
**Purpose:** Compare Loopr's daemon architecture with the previous TaskDaemon implementation

## Summary

Both systems implement the same core concept (a daemon orchestrating LLM-driven loops), but with different architectural approaches:

| Aspect | **Loopr** | **TaskDaemon** |
|--------|-----------|----------------|
| **Module count** | ~50 files | ~96 files |
| **IPC Protocol** | JSON-RPC style request/response | Simple message types (Wake/Shutdown/Ping) |
| **CLI↔Daemon** | Full RPC (all operations via IPC) | Direct state access + minimal IPC |
| **Daemon Scope** | Minimal - just orchestration | Heavy - runs the full task loop |

## Key Differences

### 1. CLI Command Execution

| | Loopr | TaskDaemon |
|--|-------|------------|
| **Plan/List/Status** | CLI → IpcClient → Daemon → Response | CLI → StateManager (direct file access) |
| **Pause/Resume** | CLI → IpcClient → Daemon | CLI → StateManager.pause_execution() |

**Loopr**: CLI commands *always* go through the daemon via IPC socket. The daemon owns all state.

**TaskDaemon**: CLI commands access the TaskStore *directly* via `StateManager`. The daemon is only needed for:
- Running loops (active LLM work)
- Waking immediately via IPC instead of waiting for poll interval

### 2. IPC Complexity

| | Loopr | TaskDaemon |
|--|-------|------------|
| **Messages** | Full RPC with 20+ methods (`loop.list`, `chat.send`, `plan.approve`, etc.) | 4 message types (`Wake`, `Shutdown`, `Ping`, `Pong`) |
| **State ownership** | Daemon owns state | Shared via TaskStore files |

**TaskDaemon's IPC is "wake-up only"** - it just nudges the daemon to check for work immediately. All state queries go through the file-based `StateManager`.

### 3. Daemon Run Loop

**Loopr** (`Daemon::run()`):
```
Create DaemonContext (owns LoopManager, LlmClient, Storage)
Create IpcServer
Loop: Accept connection → Route request → Return response
```

**TaskDaemon** (`run_daemon()`):
```
Spawn StateManager (file-backed)
Spawn Coordinator (inter-loop messaging)
Spawn MainWatcher (git branch monitoring)
Spawn TaskManager with IPC listener
Loop: Run tasks, listen for IPC wake signals
```

TaskDaemon has more components running concurrently (coordinator, watcher, scheduler).

### 4. State Persistence

| | Loopr | TaskDaemon |
|--|-------|------------|
| **Store** | `JsonlStorage` | `StateManager` (with `TaskStore`) |
| **Access** | Via daemon only | Shared read/write by CLI and daemon |
| **Recovery** | Via daemon crash recovery | Built-in with `.pending` files |

## Trade-offs

### Loopr's Approach (Full RPC)

**Pros:**
- Cleaner separation - daemon owns all state
- Easier to reason about concurrency
- No file locking issues

**Cons:**
- CLI commands fail if daemon isn't running
- More IPC latency for simple queries

### TaskDaemon's Approach (Shared State + Wake IPC)

**Pros:**
- CLI works without daemon (for queries/status updates)
- Less IPC overhead
- Daemon can be restarted without losing state visibility

**Cons:**
- File locking/race conditions possible
- State can drift if daemon and CLI modify simultaneously
- More complex state synchronization

## Component Comparison

### TaskDaemon Components (not present in Loopr)

| Component | Purpose |
|-----------|---------|
| `Coordinator` | Inter-loop messaging and event persistence |
| `MainWatcher` | Git main branch monitoring for invalidation |
| `Scheduler` | API rate limiting with token bucket |
| `TaskManager` | Task orchestration with concurrent loop limits |
| `LoopLoader` | Dynamic loop type loading from YAML |

### Loopr Components (not present in TaskDaemon)

| Component | Purpose |
|-----------|---------|
| `DaemonContext` | Shared state container for request handlers |
| `AsyncDaemonHandler` | Async request routing |
| `ChatSession` | Conversation history for TUI chat |
| `CompositeValidator` | Pluggable validation rules |

## Recommendations

If continuing with Loopr's architecture:

1. **Keep the full-RPC model** - simpler concurrency, cleaner state ownership
2. **Daemon auto-start is good** - already implemented for CLI commands
3. **Consider offline fallback** - add file-based read path for `loop.list`/`loop.get` when daemon is offline (like TaskDaemon does)
4. **Evaluate need for components** - TaskDaemon's Coordinator/Watcher/Scheduler may be needed as Loopr matures

## File Structure Comparison

```
TaskDaemon (96 files)              Loopr (50 files)
├── coordinator/                   ├── daemon/
│   ├── core.rs                   │   ├── context.rs
│   ├── handle.rs                 │   ├── handlers/
│   ├── messages.rs               │   │   ├── chat.rs
│   └── persistence.rs            │   │   ├── loops.rs
├── events/                       │   │   └── plan.rs
│   ├── bus.rs                    │   ├── recovery.rs
│   └── types.rs                  │   ├── scheduler.rs
├── ipc/                          │   └── tick.rs
│   ├── client.rs                 ├── ipc/
│   ├── listener.rs               │   ├── client.rs
│   └── messages.rs               │   ├── messages.rs
├── loop/                         │   └── server.rs
│   ├── engine.rs                 ├── manager/
│   ├── manager.rs                │   └── loop_manager.rs
│   └── metrics.rs                ├── domain/
├── scheduler/                    │   └── loop_record.rs
│   ├── core.rs                   └── ...
│   └── queue.rs
├── watcher/
│   └── main_watcher.rs
└── ...
```

## References

- Loopr source: `/home/saidler/repos/scottidler/loopr/src/`
- TaskDaemon source: `/home/saidler/repos/taskdaemon/taskdaemon/td/src/`
