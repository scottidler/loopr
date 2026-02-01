# Process Model: TUI + Daemon + Runners

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

Loopr runs as three process types: a **TUI client** for user interaction, a **Daemon** for orchestration, and **Runner** processes for tool execution. This separation enables:

- TUI can detach/reattach without stopping loops
- Tool execution in sandboxed environments
- Clean process group management for killability
- Independent scaling of LLM calls vs tool execution

---

## Process Overview

| Process | Count | Lifecycle | Responsibility |
|---------|-------|-----------|----------------|
| TUI | 0-N | User session | Display, input |
| Daemon | 0-1 | Background | Orchestration |
| Runner (no-net) | 1 | Daemon child | Sandboxed tools |
| Runner (net) | 1 | Daemon child | Network tools |
| Runner (heavy) | 1 | Daemon child | Build/test tools |

---

## TUI Client

### Responsibility

- Terminal rendering (ratatui)
- Keyboard handling
- Display streaming LLM responses
- Show loop hierarchy tree
- Send user commands to daemon

### Does NOT

- Execute tools directly
- Make LLM API calls
- Manage loop state

### Connection Model

```rust
pub struct TuiClient {
    daemon_conn: UnixStream,
    event_rx: mpsc::Receiver<DaemonEvent>,
    request_id: AtomicU64,
}

impl TuiClient {
    pub async fn connect() -> Result<Self> {
        let socket_path = config::daemon_socket_path();
        let stream = UnixStream::connect(&socket_path).await?;
        // ...
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let request = DaemonRequest {
            id: self.request_id.fetch_add(1, Ordering::SeqCst),
            method: method.to_string(),
            params,
        };
        // Write request, read response
    }
}
```

### Lifecycle

```
User runs `loopr`
    │
    ▼
Try connect to daemon.sock
    │
    ├── Success → Launch TUI, attach to daemon
    │
    └── Fail → Start daemon, then connect
          │
          ▼
TUI main loop
    │
    ├── Render state
    ├── Handle keyboard
    ├── Process daemon events
    └── Send user commands
          │
          ▼
User quits (q or Ctrl+C)
    │
    ▼
Disconnect from daemon (loops continue)
```

---

## Daemon

### Responsibility

- Agent/loop lifecycle management
- Anthropic API calls (LlmClient)
- Tool routing to runners
- Event streaming to TUIs
- TaskStore persistence
- Concurrency enforcement

### Internal Architecture

```rust
pub struct Daemon {
    config: DaemonConfig,
    store: TaskStore,
    loop_manager: LoopManager,
    llm_client: Arc<dyn LlmClient>,
    tool_router: ToolRouter,
    tui_connections: Vec<TuiConnection>,
}

impl Daemon {
    pub async fn run(&mut self) -> Result<()> {
        // Initialize runners
        self.spawn_runners().await?;

        // Recover interrupted loops
        self.recover_loops().await?;

        // Accept TUI connections
        let listener = UnixListener::bind(&config::daemon_socket_path())?;

        loop {
            tokio::select! {
                // Accept new TUI connection
                conn = listener.accept() => {
                    self.handle_tui_connect(conn?).await?;
                }

                // Loop manager tick (poll for runnable loops)
                _ = self.loop_manager.tick() => {}

                // Process pending tool results from runners
                result = self.tool_router.recv_result() => {
                    self.handle_tool_result(result?).await?;
                }
            }
        }
    }
}
```

### Lifecycle

```
Daemon start (explicit or implicit)
    │
    ▼
Initialize
    ├── Open/create TaskStore
    ├── Load config
    ├── Create directories (~/.loopr/)
    │
    ▼
Spawn runners
    ├── runner-no-net (subprocess)
    ├── runner-net (subprocess)
    ├── runner-heavy (subprocess)
    │
    ▼
Wait for runner handshakes
    │
    ▼
Recover state
    ├── Find loops with status=running
    ├── Mark as interrupted or resume
    │
    ▼
Create daemon.sock
    │
    ▼
Main loop
    ├── Accept TUI connections
    ├── Run loop scheduler
    ├── Route tool calls
    ├── Stream LLM responses
    │
    ▼
Shutdown (signal or command)
    ├── Signal running loops to stop
    ├── Wait for graceful checkpoint
    ├── Terminate runners
    └── Exit
```

### Daemon Commands

| Command | Description |
|---------|-------------|
| `loopr daemon start` | Start daemon in background |
| `loopr daemon stop` | Graceful shutdown |
| `loopr daemon status` | Show running state |
| `loopr daemon restart` | Stop + start |

---

## Runners

### Responsibility

- Execute tool jobs from daemon
- Enforce sandbox constraints
- Apply timeouts and output limits
- Spawn subprocesses in process groups
- Kill process trees on cancel/timeout

### Runner Types

| Lane | Socket | Network | Concurrency | Use Case |
|------|--------|---------|-------------|----------|
| `no-net` | `runner-no-net.sock` | Blocked | 10 | File I/O, local git, grep |
| `net` | `runner-net.sock` | Allowed | 5 | Web fetch, API calls |
| `heavy` | `runner-heavy.sock` | Allowed | 1 | cargo build, npm test |

### Internal Architecture

```rust
pub struct Runner {
    lane: RunnerLane,
    config: RunnerConfig,
    daemon_conn: UnixStream,
    active_jobs: HashMap<String, JobHandle>,
}

impl Runner {
    pub async fn run(&mut self) -> Result<()> {
        // Send handshake to daemon
        self.send_handshake().await?;

        loop {
            tokio::select! {
                // Receive job from daemon
                job = self.recv_job() => {
                    self.execute_job(job?).await?;
                }

                // Check for completed jobs
                completed = self.poll_completed() => {
                    for (job_id, result) in completed {
                        self.send_result(job_id, result).await?;
                    }
                }

                // Handle cancellation requests
                cancel = self.recv_cancel() => {
                    self.cancel_job(cancel?.job_id).await?;
                }
            }
        }
    }

    async fn execute_job(&mut self, job: ToolJob) -> Result<()> {
        // Validate constraints
        self.validate_path(&job.cwd, &job.worktree_dir)?;

        // Spawn in new process group
        let child = Command::new(&job.argv[0])
            .args(&job.argv[1..])
            .current_dir(&job.cwd)
            .process_group(0)  // New process group
            .spawn()?;

        let handle = JobHandle {
            child,
            timeout: Instant::now() + Duration::from_millis(job.timeout_ms),
            max_output: job.max_output_bytes,
        };

        self.active_jobs.insert(job.job_id, handle);
        Ok(())
    }

    async fn cancel_job(&mut self, job_id: String) -> Result<()> {
        if let Some(handle) = self.active_jobs.remove(&job_id) {
            // Kill entire process group
            let pgid = handle.child.id() as i32;
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        Ok(())
    }
}
```

### Sandboxing (runner-no-net)

For `runner-no-net`, network access is blocked:

**Option 1: Network namespace (Linux)**
```rust
// Before exec, enter new network namespace
unshare(CloneFlags::CLONE_NEWNET)?;
```

**Option 2: seccomp filter**
```rust
// Block socket syscalls
let filter = SeccompFilter::new()
    .deny_syscall(Syscall::socket)
    .deny_syscall(Syscall::connect)
    .build()?;
```

**Option 3: Firewall rules (fallback)**
```bash
# iptables rules for runner-no-net process
iptables -A OUTPUT -m owner --uid-owner $RUNNER_UID -j DROP
```

### Process Group Kill

Critical for cleanup: when a tool spawns subprocesses, we must kill the entire tree.

```rust
// Spawn with new process group
Command::new(cmd)
    .process_group(0)  // setsid() equivalent
    .spawn()?;

// Kill entire group
let pgid = child.id() as i32;
unsafe {
    libc::killpg(pgid, libc::SIGTERM);
    sleep(Duration::from_secs(1));
    libc::killpg(pgid, libc::SIGKILL);
}
```

---

## Startup Scenarios

### Fresh Start

```
$ loopr
  → No daemon.sock found
  → Fork daemon process
  → Daemon spawns runners
  → Daemon creates daemon.sock
  → TUI connects to daemon
  → TUI renders empty state
```

### Reconnect

```
$ loopr  (daemon already running)
  → Connect to daemon.sock
  → Request current state
  → TUI renders existing loops
```

### Daemon Only

```
$ loopr daemon start
  → Daemon starts in background
  → No TUI
  → Loops continue running

$ loopr  (later)
  → Connect to existing daemon
  → See in-progress loops
```

---

## Failure Modes

### TUI Crash

- Daemon continues running
- Loops keep executing
- Reconnect to see state

### Daemon Crash

- Runners exit (socket closed)
- TaskStore has last checkpoint
- On restart: recover interrupted loops

### Runner Crash

- Daemon detects socket close
- Pending jobs fail with error
- Daemon restarts runner
- Retry jobs if idempotent

### Kernel OOM

- Process groups help contain damage
- TaskStore persists state
- Restart and recover

---

## Configuration

```yaml
# ~/.config/loopr/loopr.yml

daemon:
  socket_path: ~/.loopr/daemon.sock
  pid_file: ~/.loopr/daemon.pid

  # Concurrency limits
  max_concurrent_loops: 50
  llm_slots_global: 10

runners:
  no_net:
    socket_path: ~/.loopr/runner-no-net.sock
    slots: 10
    timeout_default_ms: 30000

  net:
    socket_path: ~/.loopr/runner-net.sock
    slots: 5
    timeout_default_ms: 60000

  heavy:
    socket_path: ~/.loopr/runner-heavy.sock
    slots: 1
    timeout_default_ms: 600000  # 10 minutes for builds
```

---

## References

- [architecture.md](architecture.md) - System overview
- [runners.md](runners.md) - Runner lane details
- [ipc-protocol.md](ipc-protocol.md) - Message schemas
