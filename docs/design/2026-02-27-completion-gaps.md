# Design Document: Loopr v3 Completion — All 33 Remaining Gaps

**Author:** Claude (Opus 4.6)
**Date:** 2026-02-27
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

An exhaustive line-by-line audit of Loopr v3 design docs (MVP1-4) against the implementation found 33 items that were designed but not implemented: 15 missing features, 14 missing enforcement points, 2 missing wiring connections, and 2 deviations from spec. This document specifies the exact fix for every single one, organized into 8 phases by dependency order. No item is deferred.

## Problem Statement

### Background

Loopr v3 was built across four MVPs by the Ralph Wiggum Loop. Each MVP was validated by `otto ci` (compile, clippy, fmt, tests) after every iteration. The result: 39K lines of Rust, 1244 passing tests, zero clippy warnings — but `otto ci` validates syntax and unit correctness, not design fidelity. A line-by-line audit against the original design docs revealed 33 gaps where specified behavior was never implemented.

### Problem

33 items from the MVP1-4 design docs are not in the code. Each round of auditing has found "new" gaps because previous rounds categorized items as "future work" or "not bugs." This document addresses every single one with no exceptions.

### Goals

- Implement all 33 items exactly as specified in the design docs
- Add tests for each item proving correctness
- Pass `otto ci` after every phase
- Zero remaining gaps between design docs and code

### Non-Goals

- New features not in any design doc
- Performance optimization
- Refactoring existing working code

## Master Checklist

| # | Phase | Category | Gap | Primary File |
|---|-------|----------|-----|-------------|
| 1 | P3 | Missing Feature | `*.update` IPC handlers (7 methods) | `handlers.rs` |
| 2 | P4 | Missing Feature | TUI keybindings `n` (new) and `t` (transition) | `tui/input.rs` |
| 3 | P1 | Missing Feature | `record.deleted` event constructor | `ipc/protocol.rs` |
| 4 | P1 | Missing Feature | `transition.rejected` event broadcast | `handlers.rs` |
| 5 | P1 | Missing Feature | `validation.started` / `validation.completed` events | `integrator_task.rs` |
| 6 | P4 | Missing Feature | `loopr role` CLI command | `cli/mod.rs` |
| 7 | P4 | Missing Feature | `daemon.version` file on startup | `daemon/mod.rs` |
| 8 | P8 | Missing Feature | Audit trail for `--skip-validation` | `handlers.rs` |
| 9 | P5 | Missing Feature | `agent.output` IPC method + ring buffer | `handlers.rs` |
| 10 | P5 | Missing Feature | Tool SIGTERM → wait 5s → SIGKILL | `tools/mod.rs` |
| 11 | P5 | Missing Feature | Agent session wall-clock timeouts | `executor.rs` |
| 12 | P1 | Missing Feature | `learning.policy_contradicted` event | `handlers.rs` |
| 13 | P4 | Missing Feature | `loopr agent start-integrator` CLI | `cli/mod.rs` |
| 14 | P7 | Missing Feature | Integrator `git merge` of bundle branches | `integrator_task.rs` |
| 15 | P8 | Missing Feature | `confidence` in Learning indexed_fields | `domain/learning.rs` |
| 16 | P2 | Missing Enforce | Tick: one in Sealing/Validating at handler level | `handlers.rs` |
| 17 | P2 | Missing Enforce | Bundle: locked resources check | `handlers.rs` |
| 18 | P2 | Missing Enforce | Bundle: verification required for Reviewed+ | `handlers.rs` |
| 19 | P6 | Missing Enforce | `StalePolicy::ReplanAtSafePoint` | `integrator_task.rs` |
| 20 | P6 | Missing Enforce | `StalePolicy::AutoReplayAndVerify` | `integrator_task.rs` |
| 21 | P6 | Missing Enforce | `TickCadence::Batched` | `integrator_task.rs` |
| 22 | P2 | Missing Enforce | `BundleSizePolicy` enforcement | `handlers.rs` |
| 23 | P6 | Missing Enforce | `ValidatorStrictness` wiring | `handlers.rs` |
| 24 | P2 | Missing Enforce | `PromotionPolicy` from config | `handlers.rs` |
| 25 | P2 | Missing Enforce | `max_lock_ttl_minutes` + expiry sweep | `handlers.rs`, `context.rs` |
| 26 | P2 | Missing Enforce | Researcher dedup by target_id | `handlers.rs` |
| 27 | P8 | Missing Enforce | Draft-awareness guard in executor | `executor.rs` |
| 28 | P8 | Missing Enforce | One-level-per-iteration guard | `executor.rs` |
| 29 | P5 | Missing Wiring | Auto-start agents on transitions | `handlers.rs` |
| 30 | P8 | Missing Wiring | Lock expiry in crash recovery | `context.rs` |
| 31 | P5 | Missing Wiring | `auto_start_coordinator` at startup | `daemon/mod.rs` |
| 32 | P7 | Deviation | SearchCode: `grep` → `rg` | `researcher.rs` |
| 33 | P2 | Deviation | `system.status` missing Tick SHA + stale items | `handlers.rs` |

## File Reference

| File | Gaps Addressed |
|------|---------------|
| `src/daemon/handlers.rs` | #1, #4, #5, #8, #12, #16, #17, #18, #22, #24, #25, #26, #29, #33 |
| `src/ipc/protocol.rs` | #3, #4, #5, #12 |
| `src/tui/input.rs` | #2 |
| `src/tui/run.rs` | #2 |
| `src/tui/app.rs` | #2 |
| `src/tools/mod.rs` | #10 |
| `src/agents/executor.rs` | #11, #27, #28 |
| `src/agents/integrator_task.rs` | #14, #19, #20, #21 |
| `src/agents/researcher.rs` | #32 |
| `src/daemon/mod.rs` | #7, #31 |
| `src/daemon/context.rs` | #30 |
| `src/cli/mod.rs` | #6, #13 |
| `src/cli/dispatch.rs` | #6, #13 |
| `src/config.rs` | #11 |
| `src/domain/learning.rs` | #15 |
| `src/validator/mod.rs` | #23 |

## Proposed Solution

### Phase 1: IPC Events & Protocol (Gaps #3, #4, #5, #12)

These are missing event constructors and emission points. No behavioral changes, just broadcasting events that consumers (TUI, future tooling) can observe.

---

**Gap #3: `record.deleted` event**

*Design:* MVP1 event catalog specifies `record.deleted { collection, id }`.

*Fix:*

1. Add constructor to `src/ipc/protocol.rs`:

```rust
pub fn record_deleted(collection: &str, id: &str) -> Self {
    Self::new(
        "record.deleted",
        json!({ "collection": collection, "id": id }),
    )
}
```

2. No handler currently deletes records (MVP1 specifies "delete bottom-up only"), so the event exists for when delete handlers are added. The constructor is the deliverable.

*Tests:* Unit test in `protocol.rs` asserting event name and payload shape.

---

**Gap #4: `transition.rejected` event broadcast**

*Design:* MVP1 event catalog specifies `transition.rejected { collection, id, from, to, role, reason }`.

*Fix:*

1. Add constructor to `src/ipc/protocol.rs`:

```rust
pub fn transition_rejected(
    collection: &str, id: &str, from: &str, to: &str, role: &str, reason: &str,
) -> Self {
    Self::new(
        "transition.rejected",
        json!({
            "collection": collection, "id": id,
            "from": from, "to": to, "role": role, "reason": reason,
        }),
    )
}
```

2. In every `handle_*_transition` function in `handlers.rs`, when `validate_transition()` returns `Err`, emit the event before returning the error response:

```rust
Err(e) => {
    let _ = event_tx.send(DaemonEvent::transition_rejected(
        "plans", &id, &format!("{:?}", current_status), &format!("{:?}", target_status), &role_str, &e.to_string(),
    ));
    return DaemonResponse::err(req.id, RpcError::transition_rejected(&e.to_string()));
}
```

This applies to: `handle_plan_transition`, `handle_spec_transition`, `handle_phase_transition`, `handle_work_transition`, `handle_bundle_transition`, `handle_tick_transition`.

*Tests:* For each collection, test that a rejected transition emits the event via `event_tx.try_recv()`.

---

**Gap #5: `validation.started` and `validation.completed` events**

*Design:* MVP1 event catalog specifies these for Tick validation.

*Fix:*

1. Add constructors to `src/ipc/protocol.rs`:

```rust
pub fn validation_started(tick_id: &str) -> Self {
    Self::new("validation.started", json!({ "tick_id": tick_id }))
}

pub fn validation_completed(tick_id: &str, success: bool, log: &str) -> Self {
    Self::new(
        "validation.completed",
        json!({ "tick_id": tick_id, "success": success, "log": log }),
    )
}
```

2. In `src/agents/integrator_task.rs`, in the `run_integrator_cycle` function:
   - Emit `validation_started` before calling `run_validation_commands()`
   - Emit `validation_completed` after validation returns (with success/failure and log)

3. In `src/daemon/handlers.rs`, in `handle_integrator_validate`:
   - Same pattern: emit before/after validation

*Tests:* Integration test: create tick, run integrator validate, assert both events received.

---

**Gap #12: `learning.policy_contradicted` event**

*Design:* MVP4 specifies that contradicting a promoted Learning emits `learning.policy_contradicted { learning_id, contradiction_content }`.

*Fix:*

1. Add constructor to `src/ipc/protocol.rs`:

```rust
pub fn learning_policy_contradicted(learning_id: &str) -> Self {
    Self::new(
        "learning.policy_contradicted",
        json!({ "learning_id": learning_id }),
    )
}
```

2. In `handle_learning_contradict` in `handlers.rs`, after calling `learning.contradict()`, check if the learning is promoted:

```rust
if learning.promoted {
    let _ = event_tx.send(DaemonEvent::learning_policy_contradicted(&id));
}
```

*Tests:* Create a promoted learning, contradict it, assert `learning.policy_contradicted` event emitted.

---

### Phase 2: Handler Enforcement (Gaps #16, #17, #18, #22, #24, #25, #26, #33)

These are invariant checks that should exist in handlers but don't. Each is a guard clause added to an existing handler.

---

**Gap #16: Tick "only one in Sealing/Validating" at handler level**

*Design:* MVP1 Tick invariant.

*Fix:* In `handle_tick_transition`, before allowing transitions to `Sealing` or `Validating`, check:

```rust
if matches!(target_status, TickStatus::Sealing | TickStatus::Validating) {
    let has_active = ticks.values().any(|t| {
        t.id != id && matches!(t.status, TickStatus::Sealing | TickStatus::Validating)
    });
    if has_active {
        return DaemonResponse::err(
            req.id,
            RpcError::precondition_failed("Another Tick is already in Sealing/Validating"),
        );
    }
}
```

*Tests:* Create two ticks (second after first is Published). Transition first to Sealing. Try to transition second to Sealing — expect error.

---

**Gap #17: Bundle "cannot touch locked resources it doesn't own"**

*Design:* MVP1 Bundle invariant.

*Fix:* In `handle_bundle_transition`, when transitioning to `Integrating` (the point where the bundle's files would be merged), check:

```rust
if target_status == BundleStatus::Integrating {
    let locks = stores.locks.read().unwrap();
    for path in &bundle.touched_paths {
        if let Some(lock) = locks.values().find(|l| l.resource == *path && l.is_active()) {
            if lock.holder_id != bundle.work_id {
                return DaemonResponse::err(
                    req.id,
                    RpcError::precondition_failed(&format!(
                        "Bundle touches locked resource '{}' owned by '{}'", path, lock.holder_id
                    )),
                );
            }
        }
    }
}
```

*Note:* `touched_paths` starts empty at Bundle creation. It gets populated by the Implementer's `ProposeBundle` action (which sets `files_changed`) or by `bundle.update` (Gap #1). The lock check naturally becomes active once `touched_paths` is populated. If `touched_paths` is empty, no paths to check, so the guard is a no-op (safe default).

*Tests:* Create lock on "src/foo.rs" held by wi-1. Create bundle for wi-2, then update it with touched_paths=["src/foo.rs"]. Try to transition to Integrating — expect error.

---

**Gap #18: Bundle "verification metadata required for Reviewed+"**

*Design:* MVP1 Bundle invariant.

*Fix:* In `handle_bundle_transition`, when transitioning to `Reviewed` or beyond, check that `verification` is non-empty:

```rust
if matches!(target_status, BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating | BundleStatus::Merged) {
    if bundle.verification.is_empty() && !matches!(bundle.status, BundleStatus::Reviewed | BundleStatus::Accepted | BundleStatus::Integrating) {
        return DaemonResponse::err(
            req.id,
            RpcError::precondition_failed("Bundle must have verification metadata before Reviewed+"),
        );
    }
}
```

*Note:* Only enforce on the first transition INTO Reviewed. Once past Reviewed, verification was already checked.

*Tests:* Create bundle with empty verification. Try to transition to Reviewed — expect error. Set verification, retry — succeeds.

---

**Gap #22: `BundleSizePolicy` enforcement**

*Design:* MVP4 specifies `max_files_touched` and `max_loc_changed` are enforced.

*Fix:*

**Where to enforce:** `touched_paths` is empty at `Bundle::new()` time (bundles are created with `touched_paths: Vec::new()`). The paths get populated either via `bundle.update` (Gap #1) or via the `ProposeBundle` agent action. Therefore, enforce the policy in **two places**:

1. In `handle_bundle_update` (from Gap #1), when `touched_paths` is being set:

```rust
if let Some(paths) = req.params.get("touched_paths").and_then(|v| v.as_array()) {
    let policy = &stores.config.strategy.bundle_size;
    if paths.len() as u32 > policy.max_files_touched {
        return DaemonResponse::err(req.id, RpcError::precondition_failed(&format!(
            "Bundle touches {} files, exceeds max_files_touched={}",
            paths.len(), policy.max_files_touched
        )));
    }
    bundle.touched_paths = paths.iter().filter_map(|v| v.as_str().map(String::from)).collect();
}
```

2. In `handle_bundle_create`, when `files_changed` param is provided (the create handler already parses this):

```rust
let policy = &stores.config.strategy.bundle_size;
if bundle.touched_paths.len() as u32 > policy.max_files_touched {
    return DaemonResponse::err(req.id, RpcError::precondition_failed(&format!(
        "Bundle touches {} files, exceeds max_files_touched={}",
        bundle.touched_paths.len(), policy.max_files_touched
    )));
}
```

3. For LOC checking, add `loc_changed: Option<u32>` field to Bundle with `#[serde(default)]`:

```rust
pub loc_changed: Option<u32>,
```

Check on create and update when present:

```rust
if let Some(loc) = bundle.loc_changed {
    if loc > policy.max_loc_changed {
        return DaemonResponse::err(req.id, RpcError::precondition_failed(&format!(
            "Bundle changes {} LOC, exceeds max_loc_changed={}", loc, policy.max_loc_changed
        )));
    }
}
```

*Tests:*
1. Create bundle with `files_changed: ["a","b","c","d","e","f","g","h","i"]` (9 files) — expect rejection (default max=8).
2. Update bundle setting touched_paths to 10 entries — expect rejection.
3. Create bundle with 2 files — succeeds.

---

**Gap #24: `PromotionPolicy` from StrategyConfig**

*Design:* MVP4 specifies the TODO at handlers.rs:1755 should be resolved.

*Fix:* In `handle_learning_reinforce`, replace `PromotionPolicy::default()` with:

```rust
let promotion = stores.config.strategy.promotion;
```

This is a one-line fix.

*Tests:* Create stores with custom PromotionPolicy (min_reinforcements=5). Reinforce 3 times — should NOT auto-promote. Reinforce 5 times — should auto-promote.

---

**Gap #25: `max_lock_ttl_minutes` enforcement**

*Design:* MVP4 specifies locks have TTL, auto-expiry sweep, and `lock.create` sets `expires_at`.

*Fix:*

1. In `handle_lock_create` in `handlers.rs`, set `expires_at` from config:

```rust
let ttl_minutes = stores.config.strategy.max_lock_ttl_minutes;
lock.expires_at = Some(crate::id::now_millis() + (ttl_minutes as i64 * 60 * 1000));
```

*Note:* `now_millis()` returns `i64`, `max_lock_ttl_minutes` is `u64`. Cast to `i64` before arithmetic. For any sane TTL value (< 292 million years), this won't overflow.

2. Add a sweep function `expire_stale_locks()` to `daemon/context.rs` following the `recover_orphaned_records` pattern:

```rust
pub fn expire_stale_locks(&self) -> usize {
    let mut expired = 0;
    let mut locks = self.stores.locks.write().unwrap();
    for (id, lock) in locks.iter_mut() {
        if lock.is_active() && lock.is_expired() {
            warn!("Auto-expiring lock {}: resource={}", id, lock.resource);
            lock.expire();
            if let Some(store) = &self.stores.store
                && let Err(e) = store.lock().unwrap().update(lock.clone())
            {
                warn!("Failed to persist expired lock: {}", e);
            }
            expired += 1;
        }
    }
    expired
}
```

3. Call `expire_stale_locks()` from `recover_orphaned_records()` (gap #30 below) and from the Coordinator loop on each iteration.

*Tests:* Create lock with short TTL (1ms). Sleep 2ms. Call expire_stale_locks — lock should be Expired.

---

**Gap #26: Researcher deduplication by target_id**

*Design:* MVP4 specifies rejecting spawn if non-terminal Researcher session with same target_id (scope_id in design doc terminology) exists. The `AgentSession` struct uses the field name `target_id`.

*Fix:* In `handle_agent_start`, after pool_size check, add:

```rust
if agent_type == AgentType::Researcher {
    if let Some(target_id) = req.params.get("target_id").and_then(|v| v.as_str()) {
        let has_existing = sessions.values().any(|s| {
            s.agent_type == AgentType::Researcher
                && !s.status.is_terminal()
                && s.target_id.as_deref() == Some(target_id)
        });
        if has_existing {
            return DaemonResponse::err(
                req.id,
                RpcError::precondition_failed(&format!(
                    "Non-terminal Researcher session already exists for target_id '{}'", target_id
                )),
            );
        }
    }
}
```

*Tests:* Start researcher with target_id="spec-1". Try to start another with same target_id — expect error. Stop first, retry — succeeds.

---

**Gap #33: `system.status` returns Tick SHA and stale work items**

*Design:* MVP1 specifies `system.status` returns current Tick SHA and stale work items.

*Fix:* In `handle_status`, add two blocks before building the response JSON. Each acquires and releases its own read lock (no cross-lock holding):

```rust
// Current Tick SHA — find the latest Published tick
let current_tick_sha: Option<String> = {
    let ticks = stores.ticks.read().unwrap();
    ticks.values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .and_then(|t| t.integration_sha.clone())
};

// Latest published tick ID for staleness check
let latest_tick_id: Option<String> = {
    let ticks = stores.ticks.read().unwrap();
    ticks.values()
        .filter(|t| t.status == TickStatus::Published)
        .max_by_key(|t| t.number)
        .map(|t| t.id.clone())
};

// Stale work items: InProgress work items that have bundles with
// base_tick_id mismatching the latest published tick
let stale_work_count: usize = {
    let wis = stores.works.read().unwrap();
    let bundles = stores.bundles.read().unwrap();
    if let Some(ref latest_tid) = latest_tick_id {
        wis.values()
            .filter(|wi| wi.status == WorkStatus::InProgress)
            .filter(|wi| {
                // Work item is stale if ANY of its non-terminal bundles have
                // a base_tick_id that doesn't match the latest published tick
                bundles.values().any(|b| {
                    b.work_id == wi.id
                        && !matches!(b.status, BundleStatus::Merged | BundleStatus::Rejected | BundleStatus::Superseded)
                        && b.base_tick_id.as_ref().is_some_and(|btid| btid != latest_tid)
                })
            })
            .count()
    } else {
        0 // No published ticks yet — nothing is stale
    }
};
```

Add these to the response JSON:

```rust
"current_tick_sha": current_tick_sha,
"stale_works": stale_work_count,
```

*Tests:*
1. Create and publish a Tick with SHA "abc123". Call system.status — assert `current_tick_sha == "abc123"`.
2. Create a work item InProgress with a bundle whose base_tick_id is an old tick. Call system.status — assert `stale_works == 1`.

---

### Phase 3: Update Handlers (Gap #1)

**Gap #1: `*.update` IPC handlers**

*Design:* MVP1 specifies `plan.update`, `spec.update`, `phase.update`, `work.update`, `bundle.update`, `tick.update`, `learning.update`.

*Fix:* Add 7 handlers and 7 dispatch entries following the pattern:

```rust
fn handle_plan_update(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    req: DaemonRequest,
) -> DaemonResponse {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("id is required")),
    };

    let mut plans = stores.plans.write().unwrap();
    let plan = match plans.get_mut(&id) {
        Some(p) => p,
        None => return DaemonResponse::err(req.id, RpcError::not_found("plans", &id)),
    };

    // Apply partial updates (only fields present in params)
    if let Some(title) = req.params.get("title").and_then(|v| v.as_str()) {
        plan.title = title.to_string();
    }
    if let Some(desc) = req.params.get("description").and_then(|v| v.as_str()) {
        plan.description = desc.to_string();
    }
    if let Some(criteria) = req.params.get("acceptance_criteria").and_then(|v| v.as_str()) {
        plan.acceptance_criteria = criteria.to_string();
    }
    plan.updated_at = crate::id::now_millis();

    // Persist to TaskStore
    if let Some(store) = &stores.store
        && let Err(e) = store.lock().unwrap().update(plan.clone())
    {
        return DaemonResponse::err(req.id, RpcError::internal(&e.to_string()));
    }

    let plan_json = serde_json::to_value(&*plan).unwrap();
    let _ = event_tx.send(DaemonEvent::record_updated("plans", &id));
    DaemonResponse::ok(req.id, plan_json)
}
```

Repeat for all 7 types with their respective updatable fields:

| Record | Updatable Fields |
|--------|-----------------|
| Plan | `title`, `description`, `acceptance_criteria` |
| Spec | `title`, `description` |
| Phase | `title`, `description`, `order` |
| Work | `title`, `description`, `assignee`, `resource_tags`, `acceptance_criteria`, `dependencies`, `checklist` |
| Bundle | `description`, `touched_paths`, `claims`, `verification`, `locks_used` |
| Tick | `validation_log`, `bundle_ids`, `attempted_bundle_ids` |
| Learning | `content`, `applicable_roles`, `resource_tags` |

Add dispatch entries:

```rust
"plan.update" => handle_plan_update(stores, event_tx, req),
"spec.update" => handle_spec_update(stores, event_tx, req),
"phase.update" => handle_phase_update(stores, event_tx, req),
"work.update" => handle_work_update(stores, event_tx, req),
"bundle.update" => handle_bundle_update(stores, event_tx, req),
"tick.update" => handle_tick_update(stores, event_tx, req),
"learning.update" => handle_learning_update(stores, event_tx, req),
```

*Tests:* For each type: create record, update one field, assert field changed and `updated_at` advanced. Update non-existent ID — expect error.

---

### Phase 4: CLI & TUI (Gaps #2, #6, #7, #13)

**Gap #2: TUI keybindings `n` and `t`**

*Design:* MVP1 specifies `n` (new record) and `t` (transition) keybindings.

*Fix:*

1. Add `Action` variants to `tui/input.rs`:

```rust
/// Create a new record (context-dependent on current view).
NewRecord,
/// Transition selected record's status.
TransitionRecord,
```

2. Add key mappings in `handle_key` Normal mode:

```rust
KeyCode::Char('n') => Action::NewRecord,
KeyCode::Char('t') => Action::TransitionRecord,
```

3. Add `IpcAction` variants to `tui/app.rs`:

```rust
NewRecord { collection: String },
TransitionRecord { collection: String, id: String },
```

4. In `apply_action`:
   - `NewRecord`: Map current view to collection name. Set `pending_ipc = Some(IpcAction::NewRecord { collection })`. For MVP, create with default/placeholder fields. Full modal input is a UX enhancement beyond the design spec.
   - `TransitionRecord`: Get selected record ID from current view's state. Set `pending_ipc` with the next logical transition for the current role.

5. In `dispatch_ipc_action` in `tui/run.rs`: handle the new `IpcAction` variants by calling `*.create` or `*.transition` via IPC.

*Tests:* Assert `handle_key(Char('n'), Normal) == Action::NewRecord`. Assert `handle_key(Char('t'), Normal) == Action::TransitionRecord`.

---

**Gap #6: `loopr role` CLI command**

*Design:* MVP1 specifies `loopr role [coordinator|integrator|implementer]` to persist role.

*Fix:*

1. Add subcommand to `cli/mod.rs`:

```rust
/// Set the active role (persisted to config)
Role {
    /// The role to set
    role: Role,
},
```

2. In `cli/dispatch.rs`, handle `Command::Role`:
   - Write role to `~/.config/loopr/role` (simple text file)
   - Print confirmation

3. When constructing IPC requests that need a role, read from this file if `--as` is not specified.

*Tests:* Parse `["loopr", "role", "coordinator"]` — assert matches `Command::Role { role: Role::Coordinator }`.

---

**Gap #7: `daemon.version` file**

*Design:* MVP1 specifies a version file at `~/.loopr/daemon.version`.

*Fix:* In `daemon/mod.rs`, in `daemon_main`, after `write_pid_file`:

```rust
fn write_version_file(config: &Config) -> eyre::Result<()> {
    // Derive runtime dir from pid_path's parent (same directory as daemon.pid)
    let runtime_dir = config.daemon.pid_path.parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let version_path = runtime_dir.join("daemon.version");
    std::fs::write(&version_path, env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
```

Call `write_version_file(&c.config)?` in `daemon_main` after `write_pid_file`.

*Tests:* Start daemon, verify version file exists and contains correct version string.

---

**Gap #13: `loopr agent start-integrator` CLI command**

*Design:* MVP4 implies Integrator can be started via agent.start. CLI has start commands for all other agent types.

*Fix:*

1. Add variant to `AgentCmd` in `cli/mod.rs`:

```rust
#[command(name = "start-integrator")]
StartIntegrator,
```

2. In `cli/dispatch.rs`, map to IPC:

```rust
AgentCmd::StartIntegrator => {
    ("agent.start".to_string(), json!({ "agent_type": "integrator" }))
}
```

*Tests:* Parse `["loopr", "agent", "start-integrator"]` — assert matches `AgentCmd::StartIntegrator`.

---

### Phase 5: Agent Lifecycle (Gaps #9, #10, #11, #29, #31)

**Gap #9: `agent.output` IPC method**

*Design:* MVP3 specifies `agent.output { session_id, since? }` with per-session ring buffer.

*Fix:*

1. Add `agent_events: RwLock<HashMap<String, VecDeque<AgentEvent>>>` to `Stores` in `daemon/context.rs`. Each session ID maps to a bounded ring buffer (capacity 1000).

2. In `run_agent_task` in `executor.rs`, every time an `AgentEvent` is sent via `event_tx.send()`, also push it into `stores.agent_events`:

```rust
fn record_agent_event(stores: &Stores, session_id: &str, event: &AgentEvent) {
    let mut events = stores.agent_events.write().unwrap();
    let ring = events.entry(session_id.to_string()).or_insert_with(|| VecDeque::with_capacity(1000));
    if ring.len() >= 1000 {
        ring.pop_front();
    }
    ring.push_back(event.clone());
}
```

3. Add handler `handle_agent_output` in `handlers.rs`:

```rust
fn handle_agent_output(stores: &Arc<Stores>, req: DaemonRequest) -> DaemonResponse {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => return DaemonResponse::err(req.id, RpcError::invalid_params("session_id is required")),
    };
    let since = req.params.get("since").and_then(|v| v.as_u64()).unwrap_or(0);

    let events = stores.agent_events.read().unwrap();
    let output: Vec<&AgentEvent> = match events.get(&session_id) {
        Some(ring) => ring.iter().skip(since as usize).collect(),
        None => Vec::new(),
    };
    DaemonResponse::ok(req.id, serde_json::to_value(&output).unwrap())
}
```

4. Add `"agent.output" => handle_agent_output(stores, req)` to dispatch table.

*Tests:* Start agent (mock), emit events, call agent.output — assert events returned.

---

**Gap #10: Tool timeout SIGTERM → wait 5s → SIGKILL escalation**

*Design:* MVP3 specifies graceful kill escalation.

*Fix:* In `tools/mod.rs`, replace the current timeout logic:

```rust
let timeout_dur = Duration::from_secs(entry.timeout_secs);
let start = Instant::now();

// Spawn the child so we control the signal sequence
let mut child = cmd.spawn().context(format!("failed to spawn tool: {}", tool_name))?;

let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
    Ok(result) => result.context(format!("failed to execute tool: {}", tool_name))?,
    Err(_) => {
        // Step 1: SIGTERM (graceful shutdown request)
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            // Use libc directly — avoids adding nix as a dependency
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
        }
        // Step 2: Wait 5s for graceful exit after SIGTERM
        match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_status)) => {
                // Process exited gracefully after SIGTERM — good
            }
            _ => {
                // Step 3: SIGKILL — process didn't exit in time
                let _ = child.kill().await;
            }
        }
        let duration = start.elapsed();
        return Ok(ToolResult {
            tool_name: tool_name.to_string(),
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("tool '{}' timed out after {}s (SIGTERM+SIGKILL)", tool_name, entry.timeout_secs),
            duration_ms: duration.as_millis() as u64,
            truncated: false,
        });
    }
};
```

*Note:* Uses `libc::kill` directly (libc is already a transitive dependency via tokio/nix). No new crate needed. The `#[cfg(unix)]` guard ensures this compiles on non-Unix (where it falls back to tokio's `child.kill()` which sends SIGKILL immediately).

*Tests:* Unit test verifying timeout ToolResult has exit_code=-1 and stderr mentions SIGTERM+SIGKILL.

---

**Gap #11: Agent session wall-clock timeouts**

*Design:* MVP4 specifies per-type timeouts: Implementer 30min, Reviewer 10min, Researcher 10min, Integrator 20min.

*Fix:*

1. Add `session_timeout_secs` to `AgentRoleConfig` in `config.rs`:

```rust
#[serde(default)]
pub session_timeout_secs: Option<u64>,
```

Defaults per type (set in `Default` impls):
- `AgentRoleConfig` for Implementer: `Some(1800)` (30 min)
- `AgentRoleConfig` for Reviewer: `Some(600)` (10 min)
- `AgentRoleConfig` for Researcher: `Some(600)` (10 min)
- `CoordinatorConfig`: None (infinite — Coordinator is long-lived)

2. Add `session_timeout_secs` to `IntegratorConfig` in `config.rs`:

```rust
#[serde(default = "default_integrator_timeout")]
pub session_timeout_secs: Option<u64>,
```

Default: `Some(1200)` (20 min).

3. In `run_agent_task` in `executor.rs`, wrap the agent loop call. Resolve the timeout per agent type from the correct config struct:

```rust
let timeout_secs: Option<u64> = match agent_type {
    AgentType::Coordinator => None, // infinite — long-lived
    AgentType::Implementer => stores.config.agents.implementer.session_timeout_secs,
    AgentType::Reviewer => stores.config.agents.reviewer.session_timeout_secs,
    AgentType::Researcher => stores.config.agents.researcher.session_timeout_secs,
    AgentType::Integrator => stores.config.integrator.session_timeout_secs,
};

let result = if let Some(secs) = timeout_secs {
    match tokio::time::timeout(Duration::from_secs(secs), run_agent_loop(...)).await {
        Ok(r) => r,
        Err(_) => {
            warn!("Agent {} timed out after {}s", session_id, secs);
            Err(eyre!("session wall-clock timeout after {}s", secs))
        }
    }
} else {
    run_agent_loop(...).await
};
```

*Tests:* Configure Researcher with session_timeout_secs=Some(1). Mock LLM that sleeps 5s. Assert session transitions to Failed with timeout message.

---

**Gap #29: Auto-start agents on Work/Bundle transitions**

*Design:* MVP3 specifies auto-start when `auto_start_implementer`/`auto_start_reviewer` flags are true.

*Fix:*

**Critical constraint:** Auto-start CANNOT be done inside transition handlers because (a) handlers hold write locks on stores that `agent.start` would also need to read, causing deadlock, and (b) transition handler signatures don't receive `worktree_mgr` or `integrator_config`.

Instead, implement auto-start as a **post-dispatch hook** in the top-level `dispatch()` function:

1. In `dispatch()` in `handlers.rs`, after the handler returns a successful response, check if auto-start should fire:

```rust
pub fn dispatch(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse {
    let method = req.method.clone();
    let params = req.params.clone();
    let resp = match method.as_str() {
        // ... existing dispatch table ...
    };

    // Post-dispatch auto-start hook (only on successful transitions)
    if !resp.is_error() {
        auto_start_agents(stores, event_tx, worktree_mgr, integrator_config, &method, &params);
    }

    resp
}
```

2. Add the `auto_start_agents` function:

```rust
fn auto_start_agents(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    method: &str,
    params: &serde_json::Value,
) {
    if method == "work.transition" {
        if let Some(target) = params.get("target_status").and_then(|v| v.as_str()) {
            if target == "InProgress" && stores.config.agents.auto_start_implementer {
                if let Some(wi_id) = params.get("id").and_then(|v| v.as_str()) {
                    let start_req = DaemonRequest::new(0, "agent.start", json!({
                        "agent_type": "implementer", "work_id": wi_id,
                    }));
                    let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
                }
            }
        }
    }
    if method == "bundle.transition" {
        if let Some(target) = params.get("target_status").and_then(|v| v.as_str()) {
            if target == "Triaged" && stores.config.agents.auto_start_reviewer {
                if let Some(bid) = params.get("id").and_then(|v| v.as_str()) {
                    let bundle_id = {
                        let bundles = stores.bundles.read().unwrap();
                        bundles.get(bid).map(|b| b.id.clone())
                    };
                    if let Some(bid) = bundle_id {
                        let start_req = DaemonRequest::new(0, "agent.start", json!({
                            "agent_type": "reviewer", "bundle_id": bid,
                        }));
                        let _ = dispatch(stores, event_tx, worktree_mgr, integrator_config, start_req);
                    }
                }
            }
        }
    }
}
```

This is safe because: (a) the transition handler has already returned and released all locks, (b) `dispatch()` receives `worktree_mgr` and `integrator_config` naturally, (c) the recursive `dispatch()` call acquires its own locks independently.

*Tests:* Set auto_start_implementer=true. Transition work item to InProgress via dispatch. Assert agent session created in stores.

---

**Gap #31: `auto_start_coordinator` checked at daemon startup**

*Design:* MVP4 specifies auto-starting coordinator on daemon boot.

*Fix:* In `daemon/mod.rs`, in `daemon_main`, after `ipc_server.bind()` succeeds and before entering `accept_loop()`. The Coordinator uses the in-process `AgentIpcBridge` (not the Unix socket), so the socket doesn't need to be accepting clients yet:

```rust
// After: let listener = ipc_server.bind().await?;
// Before: let result = accept_loop(listener, ctx.clone(), event_tx.clone()).await;

if ctx.read().await.config.agents.auto_start_coordinator {
    let c = ctx.read().await;
    let start_req = DaemonRequest::new(0, "agent.start", json!({ "agent_type": "coordinator" }));
    let _ = crate::daemon::handlers::dispatch(
        &c.stores, &c.event_tx, &c.worktree_manager, &c.config.integrator, start_req
    );
    info!("Auto-started Coordinator agent");
}
```

*Tests:* Configure auto_start_coordinator=true. Run daemon_main startup sequence. Assert a Coordinator session exists in stores.

---

### Phase 6: Strategy Knobs (Gaps #19, #20, #21, #23)

**Gap #19: `StalePolicy::ReplanAtSafePoint`**

*Design:* MVP4 specifies that under this policy, the Coordinator replans instead of hard-rejecting.

*Fix:* In `integrator_task.rs`, in the stale bundle handling section, read the policy:

```rust
let stale_policy = stores.config.strategy.stale_policy;
match stale_policy {
    StalePolicy::RejectIfStale => {
        // Current behavior: reject stale bundles (Integrator role)
        bridge.request("bundle.transition", json!({
            "id": bid, "target_status": "Rejected", "role": "integrator"
        }));
    }
    StalePolicy::ReplanAtSafePoint => {
        // Transition to Rejected (Integrator role — allowed by FSM) but emit
        // a replan event so the Coordinator knows to re-create the work item.
        // Note: Superseded requires Coordinator role, which the Integrator doesn't have.
        // The Integrator rejects; the Coordinator handles replanning.
        bridge.request("bundle.transition", json!({
            "id": bid, "target_status": "Rejected", "role": "integrator"
        }));
        let _ = event_tx.send(DaemonEvent::new(
            "bundle.stale_replan_needed",
            json!({"bundle_id": bid, "work_id": wi_id, "reason": "stale_base_tick"}),
        ));
        // Coordinator listens for this event and transitions Work back to Ready
        // for re-implementation against the latest tick
    }
    StalePolicy::AutoReplayAndVerify => {
        // Handled in Gap #20 below
    }
}
```

*FSM rationale:* The Bundle FSM allows `Accepted → Rejected` with `Role::Integrator` (rule #8). The `Superseded` transition requires `Role::Coordinator`. Since the Integrator task runs as `Role::Integrator`, it uses `Rejected` and emits a `bundle.stale_replan_needed` event for the Coordinator to act on.

*Tests:* Set stale_policy=ReplanAtSafePoint. Create stale bundle. Run integrator cycle. Assert bundle is Rejected AND `bundle.stale_replan_needed` event emitted.

---

**Gap #20: `StalePolicy::AutoReplayAndVerify`**

*Design:* MVP4 specifies auto-replaying stale bundles by refreshing the worktree and re-running validation.

*Fix:* In the same match arm in `integrator_task.rs`:

```rust
StalePolicy::AutoReplayAndVerify => {
    // Refresh worktree to latest tick
    let refresh_result = bridge.request("worktree.refresh", json!({"work_id": wi_id}));
    if refresh_result.is_error() {
        // Can't refresh — fall back to reject
        bridge.request("bundle.transition", json!({"id": bid, "target_status": "Rejected", ...}));
    } else {
        // Update bundle's base_tick_id to latest
        bridge.request("bundle.update", json!({"id": bid, "base_tick_id": latest_tick_id}));
        // Bundle stays Accepted, will be picked up in this cycle's normal flow
    }
}
```

*Tests:* Set stale_policy=AutoReplayAndVerify. Create stale bundle. Run integrator cycle. Assert bundle's base_tick_id updated to latest.

---

**Gap #21: `TickCadence::Batched`**

*Design:* MVP4 specifies batched mode waits for `min_bundles` accepted bundles or `timeout_secs` before creating a Tick.

*Fix:* In `run_integrator_cycle`, before creating a Tick, check cadence:

```rust
let accepted_bundles = /* collect accepted bundles */;
match &stores.config.strategy.tick_cadence {
    TickCadence::Continuous => {
        // Current behavior: process immediately if any accepted bundles
        if accepted_bundles.is_empty() { return Ok(IntegratorCycleResult::Idle); }
    }
    TickCadence::Batched { min_bundles, timeout_secs } => {
        if (accepted_bundles.len() as u32) < *min_bundles {
            // Check if timeout elapsed since earliest accepted bundle
            let earliest = accepted_bundles.iter().map(|b| b.updated_at).min().unwrap_or(0);
            let elapsed_secs = (crate::id::now_millis() - earliest) / 1000;
            if elapsed_secs < *timeout_secs as i64 {
                return Ok(IntegratorCycleResult::Idle); // Wait for more bundles or timeout
            }
            // Timeout elapsed — proceed with what we have
        }
    }
}
```

*Tests:* Set tick_cadence=Batched{min_bundles:3, timeout_secs:60}. Accept 1 bundle. Run cycle — should return Idle. Accept 2 more. Run cycle — should create Tick.

---

**Gap #23: `ValidatorStrictness` wiring**

*Design:* MVP4 specifies strictness affects the validation gate.

*Fix:*

1. In `check_validation_gate` in `handlers.rs`, read the policy:

```rust
let strictness = stores.config.strategy.validator_strictness;
match latest.verdict {
    ValidationVerdict::Fail => {
        match strictness {
            ValidatorStrictness::SuggestOnly => None, // Allow even on Fail
            _ => Some(RpcError::validation_required(collection, id)),
        }
    }
    ValidationVerdict::Warn => {
        match strictness {
            ValidatorStrictness::HardFailOnAnyAmbiguity => Some(RpcError::validation_required(collection, id)),
            _ => None, // AllowAmbiguityWithFlags and SuggestOnly allow Warn
        }
    }
    ValidationVerdict::Pass => None,
}
```

2. Pass strictness to Doc Validator prompts in `validator/prompts.rs` so the LLM knows the threshold.

*Tests:* Set strictness=SuggestOnly. Validate with Fail verdict. Transition Draft→Active — should succeed. Set strictness=HardFailOnAnyAmbiguity. Validate with Warn. Transition — should be blocked.

---

### Phase 7: Integrator Git Merge & Researcher Fix (Gaps #14, #32)

**Gap #14: Integrator git merge of Bundle branches**

*Design:* MVP4 specifies the Integrator merges Bundle branches into the integration branch.

*Fix:* Add `merge_bundle_branches` to `integrator_task.rs`:

```rust
fn merge_bundle_branches(
    worktree_mgr: &WorktreeManager,
    bundle_branches: &[String],
) -> Result<String> {
    let repo_root = &worktree_mgr.repo_path;

    for branch in bundle_branches {
        let output = std::process::Command::new("git")
            .args(["merge", "--no-ff", branch, "-m", &format!("Merge bundle branch {}", branch)])
            .current_dir(repo_root)
            .output()
            .context(format!("git merge {} failed", branch))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(eyre!("git merge {} failed: {}", branch, stderr));
        }
    }

    // Get HEAD SHA after merges
    let sha_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_root)
        .output()?;

    Ok(String::from_utf8_lossy(&sha_output.stdout).trim().to_string())
}
```

Call this in `run_integrator_cycle` after sealing the Tick and before running validation commands:

```rust
// Collect branch names from accepted bundles
let branches: Vec<String> = accepted_bundles.iter()
    .filter_map(|b| if !b.branch_name.is_empty() { Some(b.branch_name.clone()) } else { None })
    .collect();

if !branches.is_empty() {
    match merge_bundle_branches(worktree_mgr, &branches) {
        Ok(sha) => {
            tick.integration_sha = Some(sha);
        }
        Err(e) => {
            // Merge failed — fail the tick
            tick.status = TickStatus::Failed;
            tick.validation_log = format!("Merge failed: {}", e);
            // persist and return
        }
    }
}
```

*Tests:* Create temp git repo with two branches. Call merge_bundle_branches. Assert HEAD changed and contains commits from both branches.

---

**Gap #32: SearchCode uses `grep` instead of `rg`**

*Design:* MVP4 specifies `rg` (ripgrep) with `--no-follow`.

*Fix:* In `researcher.rs`, `execute_search_code`:

```rust
// Replace:
let mut cmd = tokio::process::Command::new("grep");
cmd.args(["-rn", "-E", pattern]);
if let Some(glob) = glob_filter {
    cmd.args(["--include", glob]);
}

// With:
let mut cmd = tokio::process::Command::new("rg");
cmd.args(["--no-follow", "-n", pattern]);
if let Some(glob) = glob_filter {
    cmd.args(["--glob", glob]);
}
```

Add fallback: if `rg` is not available, fall back to `grep`:

```rust
let rg_available = std::process::Command::new("rg").arg("--version").output().is_ok();
let mut cmd = if rg_available {
    let mut c = tokio::process::Command::new("rg");
    c.args(["--no-follow", "-n", pattern]);
    if let Some(glob) = glob_filter { c.args(["--glob", glob]); }
    c
} else {
    let mut c = tokio::process::Command::new("grep");
    c.args(["-rn", "-E", pattern]);
    if let Some(glob) = glob_filter { c.args(["--include", glob]); }
    c
};
```

*Tests:* Call execute_search_code with a known pattern. Assert results returned (works with either rg or grep).

---

### Phase 8: Executor Guards & Crash Recovery (Gaps #8, #15, #27, #28, #30)

**Gap #8: Audit trail for `--skip-validation`**

*Design:* MVP2 suggests recording a reason when skip-validation is used.

*Fix:* In `handle_*_transition` handlers, when `skip_validation` is true, emit an event:

```rust
if skip_validation {
    let reason = req.params.get("skip_reason").and_then(|v| v.as_str()).unwrap_or("no reason given");
    let _ = event_tx.send(DaemonEvent::new(
        "validation.skipped",
        json!({"collection": collection, "id": id, "reason": reason}),
    ));
}
```

Add `skip_reason` as an optional parameter to the CLI `--skip-validation` flag.

*Tests:* Transition with skip_validation=true and skip_reason="emergency". Assert validation.skipped event contains the reason.

---

**Gap #15: `confidence` in Learning indexed_fields()**

*Design:* MVP4 specifies confidence should be indexed.

*Fix:* In `domain/learning.rs`, in `indexed_fields()`:

```rust
fn indexed_fields(&self) -> HashMap<String, IndexValue> {
    let mut m = HashMap::new();
    m.insert("scope".into(), IndexValue::String(self.scope.to_string()));
    m.insert("source_id".into(), IndexValue::String(self.source_id.clone()));
    m.insert("promoted".into(), IndexValue::String(self.promoted.to_string()));
    m.insert("confidence".into(), IndexValue::String(format!("{:.2}", self.confidence))); // ADD THIS
    m
}
```

*Note:* TaskStore's `IndexValue` has three variants: `String`, `Int`, `Bool`. Since confidence is `f32`, we index it as a formatted string `"0.75"`. This matches the existing pattern where `promoted` is also indexed as a String.

*Tests:* Create learning. Assert indexed_fields contains "confidence" key.

---

**Gap #27: Draft-awareness guard in executor**

*Design:* MVP4 specifies programmatic enforcement that the Coordinator doesn't create duplicates at the same level.

*Fix:* In `execute_action` in `executor.rs`, for `CreatePlan`, `CreateSpec`, `CreatePhase`, `CreateWork`:

```rust
AgentAction::CreatePlan { title, description, acceptance_criteria } => {
    // Check for existing Draft plans
    let plans = stores.plans.read().unwrap();
    let has_draft = plans.values().any(|p| p.status == HierarchyStatus::Draft);
    if has_draft {
        return Ok(ActionResult::Error("A Draft Plan already exists. Iterate on the existing Draft instead of creating a new one.".into()));
    }
    // ... proceed with create
}
```

Similar guards for CreateSpec (check for Draft spec under same plan_id), CreatePhase (check for Draft phase under same spec_id), CreateWork (check for Draft work item under same phase_id).

*Tests:* Create a Draft plan. Call CreatePlan action again. Assert ActionResult::Error returned.

---

**Gap #28: One-level-per-iteration guard in executor**

*Design:* MVP4 specifies the Coordinator should only act at one hierarchy level per iteration.

*Fix:* In `executor.rs`, when processing a batch of actions from the Coordinator, track the hierarchy level of actions and reject if mixed:

```rust
fn infer_action_level(action: &AgentAction) -> Option<&'static str> {
    match action {
        AgentAction::CreatePlan { .. } => Some("plan"),
        AgentAction::CreateSpec { .. } => Some("spec"),
        AgentAction::CreatePhase { .. } => Some("phase"),
        AgentAction::CreateWork { .. } | AgentAction::AssignAgent { .. } => Some("work"),
        _ => None, // Non-hierarchy actions (transition, learning, lock, done, etc.) are level-agnostic
    }
}

// Before executing actions:
let levels: HashSet<_> = actions.iter().filter_map(infer_action_level).collect();
if levels.len() > 1 {
    warn!("Coordinator attempted multi-level actions: {:?}. Executing only first level.", levels);
    let first_level = infer_action_level(&actions[0]);
    actions.retain(|a| infer_action_level(a) == first_level || infer_action_level(a).is_none());
}
```

This is a soft guard — it filters rather than hard-rejects, so the Coordinator still makes progress.

*Tests:* Submit mixed actions [CreatePlan, CreateSpec]. Assert only CreatePlan actions executed.

---

**Gap #30: Lock auto-expiry in crash recovery**

*Design:* MVP4 specifies crash recovery sweeps expired locks.

*Fix:* In `recover_orphaned_records()` in `daemon/context.rs`, add a lock expiry block (reuses `expire_stale_locks` from Gap #25):

```rust
// Expired Locks → Expired status
{
    let mut locks = self.stores.locks.write().unwrap();
    for (id, lock) in locks.iter_mut() {
        if lock.is_active() && lock.is_expired() {
            warn!("Recovering expired Lock: {} (resource={})", id, lock.resource);
            lock.expire();
            if let Some(store) = &self.stores.store
                && let Err(e) = store.lock().unwrap().update(lock.clone())
            {
                warn!("Failed to persist expired lock: {}", e);
            }
            recovered += 1;
        }
    }
}
```

*Tests:* Create lock with expires_at in the past. Call recover_orphaned_records. Assert lock status is Expired.

---

## Implementation Plan

| Phase | Gaps | Scope | Dependencies |
|-------|------|-------|-------------|
| **P1** | #3, #4, #5, #12 | IPC events & protocol | None |
| **P2** | #16, #17, #18, #22, #24, #25, #26, #33 | Handler enforcement | P1 (events emitted on rejection) |
| **P3** | #1 | Update handlers (7 methods) | None |
| **P4** | #2, #6, #7, #13 | CLI & TUI | P3 (TUI may use update handlers) |
| **P5** | #9, #10, #11, #29, #31 | Agent lifecycle | P2 (enforcement guards) |
| **P6** | #19, #20, #21, #23 | Strategy knobs | P3 (bundle.update for #20) |
| **P7** | #14, #32 | Integrator merge & researcher fix | P6 (strategy knobs) |
| **P8** | #8, #15, #27, #28, #30 | Executor guards & crash recovery | P2 (lock expiry from #25) |

Each phase must pass `otto ci` before proceeding to the next.

## Alternatives Considered

### Alternative: Defer strategy knobs to MVP5

- **Description:** Implement only missing features and enforcement, leave strategy knobs unwired.
- **Pros:** Less code, faster delivery.
- **Cons:** Knobs were designed in MVP4. The user explicitly rejected deferral. Every gap must be addressed.
- **Why not chosen:** Explicit requirement to implement everything.

### Alternative: Soft guards only (no hard rejection)

- **Description:** Use warnings instead of errors for enforcement gaps.
- **Pros:** Less breakage risk for existing flows.
- **Cons:** Design docs specify hard guards (precondition_failed errors). Soft guards don't match spec.
- **Why not chosen:** Fidelity to design docs is the goal.

## Technical Considerations

### Dependencies

- No new crate dependencies. Gap #10 (SIGTERM escalation) uses `libc::kill` directly — `libc` is already a transitive dependency via tokio.

### Performance

- Handler enforcement adds guard clause checks (HashMap lookups) — negligible cost
- Agent event ring buffer adds ~1KB per event × 1000 = ~1MB per agent session
- Lock expiry sweep is O(n) over locks — called once on startup and per Coordinator iteration

### Testing Strategy

Each gap gets at least one test proving the new behavior. Tests follow existing patterns:
- Handler tests: create stores, dispatch request, assert response
- FSM tests: validate_transition with correct/wrong params
- Integration tests: multi-step flows through dispatch

Estimated new tests: ~80-100 across all 33 gaps.

### Rollout Plan

Phases are independent and ordered by dependency. Run `otto ci` after each phase. Each phase is a single commit with message format: `feat(completion): P{N} — {description}`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| BundleSizePolicy rejects existing valid bundles | Medium | Medium | Default max_files=8 is generous; only rejects obviously oversized bundles |
| Tool SIGTERM escalation breaks on non-Unix | Low | Low | `#[cfg(unix)]` guard, falls back to kill_on_drop |
| Integrator git merge fails on unrelated branches | Medium | Medium | Fail the tick and emit error event; Coordinator handles recovery |
| Strategy knob changes break existing configs | Low | High | All knobs have `#[serde(default)]`; existing configs get safe defaults |

## Open Questions

None. Every gap has a specified fix.

## References

- `docs/design/2026-02-25-orchestration-spine.md` — MVP1 design (orchestration spine)
- `docs/design/2026-02-26-taskstore-doc-validator.md` — MVP2 design (TaskStore + validator)
- `docs/design/2026-02-26-implementer-reviewer-agents.md` — MVP3 design (agents + tools)
- `docs/design/2026-02-26-multi-level-rwl.md` — MVP4 design (full agent roster)
- `docs/design/2026-02-27-audit-fixes.md` — Previous audit fixes
- `docs/design/2026-02-27-e2e-blockers.md` — Previous e2e blockers
