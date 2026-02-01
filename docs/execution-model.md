# Execution Model: Worktrees and Recovery

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/execution-model.md (adapted for daemon substrate)

---

## Summary

Each loop executes in its own git worktree on a feature branch. The daemon's LoopManager creates worktrees, runs loops as tokio tasks, and routes tool calls to runners. On crash, state persists in TaskStore for recovery.

---

## Why Worktrees?

- **Parallel execution**: 50 loops work simultaneously without file conflicts
- **Isolation**: Each loop modifies only its worktree
- **Clean state**: Fresh checkout from main, no leftover changes
- **Git history**: Each loop's work on its own branch
- **Easy cleanup**: Remove worktree directory when done

---

## Worktree Lifecycle

### Creation (pending → running)

```rust
impl LoopManager {
    async fn create_worktree(&self, record: &Loop) -> Result<PathBuf> {
        let worktree_dir = self.config.worktree_base.join(&record.id);
        let branch_name = format!("loop-{}", record.id);

        let status = Command::new("git")
            .args([
                "worktree", "add",
                worktree_dir.to_str().unwrap(),
                "-b", &branch_name,
                "main"
            ])
            .current_dir(&self.repo_root)
            .status()
            .await?;

        if !status.success() {
            return Err(eyre!("Failed to create worktree"));
        }

        tracing::info!(
            loop_id = %record.id,
            path = ?worktree_dir,
            branch = %branch_name,
            "Created worktree"
        );

        Ok(worktree_dir)
    }
}
```

### Tool Execution (in worktree via runners)

```rust
impl LoopManager {
    async fn execute_tool(
        &self,
        loop_id: &str,
        worktree: &Path,
        tool_call: ToolCall,
    ) -> Result<ToolResult> {
        // Build job
        let lane = self.tool_catalog.get_lane(&tool_call.name)?;
        let job = ToolJob {
            job_id: generate_job_id(),
            agent_id: loop_id.to_string(),
            tool_name: tool_call.name.clone(),
            command: self.tool_catalog.build_command(&tool_call)?,
            cwd: worktree.to_path_buf(),
            worktree_dir: worktree.to_path_buf(),
            timeout_ms: self.tool_catalog.get_timeout(&tool_call.name),
            max_output_bytes: 100_000,
            ..Default::default()
        };

        // Route to runner
        self.tool_router.submit(lane, job).await
    }
}
```

### Cleanup (complete/failed/invalidated)

```rust
impl LoopManager {
    async fn cleanup_worktree(&self, loop_id: &str) -> Result<()> {
        let worktree_path = self.config.worktree_base.join(loop_id);
        let branch_name = format!("loop-{}", loop_id);

        // Remove worktree
        Command::new("git")
            .args(["worktree", "remove", worktree_path.to_str().unwrap(), "--force"])
            .current_dir(&self.repo_root)
            .status()
            .await?;

        // Delete branch (unless preserving for debugging)
        if !self.config.preserve_failed_branches {
            Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(&self.repo_root)
                .status()
                .await?;
        }

        tracing::info!(loop_id, "Cleaned up worktree");
        Ok(())
    }
}
```

---

## Loop Execution Flow

```rust
impl LoopManager {
    async fn run_loop(&self, mut record: Loop) -> Result<()> {
        // 1. Create worktree
        let worktree = self.create_worktree(&record).await?;

        // 2. Update status
        record.status = LoopStatus::Running;
        record.updated_at = now_ms();
        self.store.update(&record)?;
        self.notify_tuis(DaemonEvent::LoopUpdated(record.clone()));

        // 3. Load loop implementation
        let mut loop_impl = load_loop(record.clone())?;

        // 4. Iteration loop
        let result = loop {
            // Check for signals
            if let Some(signal) = self.check_signals(&record.id).await? {
                match signal.signal_type {
                    SignalType::Stop => {
                        self.acknowledge_signal(&signal.id).await?;
                        break LoopOutcome::Invalidated;
                    }
                    SignalType::Pause => {
                        record.status = LoopStatus::Paused;
                        self.store.update(&record)?;
                        self.wait_for_resume(&record.id).await?;
                        record.status = LoopStatus::Running;
                        self.store.update(&record)?;
                    }
                    _ => {}
                }
            }

            // Build prompt
            let prompt = loop_impl.build_prompt(&self.config)?;

            // Save prompt to iteration directory
            self.save_iteration_prompt(&record, &prompt).await?;

            // Call LLM
            let response = self.llm_client
                .chat(&prompt, &loop_impl.tools())
                .await?;

            // Execute tool calls
            for tool_call in response.tool_calls {
                let result = self.execute_tool(&record.id, &worktree, tool_call).await?;
                // ... continue conversation with result
            }

            // Run validation
            let validation = self.run_validation(&record, &worktree).await?;

            // Update loop state
            record.iteration += 1;
            record.updated_at = now_ms();

            // Handle result
            match loop_impl.handle_validation(validation) {
                LoopAction::Continue => {
                    self.store.update(&record)?;
                    continue;
                }
                LoopAction::Complete => break LoopOutcome::Complete,
                LoopAction::Fail(reason) => break LoopOutcome::Failed(reason),
            }
        };

        // 5. Finalize
        match result {
            LoopOutcome::Complete => {
                record.status = LoopStatus::Complete;
                self.store.update(&record)?;

                // Spawn children from artifacts
                self.spawn_children(&record, &loop_impl).await?;

                // Optionally merge to main
                if self.config.auto_merge {
                    self.merge_to_main(&record.id).await?;
                }
            }
            LoopOutcome::Failed(reason) => {
                record.status = LoopStatus::Failed;
                record.progress.push_str(&format!("\nFailed: {}", reason));
                self.store.update(&record)?;
            }
            LoopOutcome::Invalidated => {
                record.status = LoopStatus::Invalidated;
                self.store.update(&record)?;

                // Archive loop directory
                self.archive_loop(&record.id).await?;
            }
        }

        // 6. Cleanup worktree
        self.cleanup_worktree(&record.id).await?;

        // 7. Notify TUIs
        self.notify_tuis(DaemonEvent::LoopUpdated(record));

        Ok(())
    }
}

enum LoopOutcome {
    Complete,
    Failed(String),
    Invalidated,
}
```

---

## Crash Recovery

### On Daemon Start

```rust
impl Daemon {
    async fn recover_loops(&mut self) -> Result<()> {
        // Find loops that were running
        let interrupted = self.store.query::<Loop>(&[
            Filter::eq("status", "running"),
        ])?;

        for record in interrupted {
            let worktree = self.config.worktree_base.join(&record.id);

            if worktree.exists() {
                // Worktree exists - can resume
                tracing::info!(loop_id = %record.id, "Recovering loop");

                // Auto-commit any uncommitted work
                if !self.is_git_clean(&worktree).await? {
                    self.auto_commit(&worktree, "WIP: auto-commit before recovery").await?;
                }

                // Mark as pending for scheduler
                let mut updated = record.clone();
                updated.status = LoopStatus::Pending;
                self.store.update(&updated)?;
            } else {
                // Worktree missing - mark failed
                tracing::warn!(loop_id = %record.id, "Worktree missing, marking failed");

                let mut updated = record.clone();
                updated.status = LoopStatus::Failed;
                updated.progress.push_str("\nFailed: worktree lost in crash");
                self.store.update(&updated)?;
            }
        }

        Ok(())
    }

    async fn is_git_clean(&self, worktree: &Path) -> Result<bool> {
        let output = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree)
            .output()
            .await?;
        Ok(output.stdout.is_empty())
    }

    async fn auto_commit(&self, worktree: &Path, message: &str) -> Result<()> {
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
}
```

### Recovery Guarantees

- Uncommitted work in worktree is preserved (auto-commit before resume)
- Iteration count continues from last persisted value
- Fresh context window on resume (Ralph pattern)
- Missing worktrees marked as failed

---

## Background Cleanup

Catches orphaned worktrees:

```rust
impl LoopManager {
    async fn cleanup_orphaned_worktrees(&self) -> Result<()> {
        let worktree_base = &self.config.worktree_base;
        if !worktree_base.exists() {
            return Ok(());
        }

        for entry in std::fs::read_dir(worktree_base)? {
            let entry = entry?;
            let loop_id = entry.file_name().to_string_lossy().to_string();

            let record = self.store.get::<Loop>(&loop_id)?;

            match record {
                None => {
                    // No record - orphaned
                    tracing::warn!(loop_id, "Found orphaned worktree, cleaning");
                    self.cleanup_worktree(&loop_id).await?;
                }
                Some(r) if r.status.is_terminal() => {
                    // Finished but worktree remains
                    tracing::info!(loop_id, "Cleaning stale worktree");
                    self.cleanup_worktree(&loop_id).await?;
                }
                Some(_) => {
                    // Still running, leave it
                }
            }
        }

        Ok(())
    }
}
```

---

## Disk Space Management

```rust
impl LoopManager {
    async fn ensure_disk_space(&self) -> Result<()> {
        let available = self.check_disk_space().await?;

        if available < self.config.disk_quota_min_gb {
            tracing::warn!(available_gb = available, "Low disk space, running cleanup");

            // Aggressive cleanup
            self.cleanup_orphaned_worktrees().await?;
            self.prune_archives().await?;

            let available = self.check_disk_space().await?;
            if available < self.config.disk_quota_min_gb {
                return Err(eyre!(
                    "Insufficient disk space: {}GB available, {}GB required",
                    available,
                    self.config.disk_quota_min_gb
                ));
            }
        }

        Ok(())
    }
}
```

---

## Merge Strategy

When all loops in a hierarchy complete successfully, their changes need to be merged to main.

### When Merge is Triggered

Merge is triggered when:
1. All CodeLoops in a PlanLoop hierarchy have status `Complete`
2. No loops are `Running`, `Pending`, or `Paused`
3. User confirms merge (if `auto_merge: false`)

```rust
impl LoopManager {
    async fn check_merge_ready(&self, plan_id: &str) -> Result<bool> {
        let descendants = self.find_all_descendants(plan_id).await?;

        // All must be complete
        for loop_record in &descendants {
            if loop_record.status != LoopStatus::Complete {
                return Ok(false);
            }
        }

        Ok(true)
    }
}
```

### Merge Order

Branches are merged depth-first (leaves first), then up the hierarchy:

```
1. CodeLoop branches → merged to parent PhaseLoop branch
2. PhaseLoop branches → merged to parent SpecLoop branch
3. SpecLoop branches → merged to parent PlanLoop branch
4. PlanLoop branch → merged to main
```

This ensures each level validates before bubbling up.

```rust
impl LoopManager {
    async fn merge_hierarchy(&self, plan_id: &str) -> Result<()> {
        let plan = self.state.get_loop(plan_id).await?;
        let plan_branch = format!("loop-{}", plan_id);

        // Collect all branches depth-first
        let branches = self.collect_branches_depth_first(plan_id).await?;

        // Merge each into its parent
        for (child_branch, parent_branch) in branches {
            self.merge_branch(&child_branch, &parent_branch).await?;
        }

        // Final merge to main
        self.merge_branch(&plan_branch, "main").await?;

        Ok(())
    }

    async fn merge_branch(&self, source: &str, target: &str) -> Result<()> {
        let output = Command::new("git")
            .args(["merge", "--no-ff", source, "-m",
                   &format!("Merge {} into {}", source, target)])
            .current_dir(&self.repo_root)
            .output()
            .await?;

        if !output.status.success() {
            return Err(eyre!(
                "Merge conflict: {} into {}\n{}",
                source, target,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }
}
```

### Conflict Handling

If a merge conflict occurs:

1. **Immediate failure** (default): Mark the merge as failed, notify user
2. **User resolution**: Pause and allow user to resolve manually
3. **Abort**: Rollback to pre-merge state

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictStrategy {
    Fail,        // Mark as failed, user must fix
    Pause,       // Pause for manual resolution
    Abort,       // Rollback merge attempt
}

impl LoopManager {
    async fn handle_merge_conflict(
        &self,
        source: &str,
        target: &str,
        error: &str,
    ) -> Result<()> {
        match self.config.conflict_strategy {
            ConflictStrategy::Fail => {
                self.emit(Event::MergeFailed {
                    source: source.to_string(),
                    target: target.to_string(),
                    reason: error.to_string(),
                });
                Err(eyre!("Merge conflict: {}", error))
            }
            ConflictStrategy::Pause => {
                self.emit(Event::MergeConflict {
                    source: source.to_string(),
                    target: target.to_string(),
                    reason: error.to_string(),
                });
                // Wait for user resolution
                self.wait_for_conflict_resolution(source, target).await
            }
            ConflictStrategy::Abort => {
                Command::new("git")
                    .args(["merge", "--abort"])
                    .current_dir(&self.repo_root)
                    .status()
                    .await?;
                Err(eyre!("Merge aborted due to conflict"))
            }
        }
    }
}
```

### Pre-Merge Validation

Before merging to main, run final validation:

```rust
impl LoopManager {
    async fn pre_merge_validate(&self, branch: &str) -> Result<()> {
        // Checkout the branch
        Command::new("git")
            .args(["checkout", branch])
            .current_dir(&self.repo_root)
            .status()
            .await?;

        // Run validation command
        let output = Command::new("sh")
            .args(["-c", &self.config.pre_merge_validation])
            .current_dir(&self.repo_root)
            .output()
            .await?;

        if !output.status.success() {
            return Err(eyre!(
                "Pre-merge validation failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }
}
```

---

## Branch Divergence Handling

When main is updated while loops are running, worktrees may become stale.

### Detection

LoopManager tracks the baseline SHA and checks for divergence:

```rust
impl LoopManager {
    async fn check_divergence(&self) -> Result<Option<String>> {
        let current_main = self.git_rev_parse("main").await?;

        if current_main != self.baseline_sha {
            return Ok(Some(current_main));
        }

        Ok(None)
    }

    async fn git_rev_parse(&self, ref_name: &str) -> Result<String> {
        let output = Command::new("git")
            .args(["rev-parse", ref_name])
            .current_dir(&self.repo_root)
            .output()
            .await?;

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}
```

### Divergence Strategies

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DivergenceStrategy {
    Continue,  // Finish work, handle at merge time (default)
    Pause,     // Pause all loops, notify user
    Rebase,    // Attempt rebase (risky for in-progress work)
}

impl LoopManager {
    async fn handle_divergence(&self, new_sha: &str) -> Result<()> {
        match self.config.divergence_strategy {
            DivergenceStrategy::Continue => {
                // Log warning, continue working
                tracing::warn!(
                    baseline = %self.baseline_sha,
                    current = %new_sha,
                    "Main branch has diverged, will handle at merge"
                );
                self.emit(Event::BranchDiverged {
                    baseline: self.baseline_sha.clone(),
                    current: new_sha.to_string(),
                });
                Ok(())
            }
            DivergenceStrategy::Pause => {
                // Pause all running loops
                let running = self.state.query_loops(&[
                    Filter::eq("status", "running"),
                ]).await?;

                for loop_record in running {
                    self.pause_loop(&loop_record.id).await?;
                }

                self.emit(Event::LoopsPausedDivergence {
                    count: running.len(),
                    reason: "Main branch updated".to_string(),
                });

                Ok(())
            }
            DivergenceStrategy::Rebase => {
                // Attempt rebase of all worktrees
                // Warning: risky if work is in progress
                tracing::warn!("Attempting rebase due to divergence");

                for worktree in self.list_worktrees().await? {
                    self.rebase_worktree(&worktree, new_sha).await?;
                }

                self.baseline_sha = new_sha.to_string();
                Ok(())
            }
        }
    }
}
```

### Periodic Check

Divergence is checked periodically:

```rust
impl LoopManager {
    async fn tick(&mut self) -> Result<()> {
        // ... existing tick logic ...

        // Check for divergence
        if self.last_divergence_check.elapsed() > self.config.divergence_check_interval {
            if let Some(new_sha) = self.check_divergence().await? {
                self.handle_divergence(&new_sha).await?;
            }
            self.last_divergence_check = Instant::now();
        }

        Ok(())
    }
}
```

---

## Configuration

```yaml
# loopr.yml
execution:
  worktree_base: ~/.loopr/worktrees
  max_concurrent_loops: 50
  poll_interval_secs: 1
  disk_quota_min_gb: 5
  preserve_failed_branches: true

  # Merge settings
  auto_merge: false                    # Require user confirmation
  conflict_strategy: fail              # fail | pause | abort
  pre_merge_validation: "cargo test"   # Command to run before merge

  # Divergence settings
  divergence_strategy: continue        # continue | pause | rebase
  divergence_check_interval_secs: 60   # How often to check main
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [runners.md](runners.md) - Tool execution
- [persistence.md](persistence.md) - TaskStore
