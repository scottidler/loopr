# Runner Lanes

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec

---

## Summary

Runners are subprocess workers that execute tools on behalf of the daemon. Three runner lanes provide different capabilities and isolation levels: `runner-no-net` (file operations, no network), `runner-net` (general tools with network), and `runner-heavy` (builds and tests with low concurrency).

---

## Lane Overview

| Lane | Network | Default Slots | Timeout | Use Case |
|------|---------|---------------|---------|----------|
| `no-net` | Blocked | 10 | 30s | File I/O, grep, glob, local git |
| `net` | Allowed | 5 | 60s | Web fetch, API calls |
| `heavy` | Allowed | 1 | 10min | cargo build, npm test, otto ci |

---

## Tool → Lane Mapping

Defined in `tools/catalog.toml`:

```toml
[tools.read_file]
lane = "no-net"
timeout_ms = 10000
description = "Read file contents"

[tools.write_file]
lane = "no-net"
timeout_ms = 10000
description = "Write content to file"

[tools.edit_file]
lane = "no-net"
timeout_ms = 10000
description = "Replace string in file"

[tools.list_directory]
lane = "no-net"
timeout_ms = 5000
description = "List directory contents"

[tools.glob]
lane = "no-net"
timeout_ms = 30000
description = "Find files by pattern"

[tools.grep]
lane = "no-net"
timeout_ms = 60000
description = "Search file contents"

[tools.run_command]
lane = "net"              # Default lane for commands
timeout_ms = 120000
description = "Execute shell command"

[tools.run_command_no_net]
lane = "no-net"
timeout_ms = 120000
description = "Execute command without network"

[tools.web_fetch]
lane = "net"
timeout_ms = 30000
description = "Fetch URL content"

[tools.web_search]
lane = "net"
timeout_ms = 30000
description = "Web search via API"

# Heavy tools
[tools.build]
lane = "heavy"
timeout_ms = 600000       # 10 minutes
description = "Run build command"

[tools.test]
lane = "heavy"
timeout_ms = 600000
description = "Run test suite"

[tools.validate]
lane = "heavy"
timeout_ms = 600000
description = "Run validation command"
```

---

## Runner Implementation

### Core Structure

```rust
pub struct Runner {
    lane: RunnerLane,
    config: RunnerConfig,
    socket: UnixStream,
    active_jobs: HashMap<String, ActiveJob>,
    semaphore: Semaphore,
}

pub enum RunnerLane {
    NoNet,
    Net,
    Heavy,
}

struct ActiveJob {
    job_id: String,
    child: Child,
    pgid: i32,
    started_at: Instant,
    timeout: Duration,
    output_buffer: Vec<u8>,
    max_output: usize,
}
```

### Main Loop

```rust
impl Runner {
    pub async fn run(&mut self) -> Result<()> {
        // Handshake with daemon
        self.handshake().await?;

        loop {
            tokio::select! {
                // Receive new job
                msg = self.socket.read_message() => {
                    match msg? {
                        RunnerMessage::Job(job) => {
                            self.start_job(job).await?;
                        }
                        RunnerMessage::Cancel { job_id } => {
                            self.cancel_job(&job_id).await?;
                        }
                        RunnerMessage::Shutdown => {
                            self.shutdown().await?;
                            break;
                        }
                    }
                }

                // Poll active jobs
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    self.poll_jobs().await?;
                }
            }
        }

        Ok(())
    }
}
```

### Job Execution

```rust
impl Runner {
    async fn start_job(&mut self, job: ToolJob) -> Result<()> {
        // Acquire semaphore slot
        let _permit = self.semaphore.acquire().await?;

        // Validate path constraints
        self.validate_paths(&job)?;

        // Build command
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&job.command)
            .current_dir(&job.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);  // New process group

        // Apply network restrictions for no-net lane
        if self.lane == RunnerLane::NoNet {
            self.apply_network_sandbox(&mut cmd)?;
        }

        // Spawn
        let child = cmd.spawn()?;
        let pgid = child.id() as i32;

        let active_job = ActiveJob {
            job_id: job.job_id.clone(),
            child,
            pgid,
            started_at: Instant::now(),
            timeout: Duration::from_millis(job.timeout_ms),
            output_buffer: Vec::new(),
            max_output: job.max_output_bytes,
        };

        self.active_jobs.insert(job.job_id, active_job);
        Ok(())
    }

    async fn poll_jobs(&mut self) -> Result<()> {
        let mut completed = Vec::new();

        for (job_id, job) in &mut self.active_jobs {
            // Check timeout
            if job.started_at.elapsed() > job.timeout {
                self.kill_process_group(job.pgid);
                completed.push((job_id.clone(), ToolResult {
                    job_id: job_id.clone(),
                    status: ToolExitStatus::Timeout,
                    output: String::from_utf8_lossy(&job.output_buffer).to_string(),
                    exit_code: None,
                    was_timeout: true,
                    was_cancelled: false,
                }));
                continue;
            }

            // Check if completed
            match job.child.try_wait()? {
                Some(status) => {
                    // Read remaining output
                    let stdout = job.child.stdout.take();
                    let stderr = job.child.stderr.take();
                    // ... collect output ...

                    completed.push((job_id.clone(), ToolResult {
                        job_id: job_id.clone(),
                        status: if status.success() {
                            ToolExitStatus::Success
                        } else {
                            ToolExitStatus::Failed
                        },
                        output: String::from_utf8_lossy(&job.output_buffer).to_string(),
                        exit_code: status.code(),
                        was_timeout: false,
                        was_cancelled: false,
                    }));
                }
                None => {
                    // Still running, read available output
                    // ... non-blocking read ...
                }
            }
        }

        // Send results and cleanup
        for (job_id, result) in completed {
            self.active_jobs.remove(&job_id);
            self.send_result(result).await?;
        }

        Ok(())
    }

    async fn cancel_job(&mut self, job_id: &str) -> Result<()> {
        if let Some(job) = self.active_jobs.remove(job_id) {
            self.kill_process_group(job.pgid);

            self.send_result(ToolResult {
                job_id: job_id.to_string(),
                status: ToolExitStatus::Cancelled,
                output: String::from_utf8_lossy(&job.output_buffer).to_string(),
                exit_code: None,
                was_timeout: false,
                was_cancelled: true,
            }).await?;
        }
        Ok(())
    }

    fn kill_process_group(&self, pgid: i32) {
        unsafe {
            // Try graceful first
            libc::killpg(pgid, libc::SIGTERM);
        }

        // Wait briefly, then force kill
        std::thread::sleep(Duration::from_millis(500));

        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}
```

---

## Path Validation

All file operations are sandboxed to the worktree:

```rust
impl Runner {
    fn validate_paths(&self, job: &ToolJob) -> Result<()> {
        // cwd must be within worktree
        let cwd = job.cwd.canonicalize()?;
        let worktree = job.worktree_dir.canonicalize()?;

        if !cwd.starts_with(&worktree) {
            return Err(RunnerError::PathViolation {
                path: cwd,
                worktree,
            });
        }

        // Check any file paths in job arguments
        for path in &job.file_paths {
            let canonical = worktree.join(path).canonicalize()?;
            if !canonical.starts_with(&worktree) {
                return Err(RunnerError::PathViolation {
                    path: canonical,
                    worktree,
                });
            }
        }

        Ok(())
    }
}
```

---

## Network Sandboxing (runner-no-net)

### Option 1: Network Namespace (Linux, preferred)

```rust
fn apply_network_sandbox(&self, cmd: &mut Command) -> Result<()> {
    // Use unshare to create new network namespace
    cmd.pre_exec(|| {
        nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNET)?;
        Ok(())
    });
    Ok(())
}
```

### Option 2: seccomp-bpf Filter

```rust
fn apply_network_sandbox(&self, cmd: &mut Command) -> Result<()> {
    cmd.pre_exec(|| {
        use seccompiler::{SeccompAction, SeccompFilter, SeccompRule};

        let filter = SeccompFilter::new(
            vec![
                // Block network syscalls
                (libc::SYS_socket, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_connect, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_bind, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_listen, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_accept, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_sendto, vec![SeccompRule::new(vec![])?]),
                (libc::SYS_recvfrom, vec![SeccompRule::new(vec![])?]),
            ].into_iter().collect(),
            SeccompAction::Errno(libc::EPERM),
            SeccompAction::Allow,
            std::env::consts::ARCH.try_into()?,
        )?;

        filter.apply()?;
        Ok(())
    });
    Ok(())
}
```

### Option 3: firejail (external tool)

```rust
fn apply_network_sandbox(&self, cmd: &mut Command) -> Result<()> {
    // Wrap command in firejail
    let original_cmd = cmd.get_program().to_string_lossy().to_string();
    let original_args: Vec<_> = cmd.get_args()
        .map(|a| a.to_string_lossy().to_string())
        .collect();

    *cmd = Command::new("firejail");
    cmd.arg("--net=none")
        .arg("--")
        .arg(original_cmd)
        .args(original_args);

    Ok(())
}
```

---

## Output Handling

### Truncation

```rust
const MAX_OUTPUT_DEFAULT: usize = 100_000;  // 100KB

fn collect_output(&mut self, job: &mut ActiveJob) -> Result<()> {
    let remaining = job.max_output.saturating_sub(job.output_buffer.len());
    if remaining == 0 {
        return Ok(());  // Already at limit
    }

    let mut buf = vec![0u8; remaining.min(4096)];
    // Non-blocking read from stdout/stderr
    // ...

    if job.output_buffer.len() >= job.max_output {
        job.output_buffer.truncate(job.max_output);
        job.output_buffer.extend_from_slice(b"\n[output truncated]");
    }

    Ok(())
}
```

### Streaming (for long-running tools)

For heavy lane tools, output is streamed to the daemon in chunks:

```rust
struct RunnerMessage {
    // ...
    OutputChunk {
        job_id: String,
        chunk: Vec<u8>,
        is_stderr: bool,
    },
}
```

---

## Handshake Protocol

On startup, runner connects to daemon and announces capabilities:

```rust
// Runner → Daemon
struct RunnerHandshake {
    lane: RunnerLane,
    pid: u32,
    slots: usize,
    version: String,
}

// Daemon → Runner
struct RunnerAck {
    accepted: bool,
    config: RunnerConfig,
}
```

---

## Supervision

### Daemon monitors runners:

```rust
impl Daemon {
    async fn supervise_runners(&mut self) {
        loop {
            for (lane, runner) in &mut self.runners {
                // Check if runner is alive
                if !runner.is_alive().await {
                    tracing::warn!(?lane, "Runner died, restarting");

                    // Fail pending jobs
                    for job_id in runner.pending_jobs() {
                        self.fail_job(&job_id, "Runner crashed").await;
                    }

                    // Restart runner
                    *runner = self.spawn_runner(*lane).await?;
                }
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}
```

### Runner heartbeats:

```rust
// Runner sends periodic heartbeat
struct RunnerHeartbeat {
    lane: RunnerLane,
    active_jobs: usize,
    uptime_secs: u64,
}
```

---

## Configuration

```yaml
# ~/.config/loopr/loopr.yml

runners:
  no_net:
    slots: 10
    timeout_default_ms: 30000
    max_output_bytes: 100000
    sandbox_method: "namespace"  # namespace, seccomp, firejail

  net:
    slots: 5
    timeout_default_ms: 60000
    max_output_bytes: 100000

  heavy:
    slots: 1
    timeout_default_ms: 600000
    max_output_bytes: 1000000   # 1MB for build output
```

---

## Error Handling

| Scenario | Behavior |
|----------|----------|
| Tool timeout | Kill process group, return partial output |
| Tool crash | Return exit code and captured output |
| Path violation | Reject job before execution |
| Network violation (no-net) | syscall returns EPERM |
| Runner crash | Daemon restarts runner, fails pending jobs |
| Output overflow | Truncate with marker |

---

## References

- [process-model.md](process-model.md) - Process lifecycle
- [ipc-protocol.md](ipc-protocol.md) - Message schemas
- [tools.md](tools.md) - Tool implementations
- [tool-catalog.md](tool-catalog.md) - Tool definitions
