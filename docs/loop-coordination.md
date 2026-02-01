# Loop Coordination via TaskStore

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/loop-coordination.md (adapted for daemon substrate)

---

## Summary

Loops coordinate through TaskStore records rather than direct IPC. This "state in files" approach aligns with the Ralph Wiggum principle: progress lives in persistent artifacts, not ephemeral runtime state.

**Key distinction in v2:** TUI↔Daemon and Daemon↔Runner use IPC for real-time interaction. But loop-to-loop coordination still uses TaskStore polling for crash resilience.

---

## Why TaskStore for Loop Coordination?

| Concern | TaskStore Polling | Direct IPC |
|---------|-------------------|------------|
| Crash recovery | State survives | Lost |
| Debugging | Full audit trail | Hard to trace |
| Decoupling | Loops don't know each other | Tight coupling |
| Simplicity | Just read/write records | Connection lifecycle |

**The daemon coordinates loops by reading/writing TaskStore, not by direct loop-to-loop communication.**

---

## Coordination Patterns

### Pattern 1: Parent Spawns Children

When a loop produces artifacts that should spawn children:

```
1. Parent loop completes iteration N
2. Parent writes artifact to iterations/N/artifacts/
3. Daemon's LoopManager detects artifact
4. LoopManager creates child Loop with:
   - parent_id: <parent-id>
   - input_artifact: "iterations/N/artifacts/spec.md"
   - status: "pending"
5. Scheduler picks up child on next tick
```

**The child record *is* the spawn signal.**

### Pattern 2: Child Signals Completion

When a child loop completes:

```
1. Child validation passes
2. LoopManager updates child record: status = "complete"
3. LoopManager checks if all siblings complete
4. If all complete → parent can proceed
5. If any failed → parent may re-iterate
```

### Pattern 3: Stop Signal Cascade

When a parent needs to stop descendants:

```
1. Parent needs to re-iterate
2. LoopManager writes signal record:
   {
     "signal_type": "stop",
     "target_selector": "descendants:<parent-id>",
     "reason": "parent re-iterating"
   }
3. Running descendants see signal on next iteration boundary
4. Descendants set status to "invalidated", exit gracefully
5. LoopManager archives invalidated loops
```

### Pattern 4: Error Escalation

When a child encounters an error:

```
1. Child validation fails repeatedly
2. Child reaches max_iterations, status = "failed"
3. LoopManager sends error signal to parent:
   {
     "signal_type": "error",
     "source_loop": "<child-id>",
     "target_loop": "<parent-id>",
     "error": "max iterations reached"
   }
4. Parent sees error, decides: retry, re-iterate, or fail
```

---

## Signal Record Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalRecord {
    pub id: String,
    pub signal_type: SignalType,

    // Source (who sent)
    pub source_loop: Option<String>,

    // Target (use exactly one)
    pub target_loop: Option<String>,      // Specific loop
    pub target_selector: Option<String>,  // Pattern match

    pub reason: String,
    pub payload: Option<serde_json::Value>,

    pub created_at: i64,
    pub acknowledged_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignalType {
    Stop,       // Terminate immediately
    Pause,      // Suspend execution
    Resume,     // Continue paused loop
    Rebase,     // Stop, rebase worktree, continue (see worktree-coordination.md)
    Error,      // Report problem upstream
    Info,       // Advisory message
}
```

### Target Selectors

| Pattern | Matches |
|---------|---------|
| `descendants:<loop-id>` | All loops with parent chain including loop-id |
| `type:<loop-type>` | All loops of given type (e.g., `type:code`) |
| `status:<status>` | All loops in given status |
| `children:<loop-id>` | Direct children only |

---

## Signal Checking (in LoopManager)

```rust
impl LoopManager {
    /// Check for signals targeting this loop
    async fn check_signals(&self, loop_id: &str) -> Result<Option<SignalRecord>> {
        // Direct signals
        let direct = self.store.query::<SignalRecord>(&[
            Filter::eq("target_loop", loop_id),
            Filter::is_null("acknowledged_at"),
        ])?;

        if let Some(signal) = direct.into_iter().next() {
            return Ok(Some(signal));
        }

        // Selector-based signals
        let selectors = self.store.query::<SignalRecord>(&[
            Filter::is_not_null("target_selector"),
            Filter::is_null("acknowledged_at"),
        ])?;

        for signal in selectors {
            if self.matches_selector(loop_id, &signal.target_selector)? {
                return Ok(Some(signal));
            }
        }

        Ok(None)
    }

    fn matches_selector(&self, loop_id: &str, selector: &Option<String>) -> Result<bool> {
        let selector = match selector {
            Some(s) => s,
            None => return Ok(false),
        };

        if selector.starts_with("descendants:") {
            let ancestor_id = &selector[12..];
            return self.is_descendant_of(loop_id, ancestor_id);
        }

        if selector.starts_with("children:") {
            let parent_id = &selector[9..];
            let record: Loop = self.store.get(loop_id)?.ok_or(eyre!("loop not found"))?;
            return Ok(record.parent_id.as_deref() == Some(parent_id));
        }

        // ... other patterns

        Ok(false)
    }

    async fn acknowledge_signal(&self, signal_id: &str) -> Result<()> {
        let mut signal: SignalRecord = self.store.get(signal_id)?.ok_or(eyre!("signal not found"))?;
        signal.acknowledged_at = Some(now_ms());
        self.store.update(&signal)?;
        Ok(())
    }
}
```

---

## Loop Manager Polling Loop

```rust
impl LoopManager {
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // 1. Check for signals to running loops
            for (loop_id, handle) in &self.running_loops {
                if let Some(signal) = self.check_signals(loop_id).await? {
                    self.handle_signal(loop_id, signal).await?;
                }
            }

            // 2. Find pending loops ready to run
            let pending = self.store.query::<Loop>(&[
                Filter::eq("status", "pending"),
            ])?;

            // 3. Filter to runnable (dependencies satisfied)
            let runnable: Vec<_> = pending.into_iter()
                .filter(|r| self.is_runnable(r))
                .collect();

            // 4. Schedule and spawn
            let to_run = self.scheduler.select(runnable, self.available_slots());
            for record in to_run {
                self.spawn_loop(record).await?;
            }

            // 5. Reap completed loops
            self.reap_completed().await?;

            // 6. Sleep until next poll
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    fn is_runnable(&self, record: &Loop) -> bool {
        // Must be pending
        if record.status != LoopStatus::Pending {
            return false;
        }

        // Parent must be complete (if any)
        if let Some(ref parent_id) = record.parent_id {
            let parent = self.store.get::<Loop>(parent_id).ok().flatten();
            if parent.map(|p| p.status != LoopStatus::Complete).unwrap_or(true) {
                return false;
            }
        }

        // Triggering artifact must exist
        if let Some(ref input_artifact) = record.input_artifact {
            let artifact_path = self.resolve_artifact_path(record, input_artifact);
            if !artifact_path.exists() {
                return false;
            }
        }

        true
    }
}
```

---

## TUI Updates (via Daemon IPC)

While loop coordination uses TaskStore, TUI gets updates via daemon events:

```rust
impl Daemon {
    fn notify_tuis(&self, event: DaemonEvent) {
        for conn in &self.tui_connections {
            let _ = conn.send_event(event.clone());
        }
    }

    async fn on_loop_updated(&self, record: &Loop) {
        // Persist to TaskStore
        self.store.update(record)?;

        // Notify connected TUIs
        self.notify_tuis(DaemonEvent {
            event: "loop.updated".to_string(),
            data: serde_json::to_value(record)?,
        });
    }
}
```

---

## Latency Considerations

With 1-second polling:
- Child spawn: 0-1s delay (acceptable for multi-minute loops)
- Stop signal: 0-1s delay (acceptable for graceful shutdown)
- Error escalation: 0-1s delay (acceptable)

For loops with iterations taking minutes, this latency is negligible.

**TUI updates are instant** via IPC events - only loop-to-loop coordination uses polling.

---

## Why Not IPC for Loop Coordination?

Previous attempts used channels/sockets for loop coordination. Problems:

1. **State loss on crash** - Channel contents disappear
2. **Hard to debug** - No audit trail
3. **Complex lifecycle** - Connection management adds code
4. **Tight coupling** - Loops need direct references

TaskStore polling avoids all of these while accepting small latency.

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [worktree-coordination.md](worktree-coordination.md) - Rebase-on-merge protocol
- [scheduler.md](scheduler.md) - Priority model
- [ipc-protocol.md](ipc-protocol.md) - TUI↔Daemon protocol
- [persistence.md](persistence.md) - TaskStore details
