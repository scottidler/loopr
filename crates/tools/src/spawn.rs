use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::ChildStderr;
use tokio::process::ChildStdout;
use tracing::{debug, instrument};
use uuid::Uuid;

/// Maximum inline output size per captured stream (stdout/stderr/combined) in
/// bytes. ~8K tokens. Anything over this gets persisted to disk at the caller's
/// `persist_base` and the inline copy truncated.
pub const MAX_INLINE_OUTPUT: usize = 32_000;

/// Grace period between SIGTERM and SIGKILL during timeout escalation
/// (used only by `KillStrategy::Pgid`).
const KILL_GRACE_SECS: u64 = 5;

/// Bound on the post-exit pipe drain. A child that backgrounds a process
/// holding the stdout/stderr write end (`some-server &`) keeps the pipe open
/// after the foreground child exits, so an unbounded drain await hangs the
/// spawn future forever (Phase-5 finding 7). On expiry we SIGKILL the process
/// group to close the pipes, then briefly wait for the readers to hit EOF.
const DRAIN_TIMEOUT_SECS: u64 = 5;
/// Grace after a drain-timeout group-kill for the readers to observe EOF.
const DRAIN_KILL_GRACE_SECS: u64 = 1;

#[derive(Debug, Clone, Copy)]
pub enum KillStrategy {
    /// Non-bwrap spawn: `setsid()` put the child at the head of its own
    /// process group. Timeout escalation: `killpg(SIGTERM)` -> 5s -> `killpg(SIGKILL)`.
    Pgid,
    /// bwrap-wrapped spawn: bwrap owns a PID namespace; inner processes can
    /// `setsid()` and escape the host-visible process group. The authoritative
    /// move is to SIGKILL bwrap's outer PID directly; the kernel tears down
    /// the PID namespace and cascades the kill to every descendant.
    BwrapChild,
}

#[derive(Debug, Clone)]
pub struct SpawnResult {
    pub stdout: String,
    pub stderr: String,
    pub combined_output: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    /// Set when any stream exceeded `MAX_INLINE_OUTPUT`; contains the full
    /// `combined_output` written to disk.
    pub persisted_output_path: Option<PathBuf>,
    pub truncated: bool,
}

/// Injected by callers to control where overflow output gets persisted.
#[derive(Default)]
pub struct PersistConfig<'a> {
    /// Agent provides `Some(.loopr/runs/<session-id>/work/<work-id>/)`. Unit tests
    /// leave `None`, in which case spawn falls back to
    /// `std::env::temp_dir().join("loopr-tool-output/")`.
    pub base: Option<&'a Path>,
    /// Per-invocation id. When `Some`, the overflow file is named
    /// `<invocation_id>.log`. When `None`, a timestamp-based fallback is
    /// synthesized.
    pub invocation_id: Option<Uuid>,
}

/// Spawn a command in its own process group, capture stdout / stderr /
/// combined_output in arrival order, enforce a timeout.
///
/// # Kill strategy
///
/// Per D16: callers building a bwrap-wrapped `Command` MUST pass
/// `KillStrategy::BwrapChild`. Plain shell spawns pass `KillStrategy::Pgid`.
/// The two strategies are not interchangeable: `killpg` on the bwrap outer
/// process's pgid does not reliably cascade into the PID namespace bwrap owns;
/// children inside the sandbox can `setsid()` and escape. SIGKILL on bwrap's
/// outer PID is the only move the kernel honors unconditionally.
#[instrument(
    name = "spawn.process_group",
    level = "debug",
    skip_all,
    fields(
        timeout_secs = timeout_secs,
        kill_strategy = ?kill_strategy,
        invocation_id = ?persist.invocation_id,
    ),
    err,
)]
pub async fn spawn_with_process_group(
    mut cmd: tokio::process::Command,
    timeout_secs: u64,
    kill_strategy: KillStrategy,
    persist: PersistConfig<'_>,
) -> Result<SpawnResult, std::io::Error> {
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    // Cancellation-safe reaping (panel must-fix #5): set `kill_on_drop` so a
    // dropped `Child` (the future being aborted mid-tool-call, e.g. an
    // operator `work.override` abort) SIGKILLs the DIRECT child instead of
    // orphaning it. `kill_on_drop` alone only reaps the direct child; the
    // process-group reaper below closes the gap for grandchildren the child
    // forked under its own `setsid()` group. Contrast `integrator/validation.rs`,
    // which already set this on its own spawn path — the tools path did not,
    // so a task abort mid-build left the whole subprocess tree running.
    cmd.kill_on_drop(true);

    let start = Instant::now();
    let mut child = cmd.spawn()?;
    let child_pid = child
        .id()
        .ok_or_else(|| std::io::Error::other("spawned child reported no PID"))? as i32;

    // Armed drop-guard: if this future is dropped before it reaches its
    // normal return (task abort / cancellation), tear down the ENTIRE
    // process group so no grandchild survives. `kill_on_drop` on `child`
    // handles the direct child; this handles everything `setsid()` grouped
    // under it. Declared AFTER `child` so it drops BEFORE `child` (locals
    // drop in reverse declaration order): the group SIGKILL lands first,
    // then `child`'s `kill_on_drop` reaps the (already-dead) leader zombie.
    // Disarmed on the normal completion path once the child has been waited
    // and the pipes drained.
    let mut reaper = ProcessGroupReaper::new(child_pid, kill_strategy);

    debug!(
        child_pid,
        timeout_secs,
        invocation_id = ?persist.invocation_id,
        ?kill_strategy,
        "spawn: process started"
    );

    let stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("stdout not piped"))?;
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("stderr not piped"))?;

    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));
    let combined_buf = Arc::new(Mutex::new(String::new()));

    let stdout_task = spawn_stdout_reader(stdout_pipe, stdout_buf.clone(), combined_buf.clone());
    let stderr_task = spawn_stderr_reader(stderr_pipe, stderr_buf.clone(), combined_buf.clone());

    let timeout_dur = Duration::from_secs(timeout_secs);

    let timed_out;
    let exit_code;

    tokio::select! {
        biased;
        wait_result = child.wait() => {
            timed_out = false;
            exit_code = wait_result?.code().unwrap_or(-1);
        }
        _ = tokio::time::sleep(timeout_dur) => {
            timed_out = true;
            exit_code = -1;
            debug!(child_pid, timeout_secs, "command timed out, killing");
            apply_kill(&mut child, child_pid, kill_strategy).await;
            // Ensure the zombie is reaped; ignore the status (we already have -1).
            let _ = child.wait().await;
        }
    }

    // Drain both reader tasks now that the foreground child has exited.
    // Bound the drain (finding 7): a backgrounded grandchild that inherited
    // the pipe write end keeps it open, so an unbounded await would hang.
    // On timeout, kill the whole process group to close the pipes, then give
    // the readers a brief grace to observe EOF.
    let drain = async move {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    };
    tokio::pin!(drain);
    if tokio::time::timeout(Duration::from_secs(DRAIN_TIMEOUT_SECS), &mut drain)
        .await
        .is_err()
    {
        debug!(child_pid, "output drain stalled; killing process group to close pipes");
        force_kill_group(child_pid, kill_strategy);
        let _ = child.wait().await;
        let _ = tokio::time::timeout(Duration::from_secs(DRAIN_KILL_GRACE_SECS), drain).await;
    }

    let mut stdout_out = take_arc_string(stdout_buf);
    let mut stderr_out = take_arc_string(stderr_buf);
    let combined_out_full = take_arc_string(combined_buf);

    if timed_out {
        let marker = format!("timed out after {}s (killed)\n", timeout_secs);
        stderr_out.push_str(&marker);
    }

    let mut combined_out_inline = combined_out_full.clone();
    let overflow = stdout_out.len() > MAX_INLINE_OUTPUT
        || stderr_out.len() > MAX_INLINE_OUTPUT
        || combined_out_inline.len() > MAX_INLINE_OUTPUT;

    let mut persisted_path = None;
    if overflow {
        match persist_combined(&combined_out_full, &persist) {
            Ok(path) => {
                persisted_path = Some(path);
            }
            Err(e) => {
                debug!(error = %e, "failed to persist overflow output");
            }
        }
        truncate_inline(&mut stdout_out, persisted_path.as_deref());
        truncate_inline(&mut stderr_out, persisted_path.as_deref());
        truncate_inline(&mut combined_out_inline, persisted_path.as_deref());
    }

    // Normal completion: the child has exited and its pipes are drained
    // (or the timeout path already killed the group). Disarm the reaper so
    // scope exit does not send a redundant SIGKILL to an already-dead group.
    reaper.disarm();

    Ok(SpawnResult {
        stdout: stdout_out,
        stderr: stderr_out,
        combined_output: combined_out_inline,
        exit_code,
        duration_ms: start.elapsed().as_millis() as u64,
        timed_out,
        persisted_output_path: persisted_path,
        truncated: overflow,
    })
}

/// Drop-guard that reaps the spawned child's entire process group when the
/// spawn future is dropped before completing (task abort / cancellation).
///
/// Why this exists (panel must-fix #5): `kill_on_drop(true)` on the tokio
/// `Command` only kills the DIRECT child. A tool like `cargo build` or
/// `bash -c 'sleep 30 & ...'` forks grandchildren that survive the direct
/// kill. Because the child was `setsid()`'d into its own process group
/// (pgid == child_pid), `killpg(child_pid, SIGKILL)` reaches every
/// descendant in one call — the same authoritative move `force_kill_group`
/// uses on the drain-timeout path. Without this, an aborted `work.override`
/// would leave an orphaned build running against the worktree.
struct ProcessGroupReaper {
    child_pid: i32,
    strategy: KillStrategy,
    armed: bool,
}

impl ProcessGroupReaper {
    fn new(child_pid: i32, strategy: KillStrategy) -> Self {
        Self {
            child_pid,
            strategy,
            armed: true,
        }
    }

    /// Called on the normal completion path: the child has already exited,
    /// so no group teardown is needed on scope exit.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupReaper {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Only reachable on an abort/cancel (or an early `?` return before
        // disarm). For `Pgid`, SIGKILL the whole group so grandchildren die
        // with the leader. For `BwrapChild`, the `Child`'s own
        // `kill_on_drop` SIGKILLs bwrap's outer PID and its
        // `--die-with-parent` PID namespace cascades the kill to every
        // descendant — `killpg` on bwrap's pgid does NOT reliably cross the
        // namespace boundary, so there is nothing reliable to target here.
        match self.strategy {
            KillStrategy::Pgid => {
                debug!(
                    child_pid = self.child_pid,
                    "spawn future dropped; reaping process group"
                );
                force_kill_group(self.child_pid, self.strategy);
            }
            KillStrategy::BwrapChild => {
                debug!(
                    child_pid = self.child_pid,
                    "spawn future dropped; bwrap kill_on_drop + die-with-parent reaps namespace"
                );
            }
        }
    }
}

fn spawn_stdout_reader(
    pipe: ChildStdout,
    target: Arc<Mutex<String>>,
    combined: Arc<Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(read_pipe_to_buffers(pipe, target, combined))
}

fn spawn_stderr_reader(
    pipe: ChildStderr,
    target: Arc<Mutex<String>>,
    combined: Arc<Mutex<String>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(read_pipe_to_buffers(pipe, target, combined))
}

/// Read a pipe chunk-by-chunk at byte granularity, lossy-convert each chunk,
/// and append to both the per-stream and combined buffers under the shared
/// mutexes. Using `read_until(b'\n', ...)` (byte-level) instead of `lines()`
/// (UTF-8-bounded) is D15's authoritative fix: the `lines()` implementation
/// returns `io::Error(InvalidData)` the moment a non-UTF-8 byte appears,
/// terminating the reader mid-stream and losing everything after. Byte-level
/// read + `from_utf8_lossy` preserves the full output and replaces invalid
/// sequences with U+FFFD.
async fn read_pipe_to_buffers<R: AsyncRead + Unpin + Send>(
    pipe: R,
    target: Arc<Mutex<String>>,
    combined: Arc<Mutex<String>>,
) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buf);
        append_chunk(&target, &combined, &chunk);
    }
}

fn append_chunk(target: &Arc<Mutex<String>>, combined: &Arc<Mutex<String>>, chunk: &str) {
    if let Ok(mut t) = target.lock() {
        t.push_str(chunk);
    }
    if let Ok(mut c) = combined.lock() {
        c.push_str(chunk);
    }
}

fn take_arc_string(arc: Arc<Mutex<String>>) -> String {
    match Arc::try_unwrap(arc) {
        Ok(m) => m.into_inner().unwrap_or_default(),
        Err(a) => a.lock().ok().map(|s| s.clone()).unwrap_or_default(),
    }
}

async fn apply_kill(child: &mut tokio::process::Child, child_pid: i32, strategy: KillStrategy) {
    match strategy {
        KillStrategy::Pgid => {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child_pid, libc::SIGTERM);
            }
            tokio::time::sleep(Duration::from_secs(KILL_GRACE_SECS)).await;
            #[cfg(unix)]
            unsafe {
                libc::killpg(child_pid, libc::SIGKILL);
            }
        }
        KillStrategy::BwrapChild => {
            let _ = child.kill().await;
        }
    }
}

/// Immediate, hard kill used by the drain-timeout path to close pipes a
/// backgrounded grandchild is still holding. For `Pgid`, `killpg(SIGKILL)`
/// reaches the whole group (the group leader's pgid persists while members
/// live, even after the leader exited). For `BwrapChild`, bwrap's PID
/// namespace + `--die-with-parent` already tear down descendants on exit, so
/// there is nothing reliable left to target.
fn force_kill_group(child_pid: i32, strategy: KillStrategy) {
    match strategy {
        KillStrategy::Pgid => {
            #[cfg(unix)]
            unsafe {
                libc::killpg(child_pid, libc::SIGKILL);
            }
        }
        KillStrategy::BwrapChild => {}
    }
}

fn truncate_inline(s: &mut String, persist_path: Option<&Path>) {
    if s.len() <= MAX_INLINE_OUTPUT {
        return;
    }
    // `String::truncate` panics when the cut index is not a UTF-8 char
    // boundary. `MAX_INLINE_OUTPUT` is a round number, so a subprocess
    // emitting >32 KB of multibyte output (cargo/test spew with unicode)
    // would otherwise panic the spawn future. Floor to the nearest char
    // boundary at or below the cap before truncating.
    let mut cut = MAX_INLINE_OUTPUT;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    if let Some(pos) = s.rfind('\n') {
        s.truncate(pos);
    }
    match persist_path {
        Some(p) => s.push_str(&format!("\n... [truncated, full output at {}]", p.display())),
        None => s.push_str("\n... [truncated]"),
    }
}

fn persist_combined(content: &str, persist: &PersistConfig<'_>) -> Result<PathBuf, std::io::Error> {
    let (dir, filename) = match (persist.base, persist.invocation_id) {
        (Some(base), Some(id)) => (base.to_path_buf(), format!("{id}.log")),
        (Some(base), None) => (base.to_path_buf(), synthesize_filename()),
        (None, id) => {
            let d = std::env::temp_dir().join("loopr-tool-output");
            let f = id.map(|u| format!("{u}.log")).unwrap_or_else(synthesize_filename);
            (d, f)
        }
    };
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(filename);
    std::fs::write(&path, content)?;
    Ok(path)
}

fn synthesize_filename() -> String {
    format!(
        "output-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    )
}

#[cfg(test)]
mod tests;
