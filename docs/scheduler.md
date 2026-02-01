# Scheduler: Loop Prioritization

**Author:** Scott A. Idler
**Date:** 2026-01-30
**Status:** Implementation Spec
**Based on:** loopr/docs/scheduler.md

---

## Summary

The scheduler determines which pending loops to run when capacity is available. It uses a priority queue with dependency awareness: loops are ordered by type priority, age, and depth, but cannot run until parent completes.

---

## Design Principles

1. **Hierarchy first** - Child loop cannot run until parent's artifact exists
2. **Depth-first execution** - Complete inner loops before starting new outer loops
3. **FIFO within priority** - Older loops of same priority run first
4. **No starvation** - Age boost prevents new high-priority loops from starving old ones

---

## Priority Model

### Base Priority by Loop Type

| Loop Type | Base Priority | Rationale |
|-----------|--------------|-----------|
| `code` | 100 | Leaf nodes - do the actual work |
| `phase` | 80 | Spawns codes, complete phases |
| `spec` | 60 | Spawns phases, moderate priority |
| `plan` | 40 | Top-level, spawn specs |

Higher number = higher priority = runs first.

### Priority Calculation

```rust
fn calculate_priority(record: &Loop, store: &TaskStore) -> i32 {
    let mut priority = match record.loop_type {
        LoopType::Code => 100,
        LoopType::Phase => 80,
        LoopType::Spec => 60,
        LoopType::Plan => 40,
    };

    // Age boost: +1 per minute waiting, max +50
    let age_minutes = (now_ms() - record.created_at) / 60_000;
    let age_boost = (age_minutes as i32).min(50);
    priority += age_boost;

    // Depth boost: +10 per level deep
    let depth = calculate_depth(record, store);
    priority += depth * 10;

    // Retry penalty: -5 per failed iteration (max -30)
    let retry_penalty = (record.iteration as i32).saturating_sub(1) * 5;
    priority -= retry_penalty.min(30);

    priority
}

fn calculate_depth(record: &Loop, store: &TaskStore) -> i32 {
    let mut depth = 0;
    let mut current_parent = record.parent_id.clone();

    while let Some(parent_id) = current_parent {
        depth += 1;
        match store.get::<Loop>(&parent_id) {
            Ok(Some(parent)) => current_parent = parent.parent_id,
            _ => break,
        }
    }

    depth
}
```

### Example Calculation

```
Loop A: code, created 5 min ago, depth 3, iteration 1
  Base: 100 + Age: 5 + Depth: 30 + Retry: 0 = 135

Loop B: phase, created 30 min ago, depth 2, iteration 1
  Base: 80 + Age: 30 + Depth: 20 + Retry: 0 = 130

Loop C: code, created 1 min ago, depth 3, iteration 5
  Base: 100 + Age: 1 + Depth: 30 + Retry: -20 = 111

Order: A (135) → B (130) → C (111)
```

---

## Dependency Resolution

A loop can only run if:

1. Status is `pending`
2. Parent loop (if any) has status `complete`
3. Triggering artifact exists

```rust
fn is_runnable(record: &Loop, store: &TaskStore) -> bool {
    if record.status != LoopStatus::Pending {
        return false;
    }

    // Check parent
    if let Some(ref parent_id) = record.parent_id {
        let parent = store.get::<Loop>(parent_id).ok().flatten();
        if parent.map(|p| p.status != LoopStatus::Complete).unwrap_or(true) {
            return false;
        }
    }

    // Check artifact
    if let Some(ref input_artifact) = record.input_artifact {
        let path = resolve_artifact_path(record, input_artifact);
        if !path.exists() {
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
    config: SchedulerConfig,
}

impl Scheduler {
    pub fn select(
        &self,
        pending: Vec<Loop>,
        available_slots: usize,
        store: &TaskStore,
    ) -> Vec<Loop> {
        if available_slots == 0 {
            return vec![];
        }

        // Filter to runnable
        let mut runnable: Vec<_> = pending
            .into_iter()
            .filter(|r| is_runnable(r, store))
            .collect();

        // Sort by priority (descending)
        runnable.sort_by(|a, b| {
            let pa = calculate_priority(a, store);
            let pb = calculate_priority(b, store);
            pb.cmp(&pa)
        });

        // Respect per-type limits
        let mut selected = Vec::new();
        let mut type_counts: HashMap<LoopType, usize> = HashMap::new();

        for record in runnable {
            if selected.len() >= available_slots {
                break;
            }

            let count = type_counts.entry(record.loop_type).or_insert(0);
            let limit = self.config.per_type_limit.get(&record.loop_type)
                .copied().unwrap_or(usize::MAX);

            if *count < limit {
                *count += 1;
                selected.push(record);
            }
        }

        selected
    }
}
```

---

## Integration with LoopManager

```rust
impl LoopManager {
    pub async fn tick(&mut self) -> Result<()> {
        let running_count = self.running_loops.len();
        let available = self.config.max_concurrent_loops.saturating_sub(running_count);

        // Query pending
        let pending = self.store.query::<Loop>(&[
            Filter::eq("status", "pending"),
        ])?;

        // Select what to run
        let to_run = self.scheduler.select(pending, available, &self.store);

        // Spawn
        for record in to_run {
            self.spawn_loop(record).await?;
        }

        Ok(())
    }
}
```

---

## Rate Limit Coordination

When Anthropic API returns 429, scheduler backs off globally:

```rust
pub struct RateLimitState {
    pub backoff_until: Option<Instant>,
    pub consecutive_hits: u32,
}

impl Scheduler {
    pub fn select_with_rate_limit(
        &self,
        pending: Vec<Loop>,
        available_slots: usize,
        store: &TaskStore,
        rate_limit: &RateLimitState,
    ) -> Vec<Loop> {
        // Don't start new loops if rate limited
        if rate_limit.is_active() {
            return vec![];
        }

        self.select(pending, available_slots, store)
    }
}

impl RateLimitState {
    pub fn record_rate_limit(&mut self, retry_after: Duration) {
        self.consecutive_hits += 1;
        let backoff = retry_after.max(
            Duration::from_secs(2u64.pow(self.consecutive_hits.min(6)))
        );
        self.backoff_until = Some(Instant::now() + backoff);
    }

    pub fn record_success(&mut self) {
        self.consecutive_hits = 0;
        self.backoff_until = None;
    }

    pub fn is_active(&self) -> bool {
        self.backoff_until.map(|u| Instant::now() < u).unwrap_or(false)
    }
}
```

---

## Configuration

```yaml
# loopr.yml
scheduler:
  poll_interval_secs: 1

  priority:
    plan: 40
    spec: 60
    phase: 80
    code: 100
    age_boost_per_minute: 1
    age_boost_max: 50
    depth_boost_per_level: 10
    retry_penalty_per_iteration: 5
    retry_penalty_max: 30

concurrency:
  max_loops: 50
  max_api_calls: 10
  per_type:
    plan: 2
    spec: 5
    phase: 20
    code: 50
```

---

## References

- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [loop-coordination.md](loop-coordination.md) - Signal handling
- [execution-model.md](execution-model.md) - Worktree management
