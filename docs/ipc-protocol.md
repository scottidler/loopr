# IPC Protocol

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

Loopr uses Unix sockets for inter-process communication. Two protocols:
1. **TUI ↔ Daemon**: JSON-RPC for commands and events
2. **Daemon ↔ Runners**: Binary protocol for tool jobs

---

## Socket Locations

```
~/.loopr/
├── daemon.sock           # TUI ↔ Daemon
├── runner-no-net.sock    # Daemon ↔ runner-no-net
├── runner-net.sock       # Daemon ↔ runner-net
└── runner-heavy.sock     # Daemon ↔ runner-heavy
```

---

## TUI ↔ Daemon Protocol

### Transport

Newline-delimited JSON (JSON Lines) over Unix stream socket.

**Note:** This is NOT JSON-RPC. The message schema uses similar field names (`id`, `method`, `params`, `result`, `error`) for familiarity, but does not implement the JSON-RPC 2.0 specification. There's no `jsonrpc: "2.0"` field, and error codes are application-specific.

### Request/Response

```rust
/// TUI sends request
#[derive(Debug, Serialize, Deserialize)]
struct DaemonRequest {
    id: u64,
    method: String,
    params: Value,
}

/// Daemon sends response
#[derive(Debug, Serialize, Deserialize)]
struct DaemonResponse {
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<DaemonError>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DaemonError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}
```

### Push Events

```rust
/// Daemon pushes events (no request ID)
#[derive(Debug, Serialize, Deserialize)]
struct DaemonEvent {
    event: String,
    data: Value,
}
```

### Methods

#### Connection

| Method | Params | Result | Description |
|--------|--------|--------|-------------|
| `connect` | `{ version: string }` | `{ session_id, state }` | Initialize connection |
| `disconnect` | `{}` | `{}` | Graceful disconnect |
| `ping` | `{}` | `{ pong: true }` | Keep-alive |

#### Chat

| Method | Params | Result | Description |
|--------|--------|--------|-------------|
| `chat.send` | `{ message: string }` | `{ message_id }` | Send user message |
| `chat.cancel` | `{}` | `{}` | Cancel streaming response |
| `chat.clear` | `{}` | `{}` | Clear conversation |

#### Loops

| Method | Params | Result | Description |
|--------|--------|--------|-------------|
| `loop.list` | `{}` | `{ loops: Loop[] }` | List all loops |
| `loop.get` | `{ id: string }` | `{ loop: Loop }` | Get loop details |
| `loop.create_plan` | `{ description: string }` | `{ loop_id }` | Create plan loop |
| `loop.start` | `{ id: string }` | `{}` | Start pending loop |
| `loop.pause` | `{ id: string }` | `{}` | Pause running loop |
| `loop.resume` | `{ id: string }` | `{}` | Resume paused loop |
| `loop.cancel` | `{ id: string }` | `{}` | Cancel loop |
| `loop.delete` | `{ id: string }` | `{}` | Delete loop |

#### Plan Approval (User Gate)

| Method | Params | Result | Description |
|--------|--------|--------|-------------|
| `plan.approve` | `{ id: string }` | `{ specs_spawned: u32 }` | Approve plan, spawn specs |
| `plan.reject` | `{ id: string, reason?: string }` | `{}` | Reject plan, mark failed |
| `plan.iterate` | `{ id: string, feedback: string }` | `{}` | Request another iteration |
| `plan.get_preview` | `{ id: string }` | `{ content, specs }` | Get plan content for review |

#### Metrics

| Method | Params | Result | Description |
|--------|--------|--------|-------------|
| `metrics.get` | `{}` | `{ ... }` | Get current metrics |

### Events

**See [observability.md](observability.md) for the complete event taxonomy.**

| Event | Data | Description |
|-------|------|-------------|
| `chat.chunk` | `{ text, done }` | Streaming LLM response |
| `chat.tool_call` | `{ tool, input }` | Tool being called |
| `chat.tool_result` | `{ tool, output }` | Tool completed |
| `loop.created` | `{ loop: Loop }` | New loop created |
| `loop.updated` | `{ loop: Loop }` | Loop state changed |
| `loop.iteration` | `{ id, iteration, status }` | Iteration completed |
| `loop.artifact` | `{ id, path }` | Artifact produced |
| `plan.awaiting_approval` | `{ id, content, specs }` | Plan ready for user review |
| `plan.approved` | `{ id, specs_spawned }` | Plan was approved |
| `plan.rejected` | `{ id, reason }` | Plan was rejected |
| `metrics.update` | `{ ... }` | Metrics changed |

### Example Exchange

```json
// TUI → Daemon: Send chat message
{"id":1,"method":"chat.send","params":{"message":"Build a REST API"}}

// Daemon → TUI: Acknowledge
{"id":1,"result":{"message_id":"msg-001"}}

// Daemon → TUI: Streaming chunks
{"event":"chat.chunk","data":{"text":"I'll help you ","done":false}}
{"event":"chat.chunk","data":{"text":"build a REST API.","done":false}}
{"event":"chat.tool_call","data":{"tool":"read_file","input":{"path":"src/main.rs"}}}
{"event":"chat.tool_result","data":{"tool":"read_file","output":"// main.rs\nfn main()..."}}
{"event":"chat.chunk","data":{"text":"\n\nBased on the code...","done":true}}
```

---

## Daemon ↔ Runner Protocol

### Transport

Length-prefixed binary messages over Unix stream socket.

```
+--------+--------+------------------+
| Length | Type   | Payload (JSON)   |
| 4 bytes| 1 byte | variable         |
+--------+--------+------------------+
```

### Message Types

| Type | Value | Direction | Description |
|------|-------|-----------|-------------|
| `Handshake` | 0x01 | Runner → Daemon | Runner announces itself |
| `HandshakeAck` | 0x02 | Daemon → Runner | Daemon accepts runner |
| `Job` | 0x10 | Daemon → Runner | Submit tool job |
| `JobAck` | 0x11 | Runner → Daemon | Job received |
| `Result` | 0x12 | Runner → Daemon | Job completed |
| `Cancel` | 0x20 | Daemon → Runner | Cancel job |
| `CancelAck` | 0x21 | Runner → Daemon | Cancel acknowledged |
| `OutputChunk` | 0x30 | Runner → Daemon | Streaming output |
| `Heartbeat` | 0x40 | Runner → Daemon | Keep-alive |
| `HeartbeatAck` | 0x41 | Daemon → Runner | Keep-alive response |
| `Shutdown` | 0xF0 | Daemon → Runner | Graceful shutdown |

### Schemas

#### Handshake

```rust
/// Runner → Daemon
#[derive(Debug, Serialize, Deserialize)]
struct RunnerHandshake {
    lane: String,           // "no-net", "net", "heavy"
    pid: u32,
    version: String,
    capabilities: Vec<String>,
}

/// Daemon → Runner
#[derive(Debug, Serialize, Deserialize)]
struct HandshakeAck {
    accepted: bool,
    slots: usize,
    config: RunnerConfig,
}
```

#### Tool Job

```rust
/// Daemon → Runner
#[derive(Debug, Serialize, Deserialize)]
struct ToolJob {
    job_id: String,
    agent_id: String,
    tool_name: String,
    command: String,        // Shell command to execute
    argv: Vec<String>,      // Alternative: explicit argv
    cwd: PathBuf,
    worktree_dir: PathBuf,
    env: HashMap<String, String>,
    timeout_ms: u64,
    max_output_bytes: usize,
    file_paths: Vec<PathBuf>,  // Paths to validate
}

/// Runner → Daemon
#[derive(Debug, Serialize, Deserialize)]
struct JobAck {
    job_id: String,
    accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
```

#### Tool Result

```rust
/// Runner → Daemon
#[derive(Debug, Serialize, Deserialize)]
struct ToolResult {
    job_id: String,
    status: ToolExitStatus,
    output: String,
    exit_code: Option<i32>,
    was_timeout: bool,
    was_cancelled: bool,
    duration_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
enum ToolExitStatus {
    Success,
    Failed,
    Timeout,
    Cancelled,
    Error,
}
```

#### Cancel

```rust
/// Daemon → Runner
#[derive(Debug, Serialize, Deserialize)]
struct CancelRequest {
    job_id: String,
}

/// Runner → Daemon
#[derive(Debug, Serialize, Deserialize)]
struct CancelAck {
    job_id: String,
    was_running: bool,
}
```

#### Output Chunk (Streaming)

```rust
/// Runner → Daemon (for long-running jobs)
#[derive(Debug, Serialize, Deserialize)]
struct OutputChunk {
    job_id: String,
    chunk: Vec<u8>,
    stream: OutputStream,  // Stdout or Stderr
}

#[derive(Debug, Serialize, Deserialize)]
enum OutputStream {
    Stdout,
    Stderr,
}
```

#### Heartbeat

```rust
/// Runner → Daemon
#[derive(Debug, Serialize, Deserialize)]
struct Heartbeat {
    active_jobs: usize,
    uptime_secs: u64,
    memory_bytes: u64,
}

/// Daemon → Runner
#[derive(Debug, Serialize, Deserialize)]
struct HeartbeatAck {
    timestamp: u64,
}
```

---

## Connection Lifecycle

### TUI Connection

```
TUI                                    Daemon
 │                                        │
 │──────── connect() ────────────────────>│
 │                                        │
 │<─────── { session_id, state } ─────────│
 │                                        │
 │<─────── events (push) ─────────────────│
 │                                        │
 │──────── requests ─────────────────────>│
 │<─────── responses ─────────────────────│
 │                                        │
 │──────── disconnect() ─────────────────>│
 │                                        │
```

### Runner Connection

```
Runner                                 Daemon
 │                                        │
 │──────── Handshake ────────────────────>│
 │                                        │
 │<─────── HandshakeAck ──────────────────│
 │                                        │
 │<─────── Job ───────────────────────────│
 │──────── JobAck ───────────────────────>│
 │                                        │
 │──────── OutputChunk (opt) ────────────>│
 │──────── OutputChunk (opt) ────────────>│
 │                                        │
 │──────── Result ───────────────────────>│
 │                                        │
 │──────── Heartbeat ────────────────────>│
 │<─────── HeartbeatAck ──────────────────│
 │                                        │
 │<─────── Shutdown ──────────────────────│
 │                                        │
```

---

## Error Codes

### TUI Protocol

| Code | Name | Description |
|------|------|-------------|
| -32700 | ParseError | Invalid JSON |
| -32600 | InvalidRequest | Invalid request object |
| -32601 | MethodNotFound | Unknown method |
| -32602 | InvalidParams | Invalid parameters |
| -32603 | InternalError | Internal daemon error |
| 1001 | LoopNotFound | Loop ID doesn't exist |
| 1002 | InvalidState | Loop in wrong state for action |
| 1003 | Unauthorized | Action not permitted |

### Runner Protocol

| Status | Description |
|--------|-------------|
| `Error` | Job rejected (path violation, etc.) |
| `Timeout` | Job exceeded timeout |
| `Cancelled` | Job was cancelled |
| `Failed` | Job exited non-zero |
| `Success` | Job completed successfully |

---

## Implementation Notes

### Framing (Runner Protocol)

```rust
async fn write_message(socket: &mut UnixStream, msg_type: u8, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    socket.write_all(&len.to_be_bytes()).await?;
    socket.write_all(&[msg_type]).await?;
    socket.write_all(payload).await?;
    Ok(())
}

async fn read_message(socket: &mut UnixStream) -> Result<(u8, Vec<u8>)> {
    let mut len_buf = [0u8; 4];
    socket.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut type_buf = [0u8; 1];
    socket.read_exact(&mut type_buf).await?;
    let msg_type = type_buf[0];

    let mut payload = vec![0u8; len];
    socket.read_exact(&mut payload).await?;

    Ok((msg_type, payload))
}
```

### TUI Reconnection

If daemon connection drops, TUI should:
1. Attempt reconnect with exponential backoff
2. On reconnect, request full state refresh
3. Show "reconnecting..." indicator to user

### Runner Recovery

If runner dies mid-job:
1. Daemon detects socket close
2. Marks pending jobs as failed
3. Spawns replacement runner
4. Does NOT retry jobs automatically (idempotency concerns)

---

## References

- [architecture.md](architecture.md) - System overview
- [process-model.md](process-model.md) - Process lifecycle
- [runners.md](runners.md) - Runner details
- [observability.md](observability.md) - Complete event taxonomy
