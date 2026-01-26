# Merge Conflict Handling

**Version:** 1.0
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [execution-model.md](execution-model.md)

---

## Summary

Merge conflicts are rare in Loopr because each loop works in an isolated git worktree on its own branch. However, conflicts can occur when multiple loops modify the same files and attempt to merge to main. This document specifies detection, resolution strategies, and user notification.

---

## When Conflicts Occur

### Scenario 1: Parallel Phase Loops

```
main: version A of src/api.rs

PhaseLoop-1 (branch: loop-001)     PhaseLoop-2 (branch: loop-002)
├── Modifies src/api.rs            ├── Modifies src/api.rs
├── Adds function foo()            ├── Adds function bar()
├── Completes, merges OK           └── Completes, CONFLICT on merge
```

PhaseLoop-2 conflicts because PhaseLoop-1 already merged changes to the same file.

### Scenario 2: Spec Re-iteration

```
SpecLoop iter 1 → spawns Phase-A, Phase-B, Phase-C
    Phase-A completes, merges to main
    Phase-B completes, merges to main

SpecLoop validation FAILS → iter 2
    New phases spawned, old ones invalidated
    Phase-A and Phase-B changes now potentially conflict with iter 2 work
```

### Scenario 3: External Changes

```
Loop working on branch loop-123
    Meanwhile, developer pushes to main directly

Loop completes, attempts merge
    CONFLICT with external changes
```

---

## Conflict Detection

### Pre-Merge Check

Before attempting merge, detect potential conflicts:

```rust
async fn check_merge_conflicts(loop_id: &str) -> Result<MergeStatus> {
    let branch = format!("loop-{}", loop_id);

    // Fetch latest main
    git(&["fetch", "origin", "main"]).await?;

    // Try merge in dry-run mode
    let result = git(&[
        "merge-tree",
        "--write-tree",
        "HEAD",           // loop branch
        "origin/main",    // target
    ]).await;

    match result {
        Ok(output) if output.status.success() => {
            Ok(MergeStatus::Clean)
        }
        Ok(output) => {
            let conflicts = parse_conflict_files(&output.stdout);
            Ok(MergeStatus::Conflict { files: conflicts })
        }
        Err(e) => Err(e),
    }
}

#[derive(Debug)]
pub enum MergeStatus {
    Clean,
    Conflict { files: Vec<String> },
}
```

### Conflict File Parsing

```rust
fn parse_conflict_files(merge_tree_output: &str) -> Vec<String> {
    // git merge-tree outputs conflict markers
    // Parse to extract file paths
    merge_tree_output
        .lines()
        .filter(|line| line.contains("CONFLICT"))
        .filter_map(|line| {
            // Format: "CONFLICT (content): Merge conflict in <path>"
            line.split("Merge conflict in ")
                .nth(1)
                .map(|s| s.trim().to_string())
        })
        .collect()
}
```

---

## Resolution Strategies

### Strategy 1: Rebase and Retry (Default)

When conflict detected, rebase the loop branch onto latest main and re-run validation:

```rust
async fn handle_conflict_rebase(
    loop_id: &str,
    conflicts: &[String],
) -> Result<ConflictResolution> {
    let branch = format!("loop-{}", loop_id);

    tracing::info!(
        loop_id,
        conflicts = ?conflicts,
        "Merge conflict detected, attempting rebase"
    );

    // Fetch latest main
    git(&["fetch", "origin", "main"]).await?;

    // Attempt rebase
    let rebase_result = git(&["rebase", "origin/main"]).await;

    match rebase_result {
        Ok(_) => {
            tracing::info!(loop_id, "Rebase successful, re-running validation");
            Ok(ConflictResolution::RebaseSuccess)
        }
        Err(_) => {
            // Rebase failed - conflicts too complex
            git(&["rebase", "--abort"]).await?;
            tracing::warn!(loop_id, "Rebase failed, marking for manual resolution");
            Ok(ConflictResolution::ManualRequired { files: conflicts.to_vec() })
        }
    }
}
```

After successful rebase:
1. Re-run validation (code may have changed during rebase)
2. If validation passes, attempt merge again
3. If validation fails, loop re-iterates with new feedback

### Strategy 2: Manual Resolution Queue

For conflicts that can't be auto-resolved:

```rust
async fn queue_manual_resolution(
    loop_id: &str,
    conflicts: &[String],
    store: &TaskStore,
) -> Result<()> {
    // Create a conflict record
    let conflict = ConflictRecord {
        id: generate_id(),
        loop_id: loop_id.to_string(),
        branch: format!("loop-{}", loop_id),
        conflict_files: conflicts.to_vec(),
        status: ConflictStatus::Pending,
        created_at: now_ms(),
    };

    store.create(&conflict)?;

    // Update loop status
    let mut loop_record: LoopRecord = store.get(loop_id)?.unwrap();
    loop_record.status = LoopStatus::Paused;
    loop_record.progress.push_str(&format!(
        "\n\n## Merge Conflict\nFiles: {}\nAwaiting manual resolution.",
        conflicts.join(", ")
    ));
    store.update(&loop_record)?;

    // Notify via TUI
    tracing::warn!(
        loop_id,
        conflicts = ?conflicts,
        "Loop paused: merge conflict requires manual resolution"
    );

    Ok(())
}
```

### Strategy 3: Ours/Theirs Selection (Future)

For simple conflicts, offer one-click resolution:

```rust
// NOT IMPLEMENTED - future enhancement
pub enum ConflictChoice {
    Ours,   // Keep loop's changes
    Theirs, // Keep main's changes
    Both,   // Include both (may not compile)
}

async fn resolve_simple_conflict(
    loop_id: &str,
    file: &str,
    choice: ConflictChoice,
) -> Result<()> {
    let strategy = match choice {
        ConflictChoice::Ours => "ours",
        ConflictChoice::Theirs => "theirs",
        ConflictChoice::Both => unimplemented!(),
    };

    git(&["checkout", &format!("--{}", strategy), file]).await?;
    git(&["add", file]).await?;
    git(&["rebase", "--continue"]).await?;

    Ok(())
}
```

---

## Conflict Prevention

### Ordered Phase Execution

For phases that touch the same files, execute sequentially rather than in parallel:

```rust
fn should_run_in_parallel(phase_a: &PhaseRecord, phase_b: &PhaseRecord) -> bool {
    // Check if file sets overlap
    let files_a: HashSet<_> = phase_a.files_to_modify.iter().collect();
    let files_b: HashSet<_> = phase_b.files_to_modify.iter().collect();

    files_a.is_disjoint(&files_b)
}
```

### Main Branch Locking

During critical merges, briefly lock main to prevent race conditions:

```rust
async fn merge_with_lock(loop_id: &str) -> Result<()> {
    let _lock = acquire_merge_lock().await?;

    // Re-check for conflicts (state may have changed)
    let status = check_merge_conflicts(loop_id).await?;

    match status {
        MergeStatus::Clean => {
            git(&["checkout", "main"]).await?;
            git(&["merge", &format!("loop-{}", loop_id)]).await?;
            git(&["push", "origin", "main"]).await?;
            Ok(())
        }
        MergeStatus::Conflict { files } => {
            Err(GitError::MergeConflict { files }.into())
        }
    }
}
```

### File-Level Merge Hints

In spec.md artifacts, encourage non-overlapping file assignments:

```markdown
## Phases

### Phase 1: User Model
**Files to modify:**
- src/models/user.rs (NEW)
- src/models/mod.rs (ADD export)

### Phase 2: User Repository
**Files to modify:**
- src/repositories/user.rs (NEW)
- src/repositories/mod.rs (ADD export)

### Phase 3: User API
**Files to modify:**
- src/api/users.rs (NEW)
- src/api/mod.rs (ADD export)
```

Each phase creates new files, only touching mod.rs for exports. Conflicts unlikely.

---

## User Notification

### TUI Display

```
┌─────────────────────────────────────────────────────────────────────┐
│ ● Loopr │ Chat · Loops                           ⚠ 1 conflict       │
├─────────────────────────────────────────────────────────────────────┤
│ ─ Loops (6) ────────────────────────────────────────────────────────│
│ ▼ ● Plan: Build REST API                                            │
│   ├── ✓ Spec: User endpoints                                        │
│   │   ├── ✓ Phase: Create models                                    │
│   │   ├── ⚠ Phase: Add validation [CONFLICT]  ← Highlighted        │
│   │   │       Conflicts: src/models/user.rs                         │
│   │   └── ○ Phase: Write tests                                      │
```

### Conflict Detail View

```
┌─────────────────────────────────────────────────────────────────────┐
│ Merge Conflict: Phase "Add validation" (loop-1737802800)            │
├─────────────────────────────────────────────────────────────────────┤
│ Branch: loop-1737802800                                             │
│ Target: main                                                        │
│                                                                     │
│ Conflicting files:                                                  │
│   • src/models/user.rs                                              │
│                                                                     │
│ Resolution options:                                                 │
│   [r] Rebase and retry validation                                   │
│   [m] Manual resolution (opens in $EDITOR)                          │
│   [a] Abort and fail loop                                           │
│   [v] View diff                                                     │
│                                                                     │
│ Note: Main branch was updated by loop-1737801500 (Phase: Create     │
│ models) while this loop was running.                                │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Conflict Record Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub id: String,
    pub loop_id: String,
    pub branch: String,
    pub conflict_files: Vec<String>,
    pub status: ConflictStatus,
    pub resolution: Option<ConflictResolution>,
    pub created_at: i64,
    pub resolved_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictStatus {
    Pending,    // Awaiting resolution
    Resolving,  // Resolution in progress
    Resolved,   // Successfully resolved
    Aborted,    // User chose to abort
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConflictResolution {
    RebaseSuccess,
    ManualResolved,
    Aborted { reason: String },
}
```

---

## Configuration

```yaml
# loopr.yml
merge:
  # Auto-attempt rebase on conflict
  auto_rebase: true

  # Max rebase attempts before requiring manual resolution
  max_rebase_attempts: 3

  # Lock timeout for merge operations (ms)
  lock_timeout_ms: 30000

  # Preserve conflict branches for debugging
  preserve_conflict_branches: true
```

---

## Edge Cases

### Conflict During Rebase

If rebase itself conflicts:

```rust
async fn handle_rebase_conflict(loop_id: &str) -> Result<()> {
    // Abort the rebase
    git(&["rebase", "--abort"]).await?;

    // Mark for manual resolution
    queue_manual_resolution(loop_id, &["rebase conflict"], store).await?;

    Ok(())
}
```

### Multiple Loops Conflict Simultaneously

If several loops hit conflicts at once:

1. Process one at a time (merge lock prevents races)
2. Each successful merge may resolve others' conflicts
3. Re-check conflicts after each successful merge

### Main Branch Force-Pushed

If someone force-pushes main while loops are running:

```rust
// Detect diverged history
async fn check_main_diverged() -> Result<bool> {
    let local = git_output(&["rev-parse", "origin/main"]).await?;
    git(&["fetch", "origin", "main"]).await?;
    let remote = git_output(&["rev-parse", "origin/main"]).await?;

    Ok(local != remote && !is_ancestor(&local, &remote).await?)
}
```

If diverged, all pending merges should be re-evaluated.

---

## References

- [execution-model.md](execution-model.md) - Worktree management
- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [loop-coordination.md](loop-coordination.md) - Inter-loop signaling
- [errors.md](errors.md) - GitError handling
