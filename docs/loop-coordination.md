# Design Document: Loop Coordination via TaskStore

**Author:** Scott Idler, Claude
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

## Summary

Loops coordinate through TaskStore records rather than IPC or message channels. This "state in files" approach aligns with the Ralph Wiggum principle: progress lives in persistent artifacts, not ephemeral runtime state. Loops discover work, signal completion, and communicate errors by reading and writing records.

## Problem Statement

### Background

Previous attempts at loop coordination used:
- **Unix sockets / IPC** - Complex connection lifecycle, daemon must be running
- **mpsc channels** - In-process only, doesn't survive restarts, hard to debug

Both approaches suffered from:
- State loss on crash
- Difficulty debugging (no audit trail)
- Tight coupling between components

### Problem

How do loops:
1. Discover new work (child loops to spawn)?
2. Signal completion to parent loops?
3. Report errors that require parent re-iteration?
4. Coordinate with sibling loops running in parallel?

### Goals

- All coordination state survives process crashes
- Full audit trail for debugging
- Decoupled components (loops don't need direct references to each other)
- Simple polling-based discovery (no complex event systems)

### Non-Goals

- Real-time sub-second coordination (polling latency is acceptable)
- Complex pub/sub patterns
- Distributed coordination across machines

## Proposed Solution

### Overview

Loops coordinate through three mechanisms:
1. **Loop records** - Status field signals lifecycle state
2. **Signal records** - Explicit messages between loops (stop, pause, error)
3. **Artifact records** - Completed artifacts trigger child loop creation

All stored in TaskStore (JSONL + SQLite cache).

### Coordination Patterns

#### Pattern 1: Parent Spawns Children

When a loop produces artifacts that should spawn children:

```
1. Parent loop completes iteration N
2. Parent writes artifact to iterations/N/artifacts/
3. Parent creates child loop record with:
   - parent_loop: <parent-id>
   - triggered_by: "iterations/N/artifacts/spec.md"
   - status: "pending"
4. Loop manager polls for pending loops, picks up child
```

**No IPC needed** - the child loop record *is* the spawn signal.

#### Pattern 2: Child Signals Completion

When a child loop completes:

```
1. Child loop validation passes
2. Child updates own record: status = "complete"
3. Parent loop (on next poll) queries children:
   SELECT * FROM loops WHERE parent_loop = '<parent-id>'
4. If all children complete → parent proceeds
   If any child failed → parent may re-iterate
```

**No callback needed** - parent polls child status.

#### Pattern 3: Stop Signal Cascade

When a parent needs to stop all descendants (e.g., re-iteration invalidates children):

```
1. Parent writes signal record:
   {
     "type": "signal",
     "signal": "stop",
     "target_loop": "<child-id>",  // or "descendants:<parent-id>"
     "reason": "parent re-iterating",
     "created_at": 1737802800000
   }
2. Child loops poll for signals on each iteration boundary
3. Child sees stop signal → sets own status to "invalidated", exits gracefully
4. Grandchildren see parent invalidated → also stop
```

#### Pattern 4: Error Escalation

When a child encounters an error the parent should know about:

```
1. Child writes signal record:
   {
     "type": "signal",
     "signal": "error",
     "source_loop": "<child-id>",
     "target_loop": "<parent-id>",
     "error": "validation failed: missing security section",
     "created_at": 1737802800000
   }
2. Parent polls for signals, sees error
3. Parent decides: retry child, re-iterate self, or escalate to its parent
```

### Data Model

#### Signal Record Schema

```json
{
  "id": "sig-1737802800",
  "type": "signal",
  "signal": "stop" | "pause" | "resume" | "error" | "info",
  "source_loop": "1737800000",
  "target_loop": "1737802800",
  "target_selector": "descendants:1737800000",
  "reason": "Human-readable explanation",
  "payload": {},  // signal-specific data
  "acknowledged_at": null,  // set when target processes signal
  "created_at": 1737802800000
}
```

#### Target Resolution

**Use exactly one of `target_loop` or `target_selector`, not both.**

| Field | Use Case | Example |
|-------|----------|---------|
| `target_loop` | Signal a specific loop by ID | `"target_loop": "1737802800"` |
| `target_selector` | Signal multiple loops by pattern | `"target_selector": "descendants:1737800000"` |

**Precedence if both specified:** `target_loop` takes precedence. However, this is a schema violation—validation should reject signals with both fields set.

**Selector Patterns:**

| Pattern | Matches |
|---------|---------|
| `descendants:<loop-id>` | All loops with parent chain including `<loop-id>` |
| `type:<loop-type>` | All loops of the given type (e.g., `type:ralph`) |
| `status:<status>` | All loops in the given status (e.g., `status:running`) |

**Example: Stop all descendants when parent re-iterates:**
```json
{
  "signal": "stop",
  "source_loop": "plan-001",
  "target_selector": "descendants:plan-001",
  "reason": "Plan re-iterating, invalidating previous work"
}
```

#### Signal Types

| Signal | Meaning | Response |
|--------|---------|----------|
| `stop` | Terminate immediately | Set status to invalidated, exit |
| `pause` | Suspend execution | Set status to paused, wait for resume |
| `resume` | Continue paused loop | Set status to running, continue |
| `error` | Report problem upstream | Parent decides how to handle |
| `info` | Advisory message | Log and continue |

### Polling Strategy

Loops poll at iteration boundaries (not continuously):

```rust
fn run_iteration(&mut self) -> Result<IterationResult> {
    // Check for signals before starting work
    self.check_signals()?;

    // Do the actual iteration work
    let result = self.execute_iteration()?;

    // Check for signals after completing work
    self.check_signals()?;

    Ok(result)
}

fn check_signals(&mut self) -> Result<()> {
    let signals = self.store.query_signals(self.id)?;
    for signal in signals {
        match signal.signal {
            Signal::Stop => return Err(LoopStopped),
            Signal::Pause => self.wait_for_resume()?,
            Signal::Error => self.handle_child_error(signal)?,
            _ => {}
        }
        signal.acknowledge(&self.store)?;
    }
    Ok(())
}
```

### Loop Manager Role

The loop manager is a simple polling loop:

```rust
loop {
    // Find pending loops ready to run
    let pending = store.query("SELECT * FROM loops WHERE status = 'pending'");

    // Check resource limits
    let running = store.query("SELECT COUNT(*) FROM loops WHERE status = 'running'");
    let slots = max_concurrent - running;

    // Spawn up to `slots` new loops
    for loop_record in pending.take(slots) {
        spawn_loop(loop_record);
    }

    // Check for orphaned loops (parent died)
    let orphans = find_orphans(&store);
    for orphan in orphans {
        orphan.set_status("failed")?;
    }

    sleep(poll_interval);
}
```

**No event bus needed** - just periodic polling.

### Why This Works

| Concern | Solution |
|---------|----------|
| Crash recovery | All state in files, restart and continue |
| Debugging | Query TaskStore, full audit trail |
| Decoupling | Loops only know their own ID and parent ID |
| Simplicity | No connection management, no channel setup |
| Concurrency | TaskStore handles concurrent writes |

### Latency Considerations

With 1-second polling:
- Child spawn: 0-1s delay (acceptable for multi-minute loops)
- Stop signal: 0-1s delay (acceptable for graceful shutdown)
- Error escalation: 0-1s delay (acceptable)

For the Ralph Wiggum pattern where iterations take minutes, this latency is negligible.

## Alternatives Considered

### Alternative 1: Event Bus (mpsc/broadcast channels)

- **Description:** In-process channels for immediate notification
- **Pros:** Sub-millisecond latency, push-based
- **Cons:** State lost on crash, complex lifecycle, hard to debug, problematic in practice
- **Why not chosen:** Previous implementation was "problematic bullshit" - complexity not worth the latency gains

### Alternative 2: Unix Sockets / IPC

- **Description:** Daemon listens on socket, loops connect to send/receive
- **Pros:** Cross-process, push-based
- **Cons:** Connection lifecycle complexity, daemon must be running, state in memory
- **Why not chosen:** Adds operational complexity, state should live in files

### Alternative 3: File Watches (inotify/kqueue)

- **Description:** Watch TaskStore files for changes, react immediately
- **Pros:** Near-instant notification without polling
- **Cons:** Platform-specific, edge cases with rapid writes, complexity
- **Why not chosen:** Could be added later as optimization, polling is simpler to start

## Technical Considerations

### Dependencies

- **TaskStore**: Must support signal record type
- **Loop records**: Must have status field indexable for queries

### Performance

- Polling at 1s interval: negligible CPU overhead
- SQLite queries: O(log n) with proper indexes
- Signal table size: bounded by loop count (signals acknowledged and can be pruned)

### Testing Strategy

- Unit tests for signal creation/acknowledgment
- Integration tests for stop cascade
- Chaos tests: kill processes mid-coordination, verify recovery

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Polling misses rapid state changes | Low | Low | State is eventually consistent, final state correct |
| Signal table grows unbounded | Medium | Low | Prune acknowledged signals older than retention period |
| Orphan detection false positives | Low | Medium | Require multiple missed heartbeats before marking orphan |

## Open Questions

1. **Heartbeat mechanism?** - Should running loops write periodic heartbeats for orphan detection, or rely on process monitoring?
2. **Signal retention** - How long to keep acknowledged signals? (Suggest: 7 days for debugging)

## Next Steps

1. Add signal record type to TaskStore schema
2. Implement signal polling in loop execution
3. Implement stop cascade in loop manager
4. Add orphan detection

## References

- [loop-architecture.md](loop-architecture.md) - Parent design document
- [TaskStore](~/repos/scottidler/taskstore/) - JSONL+SQLite storage
