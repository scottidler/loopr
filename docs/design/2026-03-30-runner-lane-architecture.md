# Design Document: Runner Lane Architecture

**Author:** Scott A. Idler
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Replace Loopr's in-process tool execution with a spawn-policy-based lane system. Each tool call is classified into one of three lanes (local, net, heavy) that control sandboxing, network access, concurrency limits, and timeouts. Lanes are not separate processes - they are spawn configurations enforced by the daemon using `bwrap` wrapping, `setsid()` process groups, `killpg()` escalation, and `tokio::sync::Semaphore` slot limiting. This is the same architecture Claude Code uses, adapted for Rust.

## Problem Statement

### Background

Loopr's "Light Loops, Heavy Tools" principle (documented in `docs/v2-light-loops-heavy-tools.md`) separates agent LLM loops (cheap tokio tasks) from tool execution (real compute needing isolation). The tool system is implemented (`src/tools/`) with 14 built-in tools and configured project tools, all executing via the `Tool` trait and `ToolExecutor` dispatch. The first autonomous E2E run completed successfully on 2026-03-30.

### Problem

All tools currently execute **in-process** via `tokio::process::Command` directly from the daemon. This creates three concrete problems:

1. **No process group management.** `shell.rs` spawns with `kill_on_drop(true)` and sends SIGTERM to a single PID on timeout. A `cargo build` that spawns rustc, linker, and proc-macro subprocesses will orphan them all. The daemon cannot kill the process tree.

2. **No network isolation.** The `read` and `glob` tools don't need network access, but nothing prevents a command injected via the `shell` tool from phoning home. There is no enforcement boundary between tools that should be offline and tools that need the network.

3. **No concurrency limiting.** Nothing prevents 10 simultaneous `cargo build` invocations from starving the system. The daemon has no slot-based admission control for resource-intensive tools.

### Goals

- Classify every tool into a lane (local, net, heavy) with appropriate isolation guarantees
- Spawn all shell-based tools with `setsid()` for process group isolation
- Implement SIGTERM -> 5s grace -> SIGKILL escalation via `killpg()` on the process group
- Sandbox the local lane with `bwrap --unshare-net` to block network access
- Enforce slot-based concurrency per lane via `tokio::sync::Semaphore`
- Support `run_in_background` for heavy tools so agents can poll instead of block
- Support `persisted_output_path` for large outputs instead of truncating
- Zero changes to the `Tool` trait or `ToolExecutor` dispatch interface - the lane system is below them

### Non-Goals

- macOS seatbelt sandboxing (Linux-only for now, macOS falls back to unsandboxed)
- Filesystem sandboxing beyond the existing `ToolContext.validate_path()` (bwrap filesystem isolation is future work)
- MCP server integration
- Replacing the `ToolRunner` (configured project tool executor) - it gets the same spawn upgrades
- User-facing permission model (hooks, allowlists) - that's a separate concern
- Worktree lifecycle management - already exists, orthogonal to this work

## Proposed Solution

### Overview

Introduce a `Lane` enum and `LanePolicy` struct that sit between the `ToolExecutor` and the OS `spawn()` call. When the executor dispatches a tool, the tool's lane determines how the subprocess is spawned: what bwrap arguments to prefix, how many concurrent slots are available, what timeout applies, and whether the process runs in its own process group.

The key insight from Claude Code's architecture: you don't need separate runner processes with Unix socket IPC. You get identical isolation by wrapping each individual spawn with the right OS primitives. This eliminates an entire layer of complexity.

### Architecture

```
                    Existing (unchanged)
                    ┌──────────────────────────┐
                    │  AgenticLoop / Chat       │
                    │         │                 │
                    │         ▼                 │
                    │  ToolExecutor.execute()   │
                    │         │                 │
                    └─────────┼────────────────┘
                              │
                    New       ▼
                    ┌──────────────────────────┐
                    │  LaneRouter              │
                    │  ┌─────────────────────┐ │
                    │  │ classify(tool_name)  │ │
                    │  │   → Lane::Local     │ │
                    │  │   → Lane::Net       │ │
                    │  │   → Lane::Heavy     │ │
                    │  └────────┬────────────┘ │
                    │           ▼              │
                    │  acquire_slot(lane)      │
                    │  (Semaphore::acquire)    │
                    │           ▼              │
                    │  spawn_in_lane(cmd,lane) │
                    │  - setsid()             │
                    │  - bwrap wrapping       │
                    │  - timeout + killpg()   │
                    │           ▼              │
                    │  SpawnResult             │
                    │  { stdout, stderr,       │
                    │    exit_code, duration,  │
                    │    persisted_output? }   │
                    └──────────────────────────┘
```

### Data Model

#### Lane Classification

The v2 design doc called these `no-net`, `net`, and `heavy`. We rename `no-net` to `Local` because "local" describes what the lane *is* (filesystem-local operations), not just what it lacks (network). The enum variants are `Local`, `Net`, `Heavy`.

```rust
// src/tools/lane.rs

/// The three execution lanes for tool subprocess isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// No network access. Sandboxed via bwrap --unshare-net.
    /// For filesystem-only tools: read, write, edit, glob, grep, find, list, tree.
    Local,
    /// Network access allowed. No sandbox wrapping.
    /// For tools that need HTTP: fetch, search, shell (when command needs net).
    Net,
    /// Resource-intensive. Network allowed. Slot-limited to 1.
    /// For builds, tests, linting: configured tools (cargo build, npm test, otto ci).
    Heavy,
}

/// Lane configuration — slots, timeouts, sandbox settings.
#[derive(Debug, Clone)]
pub struct LanePolicy {
    pub lane: Lane,
    pub max_slots: usize,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub sandbox_net: bool,      // bwrap --unshare-net
}

impl LanePolicy {
    pub fn local() -> Self {
        Self {
            lane: Lane::Local,
            max_slots: 10,
            default_timeout_secs: 30,
            max_timeout_secs: 60,
            sandbox_net: true,
        }
    }

    pub fn net() -> Self {
        Self {
            lane: Lane::Net,
            max_slots: 5,
            default_timeout_secs: 60,
            max_timeout_secs: 120,
            sandbox_net: false,
        }
    }

    pub fn heavy() -> Self {
        Self {
            lane: Lane::Heavy,
            max_slots: 1,
            default_timeout_secs: 600,
            max_timeout_secs: 1800,
            sandbox_net: false,
        }
    }
}
```

#### Tool-to-Lane Mapping

```rust
/// Classify a tool into its execution lane.
pub fn classify(tool_name: &str) -> Lane {
    match tool_name {
        // Filesystem-only builtins — no network needed
        "read" | "write" | "edit" | "list" | "tree" | "glob" | "grep" | "find" => Lane::Local,

        // Network-required builtins
        "fetch" | "search" => Lane::Net,

        // In-process tools — no subprocess spawned, lane is irrelevant
        // (classification exists for completeness; these never hit the LaneRouter)
        "todo" | "plan" | "slash" | "delegate" => Lane::Local,

        // Shell tool — classified by caller or defaults to Net (conservative)
        "shell" => Lane::Net,

        // Configured project tools (test, build, lint) — always Heavy
        _ => Lane::Heavy,
    }
}
```

#### Lane Router

```rust
// src/tools/router.rs

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Manages lane semaphores and dispatches tool execution.
pub struct LaneRouter {
    policies: HashMap<Lane, LanePolicy>,
    semaphores: HashMap<Lane, Arc<Semaphore>>,
    bwrap_available: bool,
}

impl LaneRouter {
    pub fn new() -> Self {
        let policies = HashMap::from([
            (Lane::Local, LanePolicy::local()),
            (Lane::Net, LanePolicy::net()),
            (Lane::Heavy, LanePolicy::heavy()),
        ]);
        let semaphores = policies
            .iter()
            .map(|(lane, policy)| (*lane, Arc::new(Semaphore::new(policy.max_slots))))
            .collect();
        let bwrap_available = detect_bwrap();
        Self { policies, semaphores, bwrap_available }
    }

    /// Execute a shell command in the appropriate lane.
    pub async fn spawn(
        &self,
        command: &str,
        working_dir: &Path,
        lane: Lane,
        timeout_secs: Option<u64>,
    ) -> Result<SpawnResult> {
        let policy = &self.policies[&lane];
        let timeout = timeout_secs.unwrap_or(policy.default_timeout_secs)
            .min(policy.max_timeout_secs);

        // 1. Acquire slot (blocks until available)
        let permit = self.semaphores[&lane]
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| eyre!("lane {:?} semaphore closed", lane))?;

        // 2. Build command - bwrap wrapping or plain shell
        let cmd = if policy.sandbox_net && self.bwrap_available {
            bwrap_command(command, working_dir)
        } else {
            shell_command(command, working_dir)
        };

        // 3. Spawn with setsid() for process group isolation
        let result = spawn_with_process_group(cmd, timeout).await;

        // 4. Slot released on drop
        drop(permit);

        result
    }
}
```

### API Design

#### Process Group Spawn

The core spawn function replaces the current `execute_shell_command()`. It accepts a pre-built `Command` so callers (including the bwrap wrapper) can configure the command before spawn.

```rust
// src/tools/spawn.rs

use std::os::unix::process::CommandExt;

/// Build a standard (non-sandboxed) shell command.
pub fn shell_command(command: &str, working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.current_dir(working_dir);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

/// Spawn a command in its own process group with timeout and kill escalation.
/// Accepts a pre-configured Command (from shell_command() or bwrap_command()).
pub async fn spawn_with_process_group(
    mut cmd: tokio::process::Command,
    timeout_secs: u64,
) -> Result<SpawnResult> {
    // Create new process group (equivalent to Node.js detached: true)
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let start = Instant::now();
    let child = cmd.spawn()?;
    let pgid = child.id().expect("child has PID") as i32;

    let timeout_dur = Duration::from_secs(timeout_secs);

    match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            // Normal completion
            Ok(SpawnResult::from_output(output, start.elapsed()))
        }
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            // Timeout: SIGTERM the process group
            unsafe { libc::killpg(pgid, libc::SIGTERM); }

            // Wait 5s for graceful shutdown, then SIGKILL.
            // The process may already be gone (ESRCH) - that's fine, ignore errors.
            tokio::time::sleep(Duration::from_secs(5)).await;
            unsafe { libc::killpg(pgid, libc::SIGKILL); }

            // Note on zombie cleanup: child.wait_with_output() was consumed by the
            // timeout, which drops the inner future, which drops the Child. Tokio's
            // Child drop registers the process with a global background reaper thread
            // that quietly handles SIGCHLD. No zombies.

            Ok(SpawnResult {
                stdout: String::new(),
                stderr: format!("timed out after {}s (SIGTERM -> SIGKILL)", timeout_secs),
                exit_code: -1,
                duration_ms: start.elapsed().as_millis() as u64,
                timed_out: true,
                persisted_output_path: None,
            })
        }
    }
}
```

#### bwrap Wrapping

Instead of returning a command string (which requires shell escaping and creates a double-shell `sh -c "bwrap ... -- sh -c 'cmd'"` problem), the sandbox module builds and returns a `tokio::process::Command` directly. This is immune to shell-injection, eliminates the `shell-escape` dependency, and avoids double-shell execution.

The host filesystem is bound read-only in its entirety (`--ro-bind / /`), with the worktree and `/tmp` punched through as read-write. This guarantees the Local lane can find all installed tools (rg, eza, fd) regardless of where they're installed (`~/.cargo/bin`, `/home/linuxbrew/`, etc.) while still enforcing `--unshare-net` and preventing writes outside the worktree.

```rust
// src/tools/sandbox.rs

/// Detect if bubblewrap is available on this system.
pub fn detect_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a bwrap-wrapped Command that blocks network access.
/// Binds the entire host filesystem read-only, with worktree + /tmp read-write.
pub fn bwrap_command(command: &str, working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("bwrap");
    cmd.arg("--unshare-net")
       .arg("--ro-bind").arg("/").arg("/")
       .arg("--dev").arg("/dev")
       .arg("--proc").arg("/proc")
       .arg("--bind").arg("/tmp").arg("/tmp")
       .arg("--bind").arg(working_dir).arg(working_dir)
       .arg("--chdir").arg(working_dir)
       .arg("--")
       .arg("sh").arg("-c").arg(command);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}
```

#### Persisted Output for Large Results

```rust
/// Result of spawning a tool subprocess.
#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// If output exceeded MAX_OUTPUT, the full output is written here.
    pub persisted_output_path: Option<PathBuf>,
}

const MAX_INLINE_OUTPUT: usize = 32_000;

impl SpawnResult {
    fn from_output(output: std::process::Output, duration: Duration) -> Self {
        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut persisted_path = None;

        // If output is too large, persist to disk and truncate inline
        if stdout.len() > MAX_INLINE_OUTPUT {
            let path = persist_output(&stdout);
            persisted_path = path.ok();
            stdout.truncate(MAX_INLINE_OUTPUT);
            if let Some(ref p) = persisted_path {
                stdout.push_str(&format!(
                    "\n... [truncated, full output at {}]", p.display()
                ));
            } else {
                stdout.push_str("\n... [truncated]");
            }
        }

        Self {
            stdout,
            stderr,
            exit_code: output.status.code().unwrap_or(-1),
            duration_ms: duration.as_millis() as u64,
            timed_out: false,
            persisted_output_path: persisted_path,
        }
    }
}
```

#### Background Execution for Heavy Tools

```rust
/// Handle for a backgrounded tool execution.
pub struct BackgroundTask {
    pub task_id: String,
    pub output_path: PathBuf,
    handle: tokio::task::JoinHandle<SpawnResult>,
}

impl LaneRouter {
    /// Start a heavy tool in the background, returning immediately with a task ID.
    /// The agent can poll or use the output_path to check progress.
    pub async fn spawn_background(
        &self,
        command: &str,
        working_dir: &Path,
        lane: Lane,
    ) -> Result<BackgroundTask> {
        let task_id = format!("bg-{}", uuid::Uuid::new_v4().as_simple());
        let output_path = PathBuf::from(format!("/tmp/loopr-bg-{}.log", task_id));

        let cmd = command.to_string();
        let dir = working_dir.to_path_buf();
        let semaphores = self.semaphores.clone();
        let policies = self.policies.clone();
        let bwrap = self.bwrap_available;
        let out_path = output_path.clone();

        let handle = tokio::spawn(async move {
            // Acquire slot inside the background task
            let policy = &policies[&lane];
            let permit = semaphores[&lane].clone().acquire_owned().await.ok();

            let wrapped = if policy.sandbox_net && bwrap {
                wrap_with_bwrap(&cmd, &dir)
            } else {
                cmd
            };

            let result = spawn_with_process_group(
                &wrapped, &dir, policy.default_timeout_secs
            ).await.unwrap_or_else(|e| SpawnResult {
                stdout: String::new(),
                stderr: e.to_string(),
                exit_code: -1,
                duration_ms: 0,
                timed_out: false,
                persisted_output_path: None,
            });

            // Write full output to disk for agent to read
            let _ = std::fs::write(&out_path, format!(
                "exit_code: {}\n---stdout---\n{}\n---stderr---\n{}",
                result.exit_code, result.stdout, result.stderr
            ));

            drop(permit);
            result
        });

        Ok(BackgroundTask { task_id, output_path, handle })
    }
}
```

### Integration Points

#### Where LaneRouter Plugs In

The `LaneRouter` sits inside `execute_shell_command()` and `ConfiguredTool::execute()`. The `Tool` trait is unchanged - tools still call `execute_shell_command()` or similar, but that function now routes through the lane system.

```rust
// Updated src/tools/shell.rs

/// Execute a shell command through the lane system.
/// This is the single entry point for all subprocess-spawning tools.
pub async fn execute_shell_command(
    command: &str,
    working_dir: &Path,
    timeout_secs: u64,
    router: &LaneRouter,
    lane: Lane,
) -> Result<ShellOutput> {
    let result = router.spawn(command, working_dir, lane, Some(timeout_secs)).await?;
    Ok(ShellOutput {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        truncated: result.persisted_output_path.is_some(),
        timed_out: result.timed_out,
    })
}

/// Backward-compatible wrapper for tools not yet migrated to lanes.
/// Uses Lane::Net (conservative default) and creates a temporary LaneRouter.
pub async fn execute_shell_command_legacy(
    command: &str,
    working_dir: &Path,
    timeout_secs: u64,
) -> Result<ShellOutput> {
    let cmd = shell_command(command, working_dir);
    let result = spawn_with_process_group(cmd, timeout_secs).await?;
    Ok(ShellOutput {
        stdout: result.stdout,
        stderr: result.stderr,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        truncated: result.persisted_output_path.is_some(),
        timed_out: result.timed_out,
    })
}
```

The `_legacy` variant exists for Phase 1 - it gives every subprocess the setsid() + killpg() upgrade without requiring the full lane system. It's removed in Phase 2 when all callers migrate to the lane-aware version.

#### ToolContext Gets Lane

The `ToolContext` gains a reference to the `LaneRouter` so individual tools can spawn subprocesses in the correct lane:

```rust
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub exec_id: String,
    read_files: Arc<Mutex<HashSet<PathBuf>>>,
    pub sandbox_enabled: bool,
    deny_patterns: Vec<String>,
    pub router: Arc<LaneRouter>,  // NEW
}
```

#### Built-in Tools That Spawn Subprocesses

Only a subset of builtins actually spawn subprocesses. The rest are pure Rust (tokio::fs, glob crate). The subprocess-spawning tools and their lanes:

| Tool | Backend | Lane | Spawns Subprocess? |
|------|---------|------|--------------------|
| read | tokio::fs | Local | No |
| write | tokio::fs | Local | No |
| edit | tokio::fs | Local | No |
| glob | glob crate | Local | No |
| list | eza | Local | Yes |
| tree | eza --tree | Local | Yes |
| grep | rg (ripgrep) | Local | Yes |
| find | fd | Local | Yes |
| shell | sh -c | Net | Yes |
| fetch | reqwest | Net | No (async HTTP) |
| search | web API | Net | No (async HTTP) |
| slash | in-process | - | No |
| todo | in-process | - | No |
| plan | in-process | - | No |
| delegate | in-process | - | No |
| (configured) | sh -c | Heavy | Yes |

**Key distinction:** The lane system only applies to tools that spawn OS subprocesses. Pure Rust tools (read, write, edit, glob, fetch, search, todo, plan, slash, delegate) call tokio::fs or async HTTP directly - they never touch the `LaneRouter`. The `LaneRouter` is only invoked by tools whose `execute()` method calls `execute_shell_command()` or equivalent: shell, grep, find, list, tree, and all configured tools.

This means the `Tool` trait is unchanged. Individual tool implementations decide whether to use the router. The router is a service available via `ToolContext`, not a mandatory middleware.

### Implementation Plan

#### Phase 1: Process Group Spawn (Critical Path)

Replace `execute_shell_command()` in `src/tools/shell.rs` with `spawn_with_process_group()`.

**Files created:**
- `src/tools/spawn.rs` - `spawn_with_process_group()`, `SpawnResult`

**Files modified:**
- `src/tools/shell.rs` - delegate to `spawn_with_process_group()`
- `src/tools/mod.rs` - `ToolRunner::run()` uses new spawn

**Outcome:** All subprocess tools get setsid() + killpg() + SIGTERM->SIGKILL escalation. No behavioral changes visible to agents - just safer process cleanup.

#### Phase 2: Lane Classification + Semaphores

Introduce `Lane`, `LanePolicy`, `LaneRouter` with semaphore-based slot limiting.

**Files created:**
- `src/tools/lane.rs` - `Lane`, `LanePolicy`, `classify()`
- `src/tools/router.rs` - `LaneRouter`

**Files modified:**
- `src/tools/context.rs` - add `router: Arc<LaneRouter>`
- `src/tools/shell.rs` - accept `Lane` parameter
- `src/tools/configured.rs` - route through `LaneRouter`
- `src/tools/builtin/shell.rs` - route through `LaneRouter`
- `src/tools/builtin/grep.rs` - route through `LaneRouter` (spawns rg)
- `src/tools/builtin/find.rs` - route through `LaneRouter` (spawns fd)
- `src/tools/builtin/list.rs` - route through `LaneRouter` (spawns eza)
- `src/tools/builtin/tree.rs` - route through `LaneRouter` (spawns eza)

**Outcome:** Concurrent builds limited to 1 (Heavy lane). Local tools limited to 10. Net tools limited to 5.

#### Phase 3: bwrap Network Sandboxing

Add `bwrap --unshare-net` wrapping for the Local lane.

**Files created:**
- `src/tools/sandbox.rs` - `detect_bwrap()`, `wrap_with_bwrap()`

**Files modified:**
- `src/tools/router.rs` - call `wrap_with_bwrap()` for Local lane

**Outcome:** Local lane tools (grep, find, list, tree) cannot make network requests.

#### Phase 4: Persisted Output + Background Execution

Add `persisted_output_path` for large outputs and `spawn_background()` for Heavy lane.

**Files modified:**
- `src/tools/spawn.rs` - add `persist_output()`, `BackgroundTask`
- `src/tools/router.rs` - add `spawn_background()`
- `src/tools/builtin/shell.rs` - support `run_in_background` parameter

**Outcome:** Agents can fire off `cargo build` and continue working. Build logs are available on disk for grep.

## Alternatives Considered

### Alternative 1: Three Persistent Runner Subprocesses with Unix Socket IPC

- **Description:** The original v2 design. Three long-lived OS processes (runner-no-net, runner-net, runner-heavy) that receive tool calls over Unix sockets and spawn the actual tool.
- **Pros:** Maximum isolation - a misbehaving tool can't corrupt the runner's memory. Clean separation of concerns.
- **Cons:** Complex IPC protocol. Runner process lifecycle management (crash recovery, restart). Two layers of process management (runner + tool). Significantly more code.
- **Why not chosen:** Claude Code proves the simpler model works at scale. The isolation benefit is minimal because tool subprocesses are already in their own process group via setsid() - a crash in cargo build cannot affect the daemon regardless. The complexity cost is not justified.

### Alternative 2: Container-Based Isolation (Docker/Podman)

- **Description:** Run each tool in a lightweight container for full filesystem + network isolation.
- **Pros:** Strongest isolation. Reproducible environments.
- **Cons:** Container startup latency (200-500ms per tool call). Requires Docker/Podman installed. Overkill for read/write/grep operations. Complex volume mounting for worktrees.
- **Why not chosen:** Latency is unacceptable for the tight agentic loop (dozens of tool calls per iteration). bwrap provides the network isolation we need at near-zero overhead.

### Alternative 3: seccomp Filters in Rust

- **Description:** Write custom seccomp-bpf filters in Rust to block network syscalls (socket, connect, sendto) for the Local lane.
- **Pros:** No external dependency (no bwrap required). Fine-grained control.
- **Cons:** Complex and error-prone to write correctly. Architecture-specific (x86_64 vs aarch64 syscall numbers differ). Requires unsafe Rust. Hard to test.
- **Why not chosen:** bwrap already does this correctly and is battle-tested across millions of Flatpak installs.

## Technical Considerations

### Dependencies

**New Rust dependencies:**
- `libc` - already in Cargo.toml (for setsid(), killpg())
- `uuid` - already in Cargo.toml (for background task IDs)
- No new crate dependencies required

**New runtime dependencies:**
- `bubblewrap (bwrap)` - optional, for Local lane network sandboxing. Graceful fallback: if bwrap is not installed, Local lane runs unsandboxed with a warning at daemon startup.

### Performance

- **setsid() overhead:** Negligible (single syscall at spawn)
- **bwrap overhead:** ~1-2ms per invocation (measured on Linux 6.x). Acceptable for tools that take 10ms+ anyway.
- **Semaphore contention:** Only blocks when all slots are occupied. The Heavy lane (1 slot) means builds are serialized - this is intentional to prevent OOM.
- **Background tasks:** Zero overhead on the agentic loop. The agent continues iterating while the build runs in a tokio::spawn.

### Security

- **Network isolation:** bwrap `--unshare-net` creates a new network namespace with no interfaces. Verified: even `curl localhost` fails.
- **Process group kill:** `killpg()` sends signal to every process in the group. No orphans possible unless a child calls `setsid()` itself (extremely rare for build tools).
- **Path validation:** Unchanged - `ToolContext.validate_path()` still enforces sandbox boundaries. bwrap adds an additional OS-level enforcement layer.
- **Graceful degradation:** If bwrap is unavailable, the system still works - just without network isolation for the Local lane. This is logged at startup.

### Testing Strategy

- **Unit tests:** `spawn_with_process_group()` tested with `sleep` + timeout, verifying process group kill cleans up all children
- **Lane classification tests:** Verify every builtin maps to the correct lane
- **Semaphore tests:** Verify Heavy lane serializes concurrent requests
- **bwrap tests:** Verify network is unreachable inside Local lane (ping 8.8.8.8 fails)
- **Integration tests:** Full agentic loop with a tool that spawns children, verify cleanup
- **Background task tests:** Spawn background, poll for completion, verify output file exists
- **Existing tests:** All current `ToolRunner`, `ToolExecutor`, and builtin tool tests must continue passing

### Rollout Plan

1. **Phase 1** (process group spawn) - zero behavioral changes, strictly safer cleanup. Deploy immediately.
2. **Phase 2** (lanes + semaphores) - adds admission control. May slow down parallel builds (intentionally). Deploy after Phase 1 is stable.
3. **Phase 3** (bwrap sandboxing) - adds network isolation. Optional runtime dependency. Deploy independently.
4. **Phase 4** (background + persisted output) - new agent capabilities. Deploy after Phase 2.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| setsid() breaks terminal handling for interactive tools | Low | Medium | Only agents use lane system; TUI chat shell can bypass setsid() |
| bwrap not available on target system | Medium | Low | Graceful fallback to unsandboxed. Log warning at startup. |
| Heavy lane serialization causes agent starvation | Medium | Medium | Configurable slot count. Default 1 is conservative; can increase. |
| killpg(SIGKILL) leaves zombie processes | Low | Low | Parent reaps via wait(). tokio handles SIGCHLD. |
| Background task output file fills disk | Low | Medium | Enforce max size per background output. Cleanup on task completion. |
| bwrap wrapping breaks tool command (quoting, paths) | Medium | Medium | Extensive tests with real commands. `tokio::process::Command` argument passing guarantees no shell injection. |

## Edge Cases

### bwrap + Toolchain Paths (Resolved)

Earlier drafts piecemeal-bound `/usr`, `/bin`, `/lib`, etc., which would break tools installed in non-standard locations (`~/.cargo/bin`, `/home/linuxbrew/`, virtualenvs). The current design uses `--ro-bind / /` to bind the entire host filesystem read-only, then punches through `/tmp` and the worktree as read-write. This guarantees all installed tools are available regardless of install location, while still enforcing `--unshare-net` and write restrictions.

### Nested setsid() (Grandchild Isolation)

If a child process itself calls `setsid()`, it creates a new process group that `killpg()` on the parent's PGID won't reach. This is extremely rare for build tools (cargo, npm, pytest don't do this) but theoretically possible with custom scripts. Mitigation: the 5-second SIGKILL escalation + `kill_on_drop` provides a fallback. For complete cleanup, a cgroup-based kill could be added later (future work, not needed now).

### Heavy Lane Queue Visibility

With the Heavy lane limited to 1 slot, concurrent build requests queue on the semaphore. The agent has no visibility into queue depth. Consider exposing `LaneRouter::queue_depth(lane)` so the agent system prompt can include "N builds queued" as context, preventing the LLM from spawning more builds when the queue is already deep.

### Background Task Cleanup

Background task output files (`/tmp/loopr-bg-*.log`) accumulate if not cleaned up. The `BackgroundTask` should register a cleanup callback that removes the output file after the agent reads it or after a configurable TTL (default: 1 hour). The daemon should also sweep stale background output files on startup.

## Open Questions

- [ ] Should the `shell` builtin default to Net or Local? Net is safer (conservative), Local would be faster for most commands.
- [ ] Should lane slot counts be configurable in `loopr.yml` or hardcoded?
- [ ] Should background tasks have a maximum concurrent limit across all lanes?
- [ ] How should the agent prompt/system message expose background task polling to the LLM?
- [x] ~~Should bwrap bind `$HOME/.cargo` and `$HOME/.rustup` automatically?~~ Resolved: `--ro-bind / /` binds everything read-only.

## References

- `docs/v2-light-loops-heavy-tools.md` - architectural vision (light loops vs heavy tools)
- `docs/design/2026-03-04-native-tool-use.md` - current tool system (Tool trait, ToolExecutor)
- `docs/design/remaining-gaps.md` - SIGTERM->SIGKILL gap (#10), session timeout gap (#11)
- `docs/research/2026-03-30-claude-code-tool-architecture.md` - Claude Code deep dive
- `docs/next-steps.md` item #2 - roadmap entry
- bubblewrap: https://github.com/containers/bubblewrap
