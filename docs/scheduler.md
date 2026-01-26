# Scheduler Design: Loop Prioritization and Execution

**Author:** Scott A. Idler
**Date:** 2026-01-25
**Status:** Implementation Spec
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

---

## Summary

The scheduler determines which pending loops to run when capacity is available. It uses a simple priority queue with dependency awareness: loops are ordered by type priority and creation time, but a loop cannot run until its parent completes. The scheduler runs as part of the LoopManager's polling loop.

---

## Problem Statement

With potentially dozens of pending loops:
- Which loops should run first?
- How do we respect the hierarchy (don't run Phase before Spec completes)?
- How do we prevent starvation of older loops?
- How do we handle resource constraints (max concurrent loops)?

---

## Design Principles

1. **Hierarchy first** - A child loop cannot run until its parent's artifact exists
2. **Depth-first execution** - Complete inner loops before starting new outer loops
3. **FIFO within priority** - Older loops of same priority run first
4. **No starvation** - Age boost prevents new high-priority loops from starving old ones

---

## Priority Model

### Base Priority by Loop Type

| Loop Type | Base Priority | Rationale |
|-----------|--------------|-----------|
| `ralph` | 100 | Leaf nodes - do the actual work |
| `phase` | 80 | Spawns ralphs, should complete phases |
| `spec` | 60 | Spawns phases, moderate priority |
| `plan` | 40 | Top-level, spawn specs |

Higher number = higher priority = runs first.

### Priority Modifiers

```rust
/// Calculate how deep a loop is in the hierarchy by traversing parent_loop chain.
/// PlanLoop = 0, SpecLoop = 1, PhaseLoop = 2, RalphLoop = 3 (typically).
fn calculate_depth(loop_record: &LoopRecord, store: &TaskStore) -> i32 {
    let mut depth = 0;
    let mut current_parent = loop_record.parent_loop.clone();

    while let Some(parent_id) = current_parent {
        depth += 1;
        match store.get::<LoopRecord>(&parent_id) {
            Ok(Some(parent)) => current_parent = parent.parent_loop,
            _ => break, // Parent not found, stop traversing
        }
    }

    depth
}

fn calculate_priority(loop_record: &LoopRecord, store: &TaskStore) -> i32 {
    let mut priority = match loop_record.loop_type {
        LoopType::Ralph => 100,
        LoopType::Phase => 80,
        LoopType::Spec => 60,
        LoopType::Plan => 40,
    };

    // Age boost: +1 per minute waiting, max +50
    let age_minutes = (now_ms() - loop_record.created_at) / 60_000;
    let age_boost = (age_minutes as i32).min(50);
    priority += age_boost;

    // Depth boost: +10 per level deep (encourages completing branches)
    let depth = calculate_depth(loop_record, store);
    priority += depth * 10;

    // Retry penalty: -5 per failed iteration (deprioritize struggling loops)
    let retry_penalty = (loop_record.iteration as i32).saturating_sub(1) * 5;
    priority -= retry_penalty.min(30);

    priority
}
```

### Example Priority Calculation

```
Loop A: ralph, created 5 min ago, depth 3 (plan→spec→phase→ralph), iteration 1
  Base: 100 + Age: 5 + Depth: 30 + Retry: 0 = 135

Loop B: phase, created 30 min ago, depth 2 (plan→spec→phase), iteration 1
  Base: 80 + Age: 30 + Depth: 20 + Retry: 0 = 130

Loop C: ralph, created 1 min ago, depth 3, iteration 5
  Base: 100 + Age: 1 + Depth: 30 + Retry: -20 = 111

Order: A runs first (135), then B (130), then C (111)
```

### Depth Calculation Example

```
PlanLoop-001 (depth=0, no parent)
├── SpecLoop-002 (depth=1, parent=001)
│   ├── PhaseLoop-003 (depth=2, parent=002)
│   │   └── RalphLoop-004 (depth=3, parent=003)
│   └── PhaseLoop-005 (depth=2, parent=002)
└── SpecLoop-006 (depth=1, parent=001)

calculate_depth(RalphLoop-004):
  → parent=003 (depth++)
  → parent=002 (depth++)
  → parent=001 (depth++)
  → parent=None (stop)
  → returns 3
```

### Age vs Depth: Worked Scenarios

**Scenario 1: New leaf vs old branch**
```
Situation:
- RalphLoop-X: created just now, depth 3
- SpecLoop-Y: created 40 minutes ago, depth 1

Calculation:
  Ralph-X: 100 (base) + 0 (age) + 30 (depth) = 130
  Spec-Y:   60 (base) + 40 (age) + 10 (depth) = 110

Winner: Ralph-X (leaf node wins despite being newer)
Why: Depth boost outweighs age; completing leaves unblocks parents.
```

**Scenario 2: Very old plan vs new ralph**
```
Situation:
- PlanLoop-A: waiting 50+ minutes (max age boost)
- RalphLoop-B: created just now, depth 3

Calculation:
  Plan-A:  40 (base) + 50 (max age) + 0 (depth) = 90
  Ralph-B: 100 (base) + 0 (age) + 30 (depth) = 130

Winner: Ralph-B still wins
Why: Type priority + depth > max age boost. This is intentional.
```

**Scenario 3: Struggling loop gets deprioritized**
```
Situation:
- RalphLoop-X: depth 3, iteration 7 (failing repeatedly)
- RalphLoop-Y: depth 3, iteration 1 (fresh start)
- Both created 10 minutes ago

Calculation:
  Ralph-X: 100 + 10 + 30 - 30 (6 retries × 5, capped) = 110
  Ralph-Y: 100 + 10 + 30 - 0 = 140

Winner: Ralph-Y (fresh loop gets priority)
Why: Let healthy loops run; struggling ones may have deeper issues.
```

**Scenario 4: Completing a branch**
```
Situation: PhaseLoop-003 has two children:
- RalphLoop-004: depth 3, created 5 min ago, iteration 1
- RalphLoop-005: depth 3, created 2 min ago, iteration 1

Both are runnable (parent complete).

Calculation:
  Ralph-004: 100 + 5 + 30 = 135
  Ralph-005: 100 + 2 + 30 = 132

Order: 004 runs first, then 005 (FIFO within same type/depth)
Why: Older loops of same priority run first to prevent starvation.
```

---

## Dependency Resolution

A loop can only run if:
1. Status is `pending`
2. Parent loop (if any) has status `complete`
3. Triggering artifact exists (parsed from `triggered_by` path)

```rust
fn is_runnable(loop_record: &LoopRecord, store: &TaskStore) -> bool {
    // Must be pending
    if loop_record.status != LoopStatus::Pending {
        return false;
    }

    // Check parent dependency
    if let Some(ref parent_id) = loop_record.parent_loop {
        let parent: Option<LoopRecord> = store.get(parent_id).ok().flatten();
        match parent {
            None => return false,  // Parent doesn't exist
            Some(p) if p.status != LoopStatus::Complete => return false,
            _ => {}
        }
    }

    // Check artifact exists
    if let Some(ref triggered_by) = loop_record.triggered_by {
        let artifact_path = resolve_artifact_path(loop_record, triggered_by);
        if !artifact_path.exists() {
            return false;
        }
    }

    true
}
```

---

## Scheduler Implementation

```rust
pub struct Scheduler {
    max_concurrent: usize,
}

impl Scheduler {
    /// Select loops to run given current capacity
    pub fn select_runnable(
        &self,
        store: &TaskStore,
        currently_running: usize,
    ) -> Vec<LoopRecord> {
        let available_slots = self.max_concurrent.saturating_sub(currently_running);
        if available_slots == 0 {
            return vec![];
        }

        // Query all pending loops
        let pending: Vec<LoopRecord> = store
            .query(&[Filter::eq("status", "pending")])
            .unwrap_or_default();

        // Filter to runnable (dependencies satisfied)
        let mut runnable: Vec<LoopRecord> = pending
            .into_iter()
            .filter(|r| is_runnable(r, store))
            .collect();

        // Sort by priority (descending)
        runnable.sort_by(|a, b| {
            let pa = calculate_priority(a, store);
            let pb = calculate_priority(b, store);
            pb.cmp(&pa)  // Higher priority first
        });

        // Take up to available slots
        runnable.truncate(available_slots);

        runnable
    }
}
```

---

## Integration with LoopManager

The scheduler is invoked during the LoopManager's polling loop:

```rust
impl LoopManager {
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // Count currently running
            let running_count = self.running_loops.len();

            // Ask scheduler for loops to start
            let to_start = self.scheduler.select_runnable(&self.store, running_count);

            // Spawn each selected loop
            for record in to_start {
                self.spawn_loop(record).await?;
            }

            // Reap completed loops
            self.reap_completed().await?;

            // Sleep until next poll
            tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs)).await;
        }
    }
}
```

---

## Fairness and Starvation Prevention

### Age Boost

Loops gain +1 priority per minute waiting, capped at +50. This ensures:
- A ralph created now (priority 100) won't permanently block a plan created an hour ago (priority 40 + 50 = 90)
- After ~60 minutes, even the lowest priority loop will run

### Retry Penalty

Loops that keep failing get deprioritized:
- First iteration: no penalty
- Second iteration: -5
- Fifth iteration: -20
- Max penalty: -30

This prevents a single buggy loop from monopolizing slots while it fails repeatedly.

### Type Priority Rationale

Ralph loops (leaf nodes) have highest base priority because:
1. They do the actual implementation work
2. Completing them unblocks parent loops to finish
3. The hierarchy can't progress until leaf work is done

Plan loops have lowest priority because:
1. They just create artifacts (specs)
2. Having many pending plans doesn't help—we need to complete existing work first

---

## Depth-First Behavior

The depth boost (+10 per level) encourages completing deep branches:

```
Plan A (depth 0, priority 40)
├── Spec A1 (depth 1, priority 70)
│   ├── Phase A1a (depth 2, priority 100)
│   │   └── Ralph A1a1 (depth 3, priority 130)  ← Runs first
│   └── Phase A1b (depth 2, priority 100)
└── Spec A2 (depth 1, priority 70)

Plan B (depth 0, priority 40)  ← Starved until A completes
```

This creates depth-first execution: we complete Ralph A1a1, then Phase A1a can complete, then move to Phase A1b, etc.

---

## Concurrency Limits

### Global Limit

```yaml
# loopr.yml
concurrency:
  max_loops: 50       # Total concurrent loops
  max_api_calls: 10   # Concurrent LLM API calls (rate limit protection)
```

### Per-Type Limits (Optional)

```yaml
concurrency:
  max_loops: 50
  per_type:
    plan: 2           # Max 2 concurrent plan loops
    spec: 5
    phase: 20
    ralph: 50         # Unlimited (up to global max)
```

Implementation:

```rust
fn is_within_type_limit(
    loop_type: LoopType,
    currently_running: &HashMap<LoopType, usize>,
    config: &ConcurrencyConfig,
) -> bool {
    let running = currently_running.get(&loop_type).copied().unwrap_or(0);
    let limit = config.per_type.get(&loop_type).copied().unwrap_or(usize::MAX);
    running < limit
}
```

---

## Rate Limit Coordination

When the Anthropic API returns 429 (rate limited), the scheduler must back off globally to prevent all loops from hitting the limit simultaneously.

### Shared Rate Limit State

```rust
/// Global rate limit state shared across all loops
pub struct RateLimitState {
    /// When we can resume API calls (None = no active limit)
    pub backoff_until: Option<Instant>,
    /// Number of consecutive rate limit hits
    pub consecutive_hits: u32,
    /// Last successful API call time
    pub last_success: Instant,
}

impl RateLimitState {
    pub fn is_rate_limited(&self) -> bool {
        self.backoff_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    pub fn record_rate_limit(&mut self, retry_after: Duration) {
        self.consecutive_hits += 1;

        // Exponential backoff: use max of API's retry_after and our calculated delay
        let exp_backoff = Duration::from_secs(2u64.pow(self.consecutive_hits.min(6)));
        let delay = retry_after.max(exp_backoff);

        self.backoff_until = Some(Instant::now() + delay);

        tracing::warn!(
            retry_after_secs = delay.as_secs(),
            consecutive_hits = self.consecutive_hits,
            "Rate limited, backing off globally"
        );
    }

    pub fn record_success(&mut self) {
        self.consecutive_hits = 0;
        self.backoff_until = None;
        self.last_success = Instant::now();
    }
}
```

### Scheduler Integration

The scheduler checks rate limit state before selecting loops:

```rust
impl Scheduler {
    pub fn select_runnable(
        &self,
        store: &TaskStore,
        currently_running: usize,
        rate_limit: &RateLimitState,  // <-- Added
    ) -> Vec<LoopRecord> {
        // Don't start new loops if rate limited
        if rate_limit.is_rate_limited() {
            tracing::debug!(
                until = ?rate_limit.backoff_until,
                "Rate limited, not selecting new loops"
            );
            return vec![];
        }

        // ... rest of selection logic ...
    }
}
```

### Loop-Level Rate Limit Handling

When a loop encounters a rate limit, it reports to the global state:

```rust
async fn handle_llm_error(
    error: &LlmError,
    rate_limit: &Arc<RwLock<RateLimitState>>,
) -> Result<()> {
    if let LlmError::RateLimited { retry_after } = error {
        let mut state = rate_limit.write().await;
        state.record_rate_limit(*retry_after);

        // Also notify other loops via signal
        // They'll see the global backoff on their next iteration
    }
    Ok(())
}
```

### Configuration

```yaml
# loopr.yml
rate_limit:
  # Initial backoff on first rate limit (seconds)
  initial_backoff_secs: 5

  # Maximum backoff (seconds)
  max_backoff_secs: 120

  # How long to wait after backoff before resuming full speed
  recovery_period_secs: 30

  # Max concurrent API calls (soft limit to prevent rate limits)
  max_concurrent_api_calls: 10
```

### Graceful Degradation

When rate limited, the scheduler can prioritize loops that don't need API calls:

```rust
fn select_during_rate_limit(
    &self,
    store: &TaskStore,
) -> Vec<LoopRecord> {
    // Select loops that are in "validation" state - they run tests, not API calls
    store.query::<LoopRecord>(&[
        Filter::eq("status", "running"),
        Filter::eq("phase", "validating"),
    ]).unwrap_or_default()
}
```

---

## Edge Cases

### Orphaned Loops

If a parent loop is deleted while children are pending:

```rust
fn handle_orphan(loop_record: &LoopRecord, store: &TaskStore) -> Result<()> {
    // Parent was deleted - mark this loop as failed
    let mut updated = loop_record.clone();
    updated.status = LoopStatus::Failed;
    updated.updated_at = now_ms();
    store.update(&updated)?;

    // Recursively fail all descendants
    let children = store.query::<LoopRecord>(&[
        Filter::eq("parent_loop", &loop_record.id),
    ])?;
    for child in children {
        handle_orphan(&child, store)?;
    }

    Ok(())
}
```

### Parent Re-iteration

When a parent loop re-iterates (new iteration, new artifact), children become stale:

1. Scheduler detects `triggered_by` path no longer matches parent's current iteration
2. Children are marked `invalidated`
3. New children will be spawned from the new artifact

See [loop-architecture.md](loop-architecture.md) for invalidation cascade details.

### Resource Exhaustion

If disk space is low, the scheduler can pause new loop creation:

```rust
fn can_spawn_new_loop() -> bool {
    let available_gb = check_disk_space();
    if available_gb < config.disk_quota_min_gb {
        tracing::warn!(available_gb, "Low disk space, pausing new loop creation");
        return false;
    }
    true
}
```

---

## Metrics and Observability

### Scheduler Metrics

```rust
// Log on each scheduling decision
tracing::info!(
    pending = pending_count,
    runnable = runnable_count,
    selected = selected_count,
    running = running_count,
    slots_available = available_slots,
    "Scheduler tick"
);

// Per-loop selection logging
for record in &selected {
    tracing::debug!(
        loop_id = %record.id,
        loop_type = ?record.loop_type,
        priority = calculate_priority(record, store),
        age_ms = now_ms() - record.created_at,
        "Selected loop for execution"
    );
}
```

### Debugging Priority Issues

```bash
# Show all pending loops with calculated priorities
loopr debug scheduler

# Output:
# ID          TYPE    PRIORITY  AGE    DEPTH  ITER  RUNNABLE
# 1737802800  ralph   135       5m     3      1     yes
# 1737802500  phase   130       30m    2      1     yes
# 1737802700  ralph   111       1m     3      5     yes
# 1737802000  plan    90        50m    0      1     no (parent incomplete)
```

---

## Configuration

```yaml
# loopr.yml
scheduler:
  poll_interval_secs: 1     # How often to check for runnable loops

  # Priority tuning
  priority:
    plan: 40
    spec: 60
    phase: 80
    ralph: 100

    age_boost_per_minute: 1
    age_boost_max: 50

    depth_boost_per_level: 10

    retry_penalty_per_iteration: 5
    retry_penalty_max: 30

concurrency:
  max_loops: 50
  max_api_calls: 10
```

---

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_calculation() {
        let store = TaskStore::in_memory();
        let mut record = LoopRecord::new_ralph("test");
        record.created_at = now_ms() - 5 * 60 * 1000; // 5 minutes ago
        store.create(&record)?;

        let priority = calculate_priority(&record, &store);
        // Base 100 + age 5 + depth 0 (no parent) = 105
        assert_eq!(priority, 105);
    }

    #[test]
    fn test_dependency_blocks_execution() {
        let store = TaskStore::in_memory();

        // Create parent in running state
        let parent = LoopRecord::new_plan("parent");
        parent.status = LoopStatus::Running;
        store.create(&parent)?;

        // Create child
        let mut child = LoopRecord::new_spec("child");
        child.parent_loop = Some(parent.id.clone());
        child.status = LoopStatus::Pending;
        store.create(&child)?;

        // Child should not be runnable (parent not complete)
        assert!(!is_runnable(&child, &store));

        // Complete parent
        parent.status = LoopStatus::Complete;
        store.update(&parent)?;

        // Now child should be runnable
        assert!(is_runnable(&child, &store));
    }

    #[test]
    fn test_age_boost_prevents_starvation() {
        let store = TaskStore::in_memory();

        let mut young_ralph = LoopRecord::new_ralph("young");
        young_ralph.created_at = now_ms();
        store.create(&young_ralph)?;

        let mut old_plan = LoopRecord::new_plan("old");
        old_plan.created_at = now_ms() - 60 * 60 * 1000; // 1 hour ago
        store.create(&old_plan)?;

        // Young ralph: 100
        // Old plan: 40 + 50 (max age boost) = 90
        // Ralph still wins, but barely
        assert!(calculate_priority(&young_ralph, &store) > calculate_priority(&old_plan, &store));

        // But very old plan beats young ralph
        old_plan.created_at = now_ms() - 120 * 60 * 1000; // 2 hours
        store.update(&old_plan)?;
        // Still capped at 50, so: 40 + 50 = 90 < 100
        // This is intentional - ralphs should generally run first
    }
}
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy and lifecycle
- [loop-coordination.md](loop-coordination.md) - LoopManager polling
- [execution-model.md](execution-model.md) - Worktree management
- [loop-config.md](loop-config.md) - Configuration schema
