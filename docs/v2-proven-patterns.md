# v2 Proven Patterns: What to Carry Forward

**Author:** Scott A. Idler
**Date:** 2026-02-25
**Status:** Reference for v3

---

## Purpose

This document distills the **implemented and proven** patterns from Loopr v2.
Not aspirational design docs -- this is what actually works in code.
The client-fork-to-daemon + tokio architecture is the spine; everything else builds on it.

---

## 1. Client Fork-to-Daemon Pattern

The single most important pattern: one binary, two modes.

### How It Works

```
$ loopr                    # User runs the binary
    │
    ▼
  Try connect to ~/.loopr/daemon.sock
    │
    ├── Connected → version handshake → launch TUI
    │
    └── Connection failed →
          │
          ▼
        Check PID file (~/.loopr/daemon.pid)
          │
          ├── PID alive but not responding → wait / suggest restart
          │
          └── Not running → auto-start daemon:
                │
                ▼
              Command::new(current_exe())
                .args(["daemon", "start", "--foreground"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                │
                ▼
              Poll with exponential backoff (200ms, 400ms, 800ms, ...)
                │
                ▼
              Connect → version handshake → launch TUI
```

### Key Implementation Details

**Single binary, mode selected by subcommand:**
```rust
#[tokio::main]
async fn main() {
    match &cli.command {
        None => run_tui(config).await,                    // Default: TUI mode
        Some(Commands::Daemon { command }) => {           // Daemon mode
            match command {
                DaemonCommands::Start { foreground } => {
                    if foreground {
                        // Run daemon in THIS process (async context)
                        let mut daemon = Daemon::new(config);
                        daemon.run().await
                    } else {
                        // Fork: spawn self with --foreground, then exit
                        Command::new(current_exe())
                            .args(["daemon", "start", "--foreground"])
                            .spawn()
                    }
                }
                // stop, status, restart...
            }
        }
        Some(Commands::Plan { .. }) => /* CLI command via IPC */,
        // ... other CLI commands
    }
}
```

**Why --foreground matters:** The forked daemon process runs `daemon start --foreground`,
which means the daemon's `run()` method executes in a fresh tokio runtime as the main
async entry point. The parent process that forked it can exit immediately.

**PID file lifecycle:**
- Written by daemon on startup (`~/.loopr/daemon.pid`)
- Checked via `kill(pid, 0)` (signal 0 = existence check, no signal sent)
- Cleaned up on graceful shutdown
- Stale PID files detected when process no longer exists

**Version handshake on connect:**
```
Client connects to socket
    │
    ▼
Client sends: { method: "initialize", params: { version: VERSION } }
    │
    ▼
Daemon checks: client_version == daemon_version?
    │
    ├── Match → { result: { version, protocol, capabilities } }
    │
    └── Mismatch → { error: { code: 1004, data: { client_version, daemon_version } } }
                     │
                     ▼
                   Client disconnects → stops old daemon → restarts → reconnects
```

This prevents subtle bugs from running a stale daemon with a newer client binary.
The VERSION constant is set at compile time from `git describe`.

---

## 2. Tokio Async Architecture

### Daemon Main Loop

The daemon is a single tokio process with `select!` for concurrent operations:

```rust
impl Daemon {
    pub async fn run(&mut self) -> Result<()> {
        self.write_pid()?;
        fs::create_dir_all(&self.config.data_dir)?;

        let mut server = IpcServer::with_config(server_config);
        let event_tx = server.event_sender();  // Shared event channel
        let ctx = Arc::new(DaemonContext::with_event_channel(data_dir, event_tx)?);
        let handler = Arc::new(AsyncDaemonHandler::new(ctx));

        tokio::select! {
            result = server.run(handler) => result,
            _ = signal_handler() => {
                server.shutdown().await;
                Ok(())
            }
        }

        self.remove_pid();
    }
}
```

### IPC Server: Per-Client Tokio Tasks

Each connected client (TUI) gets its own spawned task:

```rust
// Server accept loop
loop {
    tokio::select! {
        Ok((stream, _)) = listener.accept() => {
            let handler = handler.clone();
            let event_rx = event_tx.subscribe();  // broadcast channel

            tokio::spawn(async move {
                handle_client(stream, handler, event_rx).await;
            });
        }
        _ = shutdown_rx.recv() => break,
    }
}

// Per-client handler
async fn handle_client(stream, handler, mut event_rx) {
    let (reader, mut writer) = stream.into_split();
    loop {
        tokio::select! {
            // Incoming request from this client
            line = reader.read_line() => {
                let request = parse(line);
                let response = handler.handle(request).await;
                writer.write_all(serialize(response)).await;
            }
            // Broadcast event to this client
            Ok(event) = event_rx.recv() => {
                if client.subscribed {
                    writer.write_all(serialize(event)).await;
                }
            }
        }
    }
}
```

### Event Broadcasting

Uses `tokio::sync::broadcast` for pub/sub to all connected TUI clients:

```
DaemonContext.event_tx ──broadcast──> Client 1 event_rx
                       ──broadcast──> Client 2 event_rx
                       ──broadcast──> Client N event_rx
```

The IPC server creates the broadcast channel, then shares the sender with
DaemonContext. When handlers broadcast events, they flow through to all
subscribed clients.

---

## 3. IPC Protocol

### Wire Format

Newline-delimited JSON (NDJSON) over Unix domain sockets.

**Request (client → daemon):**
```json
{"id":1,"method":"chat.send","params":{"message":"Build a REST API"}}
```

**Response (daemon → client):**
```json
{"id":1,"result":{"message":"I'll help you build a REST API."}}
```

**Push Event (daemon → client, no request ID):**
```json
{"event":"loop.created","data":{"id":"loop-20260225-143052-a1b2","status":"pending"}}
```

### Message Types

```rust
struct DaemonRequest { id: u64, method: String, params: Value }
struct DaemonResponse { id: u64, result: Option<Value>, error: Option<DaemonError> }
struct DaemonEvent { event: String, data: Value }
struct DaemonError { code: i32, message: String, data: Option<Value> }
```

### Request Dispatch

Async handler with method routing:

```rust
impl RequestHandler for AsyncDaemonHandler {
    async fn handle(&self, request: DaemonRequest) -> DaemonResponse {
        match request.method.as_str() {
            "initialize" => /* version handshake */,
            "ping"       => /* pong */,
            "chat.send"  => handle_chat_send(request, &self.ctx).await,
            "loop.list"  => handle_loop_list(request, &self.ctx).await,
            "loop.create_plan" => handle_loop_create_plan(request, &self.ctx).await,
            "plan.approve" => handle_plan_approve(request, &self.ctx).await,
            _ => DaemonResponse::error(request.id, method_not_found()),
        }
    }
}
```

### Client-Side Request/Response Correlation

The IPC client uses oneshot channels to match responses to requests:

```rust
// Send request
let (tx, rx) = oneshot::channel();
pending.insert(request_id, tx);
writer.write(serialize(request)).await;

// Background reader task routes responses
loop {
    let line = reader.read_line().await;
    if let Ok(response) = parse::<DaemonResponse>(line) {
        if let Some(tx) = pending.remove(&response.id) {
            tx.send(Ok(response));
        }
    } else if let Ok(event) = parse::<DaemonEvent>(line) {
        event_sender.send(event).await;
    }
}

// Caller awaits with timeout
let response = timeout(30s, rx).await;
```

### Error Codes

Standard (JSON-RPC inspired):
- `-32700` ParseError
- `-32601` MethodNotFound
- `-32603` InternalError

Application-specific:
- `1001` LoopNotFound
- `1002` InvalidState
- `1004` VersionMismatch

---

## 4. TUI as Thin Client

### Principle

The TUI does NOT:
- Execute tools
- Make LLM API calls
- Read/write TaskStore directly
- Manage loop state

The TUI ONLY:
- Renders state received from daemon
- Sends user input to daemon via IPC
- Subscribes to push events for live updates

### TUI Event Loop

```rust
async fn run_event_loop(terminal, app) {
    let (response_tx, mut response_rx) = mpsc::channel(10);

    while !app.should_quit {
        terminal.draw(|frame| render(frame, &app.state));

        tokio::select! {
            // Terminal input (keyboard)
            event = event_handler.next() => {
                match event {
                    Key(Enter) => {
                        let msg = take(&mut app.chat_input);
                        let client = app.client();
                        let tx = response_tx.clone();
                        tokio::spawn(async move {
                            let result = client.chat_send(&msg).await;
                            tx.send(ChatResponse { result }).await;
                        });
                    }
                    // ... other keys
                }
            }
            // Async responses from daemon
            Some(response) = response_rx.recv() => {
                app.update_from_response(response);
            }
        }
    }
}
```

### Views

Three views cycling with Tab:
1. **Chat** - Primary interaction, always input-ready
2. **Loops** - Hierarchical tree of loop status
3. **Approval** - Plan review with approve/reject/iterate

### Connection Management

- Auto-connect on startup (with auto-start of daemon)
- Status indicator in header: green (connected), yellow (version mismatch), red (disconnected)
- Version mismatch triggers daemon restart before connecting

---

## 5. DaemonContext: Shared State

All daemon components live in a single `DaemonContext` struct passed to handlers:

```rust
struct DaemonContext {
    loop_manager: Arc<RwLock<LoopManager>>,    // Loop lifecycle
    llm_client: Arc<AnthropicClient>,          // LLM API calls
    tool_router: Arc<LocalToolRouter>,         // Tool execution
    event_tx: broadcast::Sender<DaemonEvent>,  // Event broadcasting
    chat_session: Arc<RwLock<ChatSession>>,     // Chat state
    storage: Arc<StorageWrapper>,              // Persistence
}
```

This is the only place where mutable state is owned. Handlers receive `&DaemonContext`
and use `Arc<RwLock<_>>` for interior mutability where needed.

---

## 6. Signal Handling and Graceful Shutdown

```rust
tokio::select! {
    result = server.run(handler) => result,
    _ = async {
        let mut sigterm = signal(SignalKind::terminate());
        let mut sigint = signal(SignalKind::interrupt());
        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM"),
            _ = sigint.recv() => info!("SIGINT"),
        }
    } => {
        server.shutdown().await;
        Ok(())
    }
}
// PID file always cleaned up after select exits
self.remove_pid();
```

**Daemon stop from CLI:**
```rust
fn stop(pid_path: &Path) -> Result<bool> {
    let pid = read_pid(pid_path)?;
    kill(pid, SIGTERM);
    // Wait up to 3s for graceful exit
    for _ in 0..30 {
        sleep(100ms);
        if kill(pid, 0) != 0 { return Ok(true); }  // Process exited
    }
    // Force kill if still alive
    kill(pid, SIGKILL);
    remove_file(pid_path);
    Ok(true)
}
```

---

## 7. Crash Recovery

On daemon startup, check for loops that were `Running` when daemon died:

```rust
fn recover_all() -> Vec<RecoveryAction> {
    let interrupted = storage.list_all()
        .filter(|l| l.status == Running);

    for loop_record in interrupted {
        if worktree_exists(&loop_record.id) {
            auto_commit(worktree, "WIP: recovery");
            mark_as_pending(loop_record);  // Will be rescheduled
        } else {
            mark_as_failed(loop_record);   // Worktree lost
        }
    }
}
```

---

## 8. Codec Layer

Two codec implementations for different use cases:

**NDJSON (TUI ↔ Daemon):** Simple, human-readable, debuggable with `socat`.
```
{"id":1,"method":"ping","params":{}}\n
{"id":1,"result":{"pong":true}}\n
```

**Length-prefixed JSON (available for Daemon ↔ Runners):**
```
[4 bytes: u32 big-endian length][N bytes: JSON payload]
```

Both implemented as `tokio_util::codec::{Encoder, Decoder}` traits.

---

## 9. CLI Commands via IPC

All CLI subcommands go through the daemon -- the CLI is just another IPC client:

```rust
async fn handle_plan_command(task: &str) {
    let client = IpcClient::with_default_config();
    client.connect().await?;
    let response = client.create_plan(task).await?;
    println!("Plan created: {}", response.result["id"]);
}
```

This means `loopr plan "add auth"`, `loopr list`, `loopr approve plan-001`
all work headlessly without the TUI.

---

## 10. Key File Locations

```
~/.loopr/
├── daemon.sock      # Unix domain socket
├── daemon.pid       # PID file for lifecycle management
├── daemon.version   # Version file for mismatch detection
└── <project-hash>/  # Per-project storage
    └── .taskstore/
        ├── loops.jsonl
        ├── signals.jsonl
        └── taskstore.db
```

---

## What to Keep for v3

1. **Single-binary client/daemon** - `loopr` auto-starts daemon on first run
2. **Fork with --foreground** - Clean daemonization via re-exec
3. **Tokio select! main loop** - Concurrent IPC + signals + tick
4. **NDJSON over Unix socket** - Simple, debuggable IPC
5. **Version handshake** - Prevents stale daemon mismatches
6. **PID file + kill(pid, 0)** - Reliable process lifecycle checks
7. **broadcast channel for events** - Efficient pub/sub to multiple TUI clients
8. **Oneshot channels for request/response** - Clean async correlation
9. **DaemonContext as shared state** - Single owner of all mutable state
10. **Thin TUI client** - No local state mutation, pure display + input
11. **CLI commands via IPC** - Headless operation without TUI
12. **Graceful shutdown** - SIGTERM → wait → SIGKILL escalation
13. **Crash recovery** - Resume interrupted loops on restart

## What to Reconsider for v3

1. **Runner processes** - Designed but not fully implemented in v2; may simplify to
   in-process tool execution initially
2. **TaskStore complexity** - JSONL + SQLite dual storage may be overkill for MVP;
   consider starting with just JSONL or just SQLite
3. **Three runner lanes** - Network sandboxing adds complexity; may defer to later
4. **Worktree coordination** - Rebase-on-merge protocol is complex; may simplify
   for single-loop operation first
