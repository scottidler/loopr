# Design Document: Wire Up Daemon Infrastructure

**Author:** Scott A. Idler
**Date:** 2026-02-01
**Status:** Ready for Implementation
**Review Passes Completed:** 5/5

## Summary

The Loopr daemon infrastructure (scheduler, tick loop, recovery, IPC server) was built but never wired together. The TUI cannot connect to the daemon because `daemon start` is unimplemented, and socket paths are inconsistent between code and documentation. This design wires the existing components into a functional daemon with auto-start capability.

## Problem Statement

### Background

Loopr v2 uses a three-tier architecture: TUI (user interface), Daemon (orchestration), and Runners (tool execution). Per `docs/implementation-phases.md`, Phase 13 (Daemon Core) and Phase 16 (CLI Integration) define how these components should work together.

The individual components were built:
- `src/daemon/scheduler.rs` - Loop prioritization
- `src/daemon/tick.rs` - Tick loop config/state
- `src/daemon/recovery.rs` - Crash recovery
- `src/ipc/server.rs` - Unix socket server
- `src/ipc/client.rs` - Client for TUI
- `src/manager/loop_manager.rs` - Loop lifecycle

However, nothing connects them. The `Daemon` struct specified in the docs was never implemented.

### Problem

1. **Broken UX**: Running `loopr` fails with "DAEMON NOT RUNNING" and tells user to run `daemon start`
2. **Unimplemented command**: `daemon start` prints "not yet implemented" and exits
3. **Path inconsistency**: Socket paths differ between code files and don't match documentation

Current state:
```
$ loopr
FATAL: DAEMON NOT RUNNING
→ Start the daemon first: $ loopr daemon start

$ loopr daemon start
Starting daemon...
Daemon start not yet implemented
```

### Goals

- Wire existing daemon components into a functional `Daemon` struct
- Implement `daemon start`, `stop`, `status`, `restart` commands
- Auto-start daemon when TUI launches (per `docs/process-model.md` spec)
- Standardize socket/PID paths to match documentation (`~/.loopr/`)

### Non-Goals

- Full loop execution (scheduler tick loop integration) - future work
- Runner process spawning - future work
- LLM client integration - future work
- This is minimum viable daemon that accepts connections and responds to ping

## Proposed Solution

### Overview

Add the `Daemon` struct to `src/daemon/mod.rs` that coordinates IPC server, signal handling, and PID file management. Update `main.rs` to implement daemon commands and TUI auto-start. Fix all socket paths to use `~/.loopr/daemon.sock`.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                       main.rs                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐  │
│  │daemon start │  │daemon stop  │  │    run_tui()    │  │
│  │             │  │             │  │  (auto-start)   │  │
│  └──────┬──────┘  └──────┬──────┘  └────────┬────────┘  │
└─────────┼────────────────┼──────────────────┼───────────┘
          │                │                  │
          ▼                ▼                  │
┌─────────────────────────────────────────────┼───────────┐
│              src/daemon/mod.rs              │           │
│  ┌─────────────────────────────────────┐    │           │
│  │            Daemon struct            │◄───┘           │
│  │  - config: DaemonConfig             │                │
│  │  - tick_state: Arc<RwLock<...>>     │                │
│  │                                     │                │
│  │  + run() -> Result<()>              │                │
│  │  + is_running(pid_path) -> bool     │                │
│  │  + get_pid(pid_path) -> Option<i32> │                │
│  └──────────────┬──────────────────────┘                │
│                 │                                       │
│  ┌──────────────┼──────────────────────────────────┐    │
│  │              ▼                                  │    │
│  │  ┌─────────────────┐  ┌──────────────────────┐ │    │
│  │  │   IpcServer     │  │   Signal Handler     │ │    │
│  │  │ (existing)      │  │ (SIGTERM/SIGINT)     │ │    │
│  │  └─────────────────┘  └──────────────────────┘ │    │
│  │                                                │    │
│  │  ┌─────────────────┐  ┌──────────────────────┐ │    │
│  │  │   PID File      │  │   Socket File        │ │    │
│  │  │ ~/.loopr/       │  │ ~/.loopr/            │ │    │
│  │  │ daemon.pid      │  │ daemon.sock          │ │    │
│  │  └─────────────────┘  └──────────────────────┘ │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

### Data Model

**DaemonConfig**
```rust
pub struct DaemonConfig {
    pub socket_path: PathBuf,    // ~/.loopr/daemon.sock
    pub pid_path: PathBuf,       // ~/.loopr/daemon.pid
    pub data_dir: PathBuf,       // ~/.loopr/
    pub tick_config: TickConfig, // From existing tick.rs
}
```

**Path Helper Functions**
```rust
pub fn default_socket_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loopr")
        .join("daemon.sock")
}

pub fn default_pid_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loopr")
        .join("daemon.pid")
}
```

### API Design

**Daemon Struct**
```rust
impl Daemon {
    pub fn new(config: DaemonConfig) -> Result<Self>;
    pub fn is_running(pid_path: &Path) -> bool;  // Static
    pub fn get_pid(pid_path: &Path) -> Option<i32>;  // Static
    pub async fn run(&mut self) -> Result<()>;
}
```

**CLI Commands**
| Command | Behavior |
|---------|----------|
| `daemon start` | Spawn daemon in background, verify started |
| `daemon start --foreground` | Run daemon in current process |
| `daemon stop` | SIGTERM, wait 3s, SIGKILL if needed |
| `daemon status` | Check PID file, report running/stopped |
| `daemon restart` | Stop + start |

**TUI Auto-Start**
1. Try connect to socket (2s timeout)
2. On failure: spawn `daemon start --foreground` in background
3. Retry connect with exponential backoff (5 attempts)
4. On success: continue to TUI

### Implementation Plan

**Phase 1: Add libc dependency**
```bash
cargo add libc
```

**Phase 2: Standardize paths**

| File | Line | Change |
|------|------|--------|
| `src/ipc/server.rs` | 35 | `/tmp/loopr-daemon.sock` → `default_socket_path()` |
| `src/ipc/client.rs` | 34 | `/tmp/loopr.sock` → `default_socket_path()` |
| `src/tui/app.rs` | 58 | `/tmp/loopr.sock` → `default_socket_path()` |
| `src/ipc/client.rs` | 316, 331 | Update test assertions |
| `src/tui/app.rs` | 509 | Update test assertion |

Import `loopr::daemon::default_socket_path` in each file.

**Phase 3: Add Daemon to daemon/mod.rs**
- Add path helper functions (`default_socket_path()`, `default_pid_path()`, `default_data_dir()`)
- Add DaemonConfig struct
- Add Daemon struct with run(), is_running(), get_pid()
- Wire IpcServer with CallbackHandler that responds to ping

Signal handling pattern:
```rust
// In Daemon::run()
// Use tokio::select! to race server against signals
tokio::select! {
    result = server.run(handler) => {
        result?;
    }
    _ = async {
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        tokio::select! {
            _ = sigterm.recv() => {},
            _ = sigint.recv() => {},
        }
    } => {
        // Signal received - server.run() will be cancelled
        // Cleanup happens in defer (PID file removal)
    }
}
// Always remove PID file on exit
self.remove_pid();
```

Note: Socket cleanup happens automatically - IpcServer removes socket in run() on exit,
and OS cleans up on process termination.

**Phase 4: Implement daemon commands in main.rs**
- Replace stubbed handle_daemon_command()
- Add handle_daemon_start(), handle_daemon_stop(), handle_daemon_status()

**Phase 5: Add auto-start to TUI**
- Modify run_tui() to spawn daemon on connection failure
- Add retry logic with backoff

## Alternatives Considered

### Alternative 1: Use systemd/launchd for daemon management

- **Description:** Let OS service manager handle daemon lifecycle
- **Pros:** Battle-tested, automatic restart on crash, proper logging
- **Cons:** Platform-specific, requires installation step, overkill for dev tool
- **Why not chosen:** Loopr should work out-of-the-box without system configuration

### Alternative 2: Single-process mode (no daemon)

- **Description:** Run everything in TUI process, no separate daemon
- **Pros:** Simpler architecture, no IPC needed
- **Cons:** TUI disconnect kills loops, no background execution, violates existing design
- **Why not chosen:** Contradicts core architecture requirement that loops survive TUI disconnect

### Alternative 3: Create new runtime.rs file

- **Description:** Add Daemon code to a new src/daemon/runtime.rs file
- **Pros:** Separation of concerns
- **Cons:** Violates docs/implementation-phases.md which doesn't specify this file
- **Why not chosen:** Follow existing spec - Daemon belongs in mod.rs per Phase 13

## Technical Considerations

### Dependencies

**New:**
- `libc` - For process signal handling (kill, SIGTERM, SIGKILL)

**Existing (already in Cargo.toml):**
- `dirs` - Home directory resolution
- `tokio` - Async runtime, signal handling

### Performance

- Daemon startup: < 100ms (just socket bind + PID write)
- Auto-start adds ~500ms to first TUI launch (daemon spawn + connect retry)
- No performance impact once running

### Security

- PID file in user home directory (no /tmp race conditions)
- Socket file in user home directory (not world-accessible)
- No privilege escalation needed
- `unsafe` blocks limited to libc::kill calls (standard Unix pattern)

### Testing Strategy

**Unit Tests:**
- Path helper functions return correct paths
- DaemonConfig::default() uses correct paths
- is_running() with mock PID files

**Integration Tests:**
- Daemon start/stop lifecycle
- TUI auto-start when daemon not running
- Signal handling (SIGTERM graceful shutdown)

**Manual Testing:**
```bash
cargo run -q -- daemon status   # not running
cargo run -q -- daemon start    # starts
cargo run -q -- daemon status   # running (PID: xxx)
cargo run -q -- daemon stop     # stops
cargo run -q --                 # auto-starts daemon, launches TUI
```

### Rollout Plan

1. Implement all phases
2. Run `cargo test`
3. Manual verification per testing strategy
4. Commit with message referencing this design doc

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Stale PID file after crash | Medium | Low | is_running() checks process exists with kill(pid, 0) |
| Socket file not cleaned up | Medium | Low | IpcServer removes existing socket before bind |
| Race condition on double-start | Low | Low | is_running() check before spawn |
| Home directory not writable | Low | High | Fallback to current directory with warning |
| Windows compatibility | N/A | N/A | Unix-only (libc::kill, Unix sockets). Windows not supported. |
| TUI spawns daemon but can't connect | Low | Medium | 5 retry attempts with backoff; clear error message |

## Open Questions

- [x] Where should Daemon struct live? → `src/daemon/mod.rs` per Phase 13
- [x] What socket path to use? → `~/.loopr/daemon.sock` per docs
- [ ] Should daemon log to file or stderr in foreground mode? (Defer to future work)

## Files Changed Summary

| File | Type | Description |
|------|------|-------------|
| `Cargo.toml` | Modify | Add libc dependency |
| `src/daemon/mod.rs` | Modify | Add Daemon, DaemonConfig, path helpers |
| `src/main.rs` | Modify | Implement daemon commands, TUI auto-start |
| `src/ipc/server.rs` | Modify | Use default_socket_path() |
| `src/ipc/client.rs` | Modify | Use default_socket_path(), fix tests |
| `src/tui/app.rs` | Modify | Use default_socket_path(), fix test |

## References

- `docs/process-model.md` - Daemon struct specification, lifecycle
- `docs/implementation-phases.md` - Phase 13 (Daemon Core), Phase 16 (CLI)
- `docs/architecture.md` - Three-tier architecture overview
- `docs/configuration-reference.md` - Default paths
- `docs/ipc-protocol.md` - IPC message format
