# Architecture Overview

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

Loopr v2 uses a three-tier process model: **TUI** (user interface), **Daemon** (orchestration), and **Runners** (tool execution). This separation enables the TUI to detach/reattach without interrupting loops, tools to run in sandboxed environments, and clean process management with killable process groups.

---

## Process Model

```
                     ┌─────────────┐
                     │    User     │
                     └──────┬──────┘
                            │
                            ▼
                     ┌─────────────┐
                     │     TUI     │◄──── Can detach/reattach
                     │  (ratatui)  │
                     └──────┬──────┘
                            │ Unix Socket
                            ▼
┌───────────────────────────────────────────────────────────────────┐
│                           Daemon                                   │
│                                                                    │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌──────────┐  │
│  │LoopManager  │  │ LlmClient   │  │ ToolRouter  │  │TaskStore │  │
│  │             │  │             │  │             │  │          │  │
│  │ - Scheduler │  │ - Anthropic │  │ - Lane      │  │ - JSONL  │  │
│  │ - Loops[]   │  │ - Streaming │  │   routing   │  │ - SQLite │  │
│  │ - Worktrees │  │ - Tokens    │  │ - Job queue │  │          │  │
│  └─────────────┘  └─────────────┘  └──────┬──────┘  └──────────┘  │
│                                           │                        │
└───────────────────────────────────────────┼────────────────────────┘
                    │                       │                    │
                    ▼                       ▼                    ▼
           ┌─────────────┐         ┌─────────────┐      ┌─────────────┐
           │runner-no-net│         │ runner-net  │      │runner-heavy │
           │             │         │  (default)  │      │             │
           │ No network  │         │ Network OK  │      │ Builds/tests│
           │ File I/O    │         │ Web access  │      │ Low concur. │
           └─────────────┘         └─────────────┘      └─────────────┘
```

---

## Component Responsibilities

### TUI Client

Interactive terminal frontend. Connects to daemon via Unix socket.

**Responsibilities:**
- Render Chat and Loops views (ratatui)
- Accept user input, send commands to daemon
- Display streaming LLM responses
- Show loop status tree
- Can run without daemon for offline viewing

**Does NOT:**
- Execute tools
- Make LLM API calls
- Manage loop state directly

### Daemon

Long-running orchestrator. Runs as background process (can be started by TUI or independently).

**Responsibilities:**
- **LoopManager**: Schedule and execute loops (tokio tasks)
- **LlmClient**: Make Anthropic API calls, handle streaming
- **ToolRouter**: Route tool calls to appropriate runner
- **TaskStore**: Read/write all persistent state
- Event streaming to connected TUIs

**Lifecycle:**
- Starts on first `loopr` invocation (or explicit `loopr daemon start`)
- Continues running after TUI disconnects
- Graceful shutdown on `loopr daemon stop`

### Runners

Tool execution workers. Separate processes spawned by daemon.

**Responsibilities:**
- Execute tool jobs (file I/O, commands, web requests)
- Enforce sandbox constraints (cwd, path allowlists)
- Apply timeouts and output limits
- Spawn subprocesses in process groups for clean kill

**Three lanes:**

| Lane | Network | Use Case | Default Concurrency |
|------|---------|----------|---------------------|
| `runner-no-net` | Blocked | File ops, local git | 10 |
| `runner-net` | Allowed | Web fetch, API calls | 5 |
| `runner-heavy` | Allowed | Builds, tests, CI | 1 |

---

## Communication

### TUI ↔ Daemon

Unix socket at `~/.loopr/daemon.sock`

**Protocol:** JSON-RPC over newline-delimited JSON

```rust
// Request from TUI
struct DaemonRequest {
    id: u64,
    method: String,     // "chat", "create_plan", "cancel_loop", etc.
    params: Value,
}

// Response to TUI
struct DaemonResponse {
    id: u64,
    result: Option<Value>,
    error: Option<DaemonError>,
}

// Push events (daemon → TUI)
struct DaemonEvent {
    event: String,      // "loop_update", "chat_chunk", "tool_result"
    data: Value,
}
```

### Daemon ↔ Runners

Unix sockets at `~/.loopr/runner-{lane}.sock`

**Protocol:** Job queue with structured messages

```rust
// Daemon sends job
struct ToolJob {
    job_id: String,
    agent_id: String,
    tool_name: String,
    input: Value,
    cwd: PathBuf,
    worktree_dir: PathBuf,
    timeout_ms: u64,
    max_output_bytes: usize,
}

// Runner returns result
struct ToolResult {
    job_id: String,
    status: ToolExitStatus,
    output: String,
    exit_code: Option<i32>,
    was_timeout: bool,
    was_cancelled: bool,
}
```

---

## Persistence

All persistent state lives in TaskStore at `~/.loopr/<project-hash>/`:

```
~/.loopr/<project-hash>/
├── .taskstore/
│   ├── loops.jsonl           # Loop records (all types)
│   ├── signals.jsonl         # Coordination signals
│   ├── tool_jobs.jsonl       # Tool execution history
│   ├── events.jsonl          # Event stream (debugging)
│   └── taskstore.db          # SQLite index cache
├── loops/
│   └── <loop-id>/
│       ├── iterations/
│       │   └── 001/
│       │       ├── prompt.md
│       │       ├── conversation.jsonl
│       │       └── artifacts/
│       └── current -> iterations/NNN/
└── archive/                   # Invalidated loops
```

**Design principle:** JSONL is source of truth (git-friendly), SQLite is derived cache (fast queries, rebuildable).

---

## Concurrency Controls

### Daemon-Level Semaphores

```rust
struct DaemonConfig {
    // LLM API calls
    llm_slots_global: usize,      // Total concurrent API calls (default: 10)
    llm_slots_per_loop: usize,    // Per-loop limit (default: 1)

    // Tool execution per lane
    tool_slots_no_net: usize,     // runner-no-net (default: 10)
    tool_slots_net: usize,        // runner-net (default: 5)
    tool_slots_heavy: usize,      // runner-heavy (default: 1)

    // Loop execution
    max_concurrent_loops: usize,  // Total running loops (default: 50)
}
```

### Per-Loop Limits

Each loop gets at most one outstanding LLM call and one outstanding tool call at a time. This prevents a single loop from monopolizing resources.

---

## Startup Sequence

```
1. User runs `loopr`
     │
     ▼
2. Check for existing daemon (try connect to daemon.sock)
     │
     ├── Daemon exists → Connect, launch TUI
     │
     └── No daemon → Fork daemon process
           │
           ▼
3. Daemon initializes
     │
     ├── Open/create TaskStore
     ├── Load config (~/.config/loopr/loopr.yml)
     ├── Spawn runners (no-net, net, heavy)
     ├── Wait for runner handshakes
     ├── Create daemon.sock, start accepting connections
     └── Recover incomplete loops from TaskStore
           │
           ▼
4. TUI connects to daemon.sock
     │
     ├── Request current state (loops, chat history)
     ├── Subscribe to events
     └── Enter main render/event loop
```

---

## Shutdown Sequence

### Graceful (user quits TUI)

```
1. TUI sends "disconnect" to daemon
2. TUI exits
3. Daemon continues running (loops keep going)
```

### Daemon stop (`loopr daemon stop`)

```
1. Daemon receives stop signal
2. Send "stop" signals to all running loops
3. Wait for loops to reach checkpoint (max 30s)
4. Send SIGTERM to runners
5. Close TaskStore
6. Exit
```

### Force quit (SIGKILL)

```
1. Daemon dies immediately
2. Runners orphaned (will exit on socket close)
3. On next startup:
   - Daemon finds loops with status=running
   - Marks them as interrupted
   - Offers to resume or cancel
```

---

## Security Considerations

### Process Isolation

- Runners are separate processes (not threads)
- Each runner lane can have different capabilities
- Process groups enable clean kill of subprocess trees

### Path Sandboxing

- All tool file operations constrained to worktree
- Paths validated before execution
- Symlink attacks prevented by canonicalization

### Network Isolation

- `runner-no-net` has no network access (namespace or firewall)
- Prevents accidental data exfiltration
- Code analysis tools run in no-net lane

### Secrets

- API keys stored in environment, not files
- No secrets in TaskStore records
- Worktrees may contain sensitive code (same model as normal development)

---

## Testing Strategy

### Unit Tests

- Individual components (LoopManager, Scheduler, ToolRouter)
- Mock TaskStore and LlmClient

### Integration Tests

- Daemon + Runner communication
- Full loop execution (with test fixtures)

### Chaos Tests

- Kill daemon mid-loop, verify recovery
- Kill runner mid-tool, verify timeout handling
- Disconnect TUI, reconnect, verify state sync

---

## References

- [process-model.md](process-model.md) - Detailed process lifecycle
- [runners.md](runners.md) - Runner lane design
- [ipc-protocol.md](ipc-protocol.md) - Message schemas
- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
