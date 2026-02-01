# Worktree Coordination: Rebase-on-Merge Protocol

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

When multiple CodeLoops work in parallel on separate git worktrees, conflicts are avoided through a **rebase-on-merge** protocol: whenever any CodeLoop successfully merges to main, all other active CodeLoops must stop and rebase before continuing.

This ensures:
- All worktrees are always based on the latest main
- Merges are always fast-forward (no merge commits)
- Conflicts are resolved incrementally, not accumulated

---

## Core Principle

```
        main: A ─── B ─── C ─── D (after merge)
                    │           ↑
        worktree-1: B'────────→ merge (becomes D)
                    │
        worktree-2: B''─ X (stop, rebase onto D, continue)
                    │
        worktree-3: B'''─ Y (stop, rebase onto D, continue)
```

When worktree-1 merges, worktrees 2 and 3 **must rebase onto the new main** before continuing work. This prevents divergence and ensures all integration happens against current state.

---

## Signal Type: Rebase

Add to `SignalType` enum in loop-coordination:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalType {
    Stop,       // Terminate immediately
    Pause,      // Suspend execution
    Resume,     // Continue paused loop
    Rebase,     // Stop, rebase worktree, continue (NEW)
    Error,      // Report problem upstream
    Info,       // Advisory message
}
```

---

## Rebase Signal Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebasePayload {
    /// The loop that triggered the merge
    pub merged_by: String,

    /// The new HEAD of main after the merge
    pub new_main_head: String,

    /// Commit message of the merge (for context in logs)
    pub merge_summary: String,
}
```

Signal record:

```json
{
    "id": "sig-20260131-143052-rebase",
    "signal_type": "rebase",
    "source_loop": "loop-20260131-142000-code-001",
    "target_selector": "status:running",
    "reason": "Sibling loop merged to main",
    "payload": {
        "merged_by": "loop-20260131-142000-code-001",
        "new_main_head": "abc123def456",
        "merge_summary": "feat(auth): implement JWT validation"
    },
    "created_at": 1738338652000,
    "acknowledged_at": null
}
```

---

## Merge-Then-Signal Sequence

When a CodeLoop decides to merge:

```
CodeLoop-1                    LoopManager                     CodeLoop-2, 3, ...
    │                              │                                │
    │──(1) request_merge()────────>│                                │
    │                              │                                │
    │                              │──(2) acquire_merge_lock()      │
    │                              │                                │
    │                              │──(3) send_rebase_signal()─────>│
    │                              │                                │
    │                              │<─(4) wait for all to ACK──────│
    │                              │                                │
    │                              │      [all CodeLoops stopped]   │
    │                              │                                │
    │<─(5) merge_approved()────────│                                │
    │                              │                                │
    │──(6) git merge (fast-fwd)───>│                                │
    │                              │                                │
    │──(7) merge_complete()───────>│                                │
    │                              │                                │
    │                              │──(8) release_merge_lock()      │
    │                              │                                │
    │                              │──(9) send_resume_signal()─────>│
    │                              │                                │
    │                              │                                │──(10) rebase onto new main
    │                              │                                │──(11) continue work
```

---

## CodeLoop State Machine Extension

Add `Rebasing` state:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopStatus {
    Pending,
    Running,
    Paused,
    Rebasing,     // NEW: stopped for rebase
    Complete,
    Failed,
    Invalidated,
}
```

State transitions for rebase:

```
Running ──[rebase signal]──> Rebasing
Rebasing ──[rebase complete]──> Running
Rebasing ──[rebase conflict]──> Failed (with conflict details)
```

---

## Loop Rebase Handler

The rebase handler is part of `Loop` itself. All loop types use the same self-contained `Loop` struct that can run itself.

```rust
impl Loop {
    /// Called when rebase signal is received
    async fn handle_rebase_signal(&mut self, payload: RebasePayload) -> Result<()> {
        // 1. Stop at safe point (after current tool execution)
        self.stop_at_safe_point().await?;

        // 2. Update status
        self.status = LoopStatus::Rebasing;

        // 3. Acknowledge signal
        self.acknowledge_signal().await?;

        // 4. Perform rebase
        let result = self.perform_rebase(&payload.new_main_head).await;

        match result {
            Ok(()) => {
                // 5a. Success: continue work
                self.status = LoopStatus::Running;
                self.continue_iteration().await?;
            }
            Err(RebaseError::Conflict(details)) => {
                // 5b. Conflict: escalate to parent or fail
                self.handle_rebase_conflict(details).await?;
            }
        }

        Ok(())
    }

    /// Stop execution at the next safe point
    async fn stop_at_safe_point(&mut self) -> Result<()> {
        // Safe points:
        // - Between tool executions
        // - After current tool completes
        // - After current LLM response completes

        // If tool is running, wait for it OR send cancel
        if let Some(job) = self.current_tool_job.as_ref() {
            if job.is_cancellable() {
                // Short-running tools: wait for completion
                job.wait_completion().await?;
            } else {
                // Long-running tools: cancel
                self.cancel_tool_job(job.id()).await?;
            }
        }

        Ok(())
    }

    /// Perform git rebase onto new main
    async fn perform_rebase(&self, new_main_head: &str) -> Result<(), RebaseError> {
        // Loop has its worktree path directly
        let worktree_path = &self.worktree;

        // Stash any uncommitted changes (shouldn't happen, but defensive)
        let stash_result = git_command(&["stash", "--include-untracked"], worktree_path)?;
        let had_stash = !stash_result.contains("No local changes");

        // Fetch latest main
        git_command(&["fetch", "origin", "main"], worktree_path)?;

        // Rebase onto new main
        let rebase_result = git_command(
            &["rebase", new_main_head],
            worktree_path
        );

        match rebase_result {
            Ok(_) => {
                // Restore stash if we had one
                if had_stash {
                    git_command(&["stash", "pop"], worktree_path)?;
                }
                Ok(())
            }
            Err(e) if e.to_string().contains("CONFLICT") => {
                // Abort the failed rebase
                git_command(&["rebase", "--abort"], worktree_path)?;

                // Restore stash if we had one
                if had_stash {
                    git_command(&["stash", "pop"], worktree_path)?;
                }

                Err(RebaseError::Conflict(e.to_string()))
            }
            Err(e) => Err(RebaseError::GitError(e)),
        }
    }
}
```

---

## LoopManager Merge Coordination

```rust
impl LoopManager {
    /// Global lock for merge operations (only one merge at a time)
    merge_lock: Arc<Mutex<()>>,

    /// Handle merge request from a CodeLoop
    async fn handle_merge_request(&self, loop_id: &str) -> Result<MergeResult> {
        // 1. Acquire merge lock
        let _guard = self.merge_lock.lock().await;

        // 2. Get list of other running CodeLoops
        let other_loops = self.get_running_loops_except(loop_id)?;

        if !other_loops.is_empty() {
            // 3. Send rebase signal to all others
            let signal_id = self.send_rebase_signal(loop_id, &other_loops).await?;

            // 4. Wait for all to acknowledge (with timeout)
            self.wait_for_acknowledgments(&signal_id, &other_loops,
                Duration::from_secs(60)).await?;
        }

        // 5. All stopped - approve the merge
        Ok(MergeResult::Approved)
    }

    /// Called after merge completes
    async fn on_merge_complete(&self, loop_id: &str, new_head: &str) -> Result<()> {
        // Record the merge
        self.store.append(&MergeEvent {
            merged_by: loop_id.to_string(),
            new_head: new_head.to_string(),
            timestamp: now_ms(),
        })?;

        // Send resume signal (implicitly tells them to rebase first)
        // Actually, the CodeLoops handle rebase in their signal handler
        // before acknowledging, so by the time we're here they're already
        // rebasing or done

        Ok(())
    }
}
```

---

## Conflict Handling

When a rebase has conflicts:

### Option 1: Fail the Loop (Simple)

```rust
async fn handle_rebase_conflict(&mut self, details: String) -> Result<()> {
    self.set_status(LoopStatus::Failed).await?;
    self.record.failure_reason = Some(format!(
        "Rebase conflict after sibling merge: {}", details
    ));
    self.store.update(&self.record)?;

    // Parent will be notified and may retry with fresh worktree
    Ok(())
}
```

### Option 2: Escalate to Parent (Smart)

```rust
async fn handle_rebase_conflict(&mut self, details: String) -> Result<()> {
    // Send error signal to parent
    self.send_signal(SignalRecord {
        signal_type: SignalType::Error,
        target_loop: self.record.parent_id.clone(),
        reason: format!("Rebase conflict in {}: {}", self.record.id, details),
        payload: Some(json!({
            "conflict_type": "rebase",
            "files": parse_conflict_files(&details),
        })),
        ..Default::default()
    }).await?;

    // Mark as blocked, not failed
    self.set_status(LoopStatus::Paused).await?;

    Ok(())
}
```

The parent (Phase loop) can then decide:
- Abandon this CodeLoop and create a fresh one
- Wait for the conflicting sibling to finish and retry
- Escalate further up the hierarchy

---

## Merge Lock Contention

Only one merge can happen at a time. If multiple CodeLoops want to merge simultaneously:

```rust
impl LoopManager {
    /// Queue of loops waiting to merge
    merge_queue: Arc<Mutex<VecDeque<String>>>,

    async fn request_merge(&self, loop_id: &str) -> Result<MergeTicket> {
        // Add to queue
        {
            let mut queue = self.merge_queue.lock().await;
            queue.push_back(loop_id.to_string());
        }

        // Wait for our turn
        loop {
            {
                let queue = self.merge_queue.lock().await;
                if queue.front() == Some(&loop_id.to_string()) {
                    // Our turn - proceed with merge
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // Acquire merge lock
        let guard = self.merge_lock.lock().await;

        Ok(MergeTicket { _guard: guard })
    }
}
```

---

## Timing Considerations

| Event | Expected Duration |
|-------|-------------------|
| Tool execution completion | 0-60s (depending on tool) |
| Signal propagation | <100ms |
| Acknowledgment collection | 0-60s (waiting for tools) |
| Git rebase (no conflicts) | 1-5s |
| Full merge cycle | 5-120s |

Rebase signals should have a **generous timeout** (60s) because CodeLoops may be mid-tool-execution. After timeout, the LoopManager can either:
- Force-cancel the slow loop's tool
- Fail the merge and have the requester retry

---

## Git Lock Contention

Multiple worktrees share the same `.git` directory. Git operations can conflict. Mitigation:

```rust
impl Loop {
    /// Per-repo lock for git operations
    async fn with_git_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> Result<R>,
    {
        // Derive repo path from worktree
        let repo_path = self.worktree.parent().unwrap_or(&self.worktree);
        let lock_path = repo_path.join(".git/loopr.lock");
        let _lock = FileLock::acquire(&lock_path).await?;
        f()
    }

    async fn perform_rebase_with_lock(&self, new_main_head: &str) -> Result<(), RebaseError> {
        self.with_git_lock(|| {
            // ... rebase logic ...
        }).await
    }
}
```

---

## Integration with Existing Signals

The rebase signal integrates with the existing signal infrastructure in `loop-coordination.md`:

| Signal | Behavior |
|--------|----------|
| `Stop` | Terminate immediately, don't resume |
| `Pause` | Stop at safe point, wait for `Resume` |
| `Rebase` | Stop at safe point, rebase, continue automatically |
| `Resume` | Continue paused loop |
| `Error` | Report problem upstream |

`Rebase` is like a `Pause` + automatic rebase + automatic `Resume`.

---

## TaskStore Collections

Add to `persistence.md`:

```
~/.loopr/store/
├── loops.jsonl
├── signals.jsonl
├── events.jsonl
├── tool_jobs.jsonl
└── merges.jsonl    # NEW: merge history
```

`merges.jsonl` schema:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRecord {
    pub id: String,
    pub loop_id: String,
    pub branch: String,
    pub old_main_head: String,
    pub new_main_head: String,
    pub files_changed: usize,
    pub timestamp: i64,
}
```

---

## TUI Display

When loops are rebasing, TUI should show:

```
┌─ Loops ─────────────────────────────────────────────────────┐
│ ● loop-001 [code] Running                                   │
│   ↳ Implementing JWT validation                             │
│ ⟳ loop-002 [code] Rebasing                                  │
│   ↳ Waiting for merge by loop-001                          │
│ ⟳ loop-003 [code] Rebasing                                  │
│   ↳ Rebasing onto abc123d                                   │
└─────────────────────────────────────────────────────────────┘
```

New `Rebasing` status uses `⟳` spinner indicator.

---

## References

- [loop-coordination.md](loop-coordination.md) - Signal infrastructure
- [architecture.md](architecture.md) - System overview
- [process-model.md](process-model.md) - Process lifecycle
- [persistence.md](persistence.md) - TaskStore schema
