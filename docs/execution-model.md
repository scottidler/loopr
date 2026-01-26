# Execution Model: Git Worktrees and Crash Recovery

**Author:** Scott A. Idler
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

Each loop executes in its own git worktree on a feature branch, enabling parallel work without file conflicts. Coordination happens through TaskStore polling, not IPC. When loops complete or crash, worktrees are cleaned up. State persists in TaskStore, enabling full recovery after daemon restart.

---

## Git Worktree Management

### Why Worktrees?

- **Parallel execution**: 50 loops work simultaneously without file conflicts
- **Isolation**: Each loop modifies only its worktree
- **Clean state**: Fresh checkout from main, no leftover changes
- **Easy cleanup**: Remove worktree directory when done
- **Git history**: Each loop's work on its own branch

### Worktree Creation

When a loop transitions from `pending` to `running`:

```rust
async fn create_worktree(loop_record: &LoopRecord) -> Result<PathBuf> {
    let worktree_dir = config.worktree_dir; // e.g., /tmp/loopr/worktrees
    let worktree_path = worktree_dir.join(&loop_record.id);
    let branch_name = format!("loop-{}", loop_record.id);

    // Create git worktree
    let status = Command::new("git")
        .args([
            "worktree", "add",
            worktree_path.to_str().unwrap(),
            "-b", &branch_name,
            "main"
        ])
        .current_dir(&repo_root)
        .status()
        .await?;

    if !status.success() {
        return Err(eyre!("Failed to create worktree for {}", loop_record.id));
    }

    tracing::info!(
        loop_id = %loop_record.id,
        path = ?worktree_path,
        branch = %branch_name,
        "Created worktree"
    );

    Ok(worktree_path)
}
```

**Naming conventions:**
- Worktree path: `{worktree_dir}/{loop_id}`
- Branch name: `loop-{loop_id}`
- Example: `/tmp/loopr/worktrees/1737802800` on branch `loop-1737802800`

### Worktree Cleanup

Worktrees are cleaned up when loops reach terminal states:

```rust
async fn cleanup_worktree(loop_id: &str) -> Result<()> {
    let worktree_path = config.worktree_dir.join(loop_id);
    let branch_name = format!("loop-{}", loop_id);

    // Remove worktree
    let status = Command::new("git")
        .args(["worktree", "remove", worktree_path.to_str().unwrap(), "--force"])
        .current_dir(&repo_root)
        .status()
        .await?;

    if !status.success() {
        tracing::warn!(loop_id, "Failed to remove worktree, will retry later");
        // Don't fail - background cleanup will retry
        return Ok(());
    }

    // Delete the branch
    Command::new("git")
        .args(["branch", "-D", &branch_name])
        .current_dir(&repo_root)
        .status()
        .await?;

    tracing::info!(loop_id, "Cleaned up worktree and branch");
    Ok(())
}
```

**Cleanup triggers:**
- `complete` → merge to main (if configured), cleanup
- `failed` → cleanup (branch preserved for debugging if configured)
- `invalidated` → move to archive, cleanup
- `stopped` → cleanup

### Background Cleanup Task

Catches orphaned worktrees missed by normal cleanup:

```rust
/// Runs periodically (e.g., every 5 minutes)
async fn cleanup_orphaned_worktrees(store: &TaskStore) -> Result<()> {
    let worktrees_dir = &config.worktree_dir;
    if !worktrees_dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(worktrees_dir)? {
        let entry = entry?;
        let loop_id = entry.file_name().to_string_lossy().to_string();

        // Query TaskStore for this loop
        let record: Option<LoopRecord> = store.get(&loop_id)?;

        match record {
            None => {
                // No record exists - orphaned worktree
                tracing::warn!(loop_id, "Found orphaned worktree, cleaning up");
                cleanup_worktree(&loop_id).await?;
            }
            Some(r) if r.status.is_terminal() => {
                // Loop finished but worktree remains
                tracing::info!(loop_id, "Cleaning up stale worktree");
                cleanup_worktree(&loop_id).await?;
            }
            Some(_) => {
                // Still running, leave it
            }
        }
    }

    Ok(())
}

impl LoopStatus {
    fn is_terminal(&self) -> bool {
        matches!(self,
            LoopStatus::Complete |
            LoopStatus::Failed |
            LoopStatus::Invalidated
        )
    }
}
```

---

## Loop Manager Polling

The loop manager coordinates execution through TaskStore polling (not IPC):

```rust
pub struct LoopManager {
    store: TaskStore,
    config: LoopManagerConfig,
    running_loops: HashMap<String, JoinHandle<()>>,
}

impl LoopManager {
    /// Main polling loop
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // 1. Find pending loops ready to run
            let pending = self.store.query::<LoopRecord>(&[
                Filter::eq("status", "pending"),
            ])?;

            // 2. Check capacity
            let running_count = self.running_loops.len();
            let slots = self.config.max_concurrent_loops.saturating_sub(running_count);

            // 3. Spawn new loops (up to available slots)
            for record in pending.into_iter().take(slots) {
                self.spawn_loop(record).await?;
            }

            // 4. Check for completed loops, clean up handles
            self.reap_completed().await?;

            // 5. Check for orphaned worktrees
            if self.should_run_cleanup() {
                cleanup_orphaned_worktrees(&self.store).await?;
            }

            // 6. Sleep until next poll
            tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs)).await;
        }
    }

    async fn spawn_loop(&mut self, record: LoopRecord) -> Result<()> {
        let loop_id = record.id.clone();

        // Create worktree
        let worktree = create_worktree(&record).await?;

        // Update status to running
        let mut updated = record.clone();
        updated.status = LoopStatus::Running;
        updated.updated_at = now_ms();
        self.store.update(&updated)?;

        // Spawn the loop execution task
        let store = self.store.clone();
        let config = self.config.clone();
        let handle = tokio::spawn(async move {
            let result = run_loop_to_completion(record, worktree, store, config).await;
            if let Err(e) = result {
                tracing::error!(loop_id, error = %e, "Loop failed");
            }
        });

        self.running_loops.insert(loop_id, handle);
        Ok(())
    }

    async fn reap_completed(&mut self) {
        let mut completed = Vec::new();

        for (loop_id, handle) in &self.running_loops {
            if handle.is_finished() {
                completed.push(loop_id.clone());
            }
        }

        for loop_id in completed {
            if let Some(handle) = self.running_loops.remove(&loop_id) {
                let _ = handle.await; // Collect the result
            }
        }
    }
}
```

---

## Signal Handling via TaskStore

Loops check for signals by polling TaskStore (per [loop-coordination.md](loop-coordination.md)):

```rust
async fn run_loop_to_completion(
    mut record: LoopRecord,
    worktree: PathBuf,
    store: TaskStore,
    config: LoopConfig,
) -> Result<()> {
    let loop_impl = load_loop(record.clone())?;

    loop {
        // Check for signals before each iteration
        if let Some(signal) = check_for_signals(&record.id, &store).await? {
            match signal.signal_type {
                SignalType::Stop => {
                    record.status = LoopStatus::Invalidated;
                    store.update(&record)?;
                    cleanup_worktree(&record.id).await?;
                    return Ok(());
                }
                SignalType::Pause => {
                    record.status = LoopStatus::Paused;
                    store.update(&record)?;
                    wait_for_resume(&record.id, &store).await?;
                    record.status = LoopStatus::Running;
                    store.update(&record)?;
                }
                _ => {}
            }
            // Acknowledge the signal
            acknowledge_signal(&signal.id, &store).await?;
        }

        // Run one iteration
        let result = run_iteration(&mut loop_impl, &worktree, &config).await?;

        // Update record
        record.iteration = loop_impl.iteration();
        record.progress = loop_impl.progress().to_string();
        record.updated_at = now_ms();
        store.update(&record)?;

        // Check result
        match loop_impl.handle_validation(result) {
            LoopAction::Continue => continue,
            LoopAction::Complete => {
                record.status = LoopStatus::Complete;
                store.update(&record)?;

                // Spawn children if this loop produces artifacts
                spawn_child_loops(&record, &loop_impl, &store).await?;

                // Cleanup
                cleanup_worktree(&record.id).await?;
                return Ok(());
            }
            LoopAction::Fail(reason) => {
                record.status = LoopStatus::Failed;
                store.update(&record)?;

                // Signal parent about failure
                send_error_signal(&record, &reason, &store).await?;

                cleanup_worktree(&record.id).await?;
                return Err(eyre!("Loop failed: {}", reason));
            }
            LoopAction::SpawnChildren(children) => {
                for child in children {
                    store.create(&child)?;
                }
            }
        }
    }
}

async fn check_for_signals(loop_id: &str, store: &TaskStore) -> Result<Option<SignalRecord>> {
    let signals = store.query::<SignalRecord>(&[
        Filter::eq("target_loop", loop_id),
        Filter::is_null("acknowledged_at"),
    ])?;

    Ok(signals.into_iter().next())
}
```

---

## Crash Recovery

When Loopr restarts, it recovers incomplete loops:

```rust
async fn recover_loops(manager: &mut LoopManager, store: &TaskStore) -> Result<()> {
    tracing::info!("Recovering incomplete loops...");

    // Find all loops that were running when daemon crashed
    let running = store.query::<LoopRecord>(&[
        Filter::eq("status", "running"),
    ])?;

    tracing::info!(count = running.len(), "Found incomplete loops");

    for record in running {
        let worktree_path = config.worktree_dir.join(&record.id);

        // Verify worktree still exists
        if !worktree_path.exists() {
            tracing::warn!(
                loop_id = %record.id,
                "Worktree missing, marking as failed"
            );
            let mut failed = record.clone();
            failed.status = LoopStatus::Failed;
            failed.updated_at = now_ms();
            store.update(&failed)?;
            continue;
        }

        // Verify git repo is usable
        if !is_git_repo_clean(&worktree_path).await? {
            tracing::warn!(
                loop_id = %record.id,
                "Worktree has uncommitted changes, auto-committing"
            );
            auto_commit(&worktree_path, "WIP: auto-commit before recovery").await?;
        }

        // Re-add to running loops (will be picked up by main loop)
        // Status is already "running", so it won't be spawned again
        // Just need to spawn the execution task
        manager.spawn_loop(record).await?;
    }

    Ok(())
}

async fn is_git_repo_clean(worktree: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree)
        .output()
        .await?;

    Ok(output.stdout.is_empty())
}

async fn auto_commit(worktree: &Path, message: &str) -> Result<()> {
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(worktree)
        .status()
        .await?;

    Command::new("git")
        .args(["commit", "-m", message, "--allow-empty"])
        .current_dir(worktree)
        .status()
        .await?;

    Ok(())
}
```

**Recovery guarantees:**
- All uncommitted work in worktree is preserved (auto-commit before resume)
- Iteration count continues from last persisted value
- Fresh context window on resume (Ralph pattern)
- Missing worktrees marked as failed, not resumed

---

## Disk Space Management

### Monitoring

```rust
async fn check_disk_space() -> Result<u64> {
    let output = Command::new("df")
        .args(["-BG", &config.worktree_dir.to_string_lossy()])
        .output()
        .await?;

    let available_gb = parse_df_output(&output.stdout)?;

    if available_gb < 10 {
        tracing::warn!(available_gb, "Low disk space");
    }

    Ok(available_gb)
}
```

### Quota Enforcement

```rust
/// Called before creating a new worktree
async fn ensure_disk_space() -> Result<()> {
    let available = check_disk_space().await?;

    if available < config.disk_quota_min_gb {
        // Trigger aggressive cleanup
        tracing::warn!("Low disk space, running aggressive cleanup");
        cleanup_all_terminal_worktrees().await?;

        let available = check_disk_space().await?;
        if available < config.disk_quota_min_gb {
            return Err(eyre!(
                "Insufficient disk space: {}GB available, {}GB required",
                available,
                config.disk_quota_min_gb
            ));
        }
    }

    Ok(())
}
```

---

## Merge to Main

When a loop completes successfully and merge is configured:

```rust
async fn merge_to_main(loop_id: &str) -> Result<()> {
    let branch_name = format!("loop-{}", loop_id);

    // Fetch latest main
    Command::new("git")
        .args(["fetch", "origin", "main"])
        .current_dir(&repo_root)
        .status()
        .await?;

    // Checkout main
    Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_root)
        .status()
        .await?;

    // Pull to ensure up-to-date
    Command::new("git")
        .args(["pull", "origin", "main"])
        .current_dir(&repo_root)
        .status()
        .await?;

    // Merge the loop branch
    let merge_result = Command::new("git")
        .args([
            "merge", "--no-ff",
            &branch_name,
            "-m", &format!("Merge loop {}", loop_id)
        ])
        .current_dir(&repo_root)
        .status()
        .await?;

    if !merge_result.success() {
        return Err(eyre!("Failed to merge {} to main", branch_name));
    }

    tracing::info!(loop_id, "Merged to main");
    Ok(())
}
```

**Note:** Merge conflicts are rare because each loop works in isolation. If a conflict occurs, the merge fails and the loop's work remains on its branch for manual resolution.

---

## Configuration

```yaml
# loopr.yml
execution:
  worktree_dir: /tmp/loopr/worktrees
  max_concurrent_loops: 50
  poll_interval_secs: 1
  disk_quota_min_gb: 5
  auto_merge_to_main: false  # Require manual merge by default
  preserve_failed_branches: true  # Keep branches for debugging
```

---

## Performance Characteristics

| Metric | Expected Value |
|--------|----------------|
| Worktree creation | < 1s |
| Worktree cleanup | < 500ms |
| Disk per worktree | 50-100MB (depends on repo) |
| Max concurrent worktrees | 50 (configurable) |
| Poll interval | 1s (configurable) |

---

## Edge Cases

### Worktree Creation Fails

```rust
match create_worktree(&record).await {
    Ok(path) => path,
    Err(e) if e.to_string().contains("No space left") => {
        // Disk full - run cleanup and retry once
        cleanup_all_terminal_worktrees().await?;
        create_worktree(&record).await?
    }
    Err(e) => {
        // Mark loop as failed
        let mut failed = record.clone();
        failed.status = LoopStatus::Failed;
        store.update(&failed)?;
        return Err(e);
    }
}
```

### Cleanup Fails

```rust
// Non-fatal - log and continue, background task will retry
if let Err(e) = cleanup_worktree(loop_id).await {
    tracing::warn!(loop_id, error = %e, "Cleanup failed, will retry later");
}
```

### Worktree Corruption

```rust
async fn validate_worktree(path: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["status"])
        .current_dir(path)
        .status()
        .await?;

    if !status.success() {
        return Err(eyre!("Worktree at {:?} is corrupted", path));
    }

    Ok(())
}
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and storage
- [loop-coordination.md](loop-coordination.md) - Signal-based coordination via TaskStore
- [domain-types.md](domain-types.md) - LoopRecord and LoopStatus
- [loop-config.md](loop-config.md) - Execution configuration
