# Design Document: Daemon Stability Sweep & Config Audit

**Author:** Scott A. Idler
**Date:** 2026-03-31
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Two surgical cleanup passes: (1) replace `.expect()` and `.unwrap()` calls in production daemon paths with graceful error handling to prevent lock-poisoning cascades, and (2) remove 8 unwired config knobs that create false impressions of feature completeness. Additionally, update the stale `remaining-gaps.md` to reflect Items #2, #3, and #4 which shipped but were never reconciled.

## Problem Statement

### Background

Loopr's daemon is designed for 24/7 uptime, managing long-running agent sessions across multiple concurrent Implementers, Reviewers, and a Coordinator. The daemon uses `std::sync::Mutex` and `RwLock` for shared state. Rust's standard library locks "poison" when a thread panics while holding them - every subsequent `.unwrap()` on that lock then panics too, creating a cascade that kills the entire daemon.

Separately, across 5 MVPs of rapid development, config knobs were added speculatively for planned features. Some of those features were implemented differently than originally planned, leaving orphaned config fields that add cognitive load and suggest capabilities that don't exist.

### Problem

1. **Lock poisoning risk**: Four `.expect("read_cache poisoned")` calls in `src/agents/executor.rs` (lines 626, 691, 709, 732) acquire a shared `Mutex<ReadCache>`. If any thread panics while holding this lock, all four sites panic on every subsequent call, crashing the daemon.

2. **Config inflation**: 8 config fields are defined but never read in production code. They occupy space in config files, documentation, and developer mental models without providing any value.

3. **Stale gap tracking**: `docs/design/remaining-gaps.md` lists all gaps as open, but every one of them shipped in Items #2, #3, and #4. The file is entirely stale.

### Goals

- **G1**: Replace all production `.expect()` and `.unwrap()` on lock acquisitions with graceful error handling that logs and recovers instead of panicking
- **G2**: Replace daemon startup `.expect()` calls with proper `Result` propagation
- **G3**: Remove 8 unwired config knobs and their Default implementations
- **G4**: Update `remaining-gaps.md` to reflect all gaps as shipped

### Non-Goals

- Removing `.unwrap()` from test code (allowed per project conventions)
- Removing `.expect()` on truly infallible operations (system clock, PID after spawn, guarded Option)
- Wiring currently-unwired features (coverage_strictness, plan_interview_enabled, etc.) - if they're not wired, they're removed
- Touching the interview funnel or chat-to-orchestration bridge (separate work)

## Proposed Solution

### Overview

Two phases: Phase 1 handles the production `.expect()`/`.unwrap()` hardening (10 call sites, 4 HIGH risk, 2 MEDIUM risk, 4 LOW risk). Phase 2 removes unwired config knobs and updates stale documentation.

### Phase 1: Daemon Stability Sweep

#### HIGH risk - Lock poisoning (4 sites in executor.rs)

All four sites acquire `ctx.read_cache.lock()` - a `Mutex<ReadCache>` shared across the agent's tool execution context.

| Line | Operation | Current Code |
|------|-----------|-------------|
| 626 | WriteFile cache invalidation | `.lock().expect("read_cache poisoned").invalidate(&path)` |
| 691 | EditFile cache invalidation | `.lock().expect("read_cache poisoned").invalidate(&path)` |
| 709 | ReadFile cache check | `.lock().expect("read_cache poisoned").check_hit(...)` |
| 732 | ReadFile cache record | `.lock().expect("read_cache poisoned").record(...)` |

**Fix**: Replace `.expect()` with match on the `PoisonError`, recovering by clearing the poisoned state. For a cache, the correct recovery is to accept the poisoned guard (the data is still valid) and continue:

```rust
// Before
ctx.read_cache.lock().expect("read_cache poisoned").invalidate(&full_path);

// After
match ctx.read_cache.lock() {
    Ok(mut cache) => cache.invalidate(&full_path),
    Err(poisoned) => {
        warn!("read_cache lock poisoned, recovering");
        poisoned.into_inner().invalidate(&full_path);
    }
}
```

For a read cache, poisoned state is always recoverable - the cache is an optimization, not a correctness requirement. Clearing and continuing is safe. We use `PoisonError::into_inner()` to recover the inner data.

To reduce repetition across 4 sites, add a helper method on `AgentContext`. Note: the field is `pub read_cache: Mutex<ReadCache>`, so the helper must use a different name to avoid shadowing:

```rust
impl AgentContext {
    /// Acquire the read_cache lock, recovering from poison by logging a warning.
    /// Named `cache()` to avoid shadowing the `read_cache` field.
    pub fn cache(&self) -> std::sync::MutexGuard<'_, ReadCache> {
        match self.read_cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                warn!("read_cache lock was poisoned, recovering");
                poisoned.into_inner()
            }
        }
    }
}
```

Then each call site becomes: `ctx.cache().invalidate(&full_path);`

#### MEDIUM risk - Daemon startup (2 sites in daemon/mod.rs)

| Line | Operation | Current Code |
|------|-----------|-------------|
| 135 | Logging setup | `.expect("daemon: failed to setup logging")` |
| 144 | Tokio runtime creation | `.expect("daemon: failed to create Tokio runtime")` |

**Fix**: These are in `daemon_fork_main()`, which is called after `fork()` in the grandchild process. The grandchild can't easily propagate errors back to the parent CLI process (it's a detached daemon). However, we can improve the failure mode:

```rust
// Before
let log_path = crate::setup_logging(...).expect("daemon: failed to setup logging");

// After
let log_path = match crate::setup_logging(...) {
    Ok(p) => p,
    Err(e) => {
        eprintln!("daemon: failed to setup logging: {e}");
        std::process::exit(1);
    }
};
```

This is a controlled exit rather than an uncontrolled panic. The daemon can't run without logging or a Tokio runtime, so exiting is correct - but `exit(1)` is cleaner than a panic backtrace.

#### LOW risk - Keep as-is with documentation (4 sites)

| File | Line | Pattern | Why safe |
|------|------|---------|----------|
| `id.rs` | 21 | `.expect("system clock before UNIX epoch")` | Physically impossible on modern systems |
| `prompts.rs` | 140 | `.expect("prompts::init() must be called...")` | Initialization contract, called once at startup |
| `tools/spawn.rs` | 55 | `.expect("child has PID")` | PID always available after successful spawn |
| `tui/run.rs` | 321 | `.expect("guarded by is_some")` | Guarded by `if client.is_some()` in same select arm |

These are all either infallible by construction or have clear documented invariants. No changes needed.

**Edge case verification**: All other `Mutex`/`RwLock` acquisitions in production code already use graceful patterns. For example, `daemon/mod.rs:385` uses `stores.lock_agent_handles()` which returns `Result`. The `Stores` RwLock maps (plans, specs, works, etc.) only use `.read().unwrap()` and `.write().unwrap()` in test code. The 4 `read_cache` sites are the only production lock acquisitions using `.expect()`.

### Phase 2: Config Audit & Cleanup

#### Revised after implementation investigation

The original design flagged 8 fields for removal. Post-investigation, 3 fields were found to have incomplete wiring (the feature existed but the config was never connected). The revised plan:

#### Wire up 2 config fields (missing connections)

| Field | Struct | Action |
|-------|--------|--------|
| `coverage_enabled` | StrategyConfig | Wire into coordinator decision tree at `identify_next_step()` - the evaluator has its own `enabled` flag for system availability, but this strategy-level toggle was never checked |
| `max_loc_changed` | BundleSizePolicy | Add `loc_changed` field to Bundle struct, enforce in bundle create/update handlers parallel to existing `max_files_touched` check (Gap #22) |

#### Keep 1 field (already wired - original analysis was wrong)

| Field | Struct | Why keep |
|-------|--------|----------|
| `validator_strictness` | StrategyConfig | Used in `daemon/handlers/common.rs:67` for validation gate decisions |

#### Remove 5 config fields (genuinely dead or superseded)

| Field | Struct | Why remove |
|-------|--------|-----------|
| `coverage_strictness` | StrategyConfig | Coverage decisions are binary (Complete/Incomplete); no gate point for strictness in current flow |
| `plan_interview_enabled` | StrategyConfig | Superseded by `InterviewMode` enum on CoordinatorConfig |
| `plan_approval_required` | StrategyConfig | Superseded by `InterviewMode` enum (approval flow controlled by mode variants) |
| `min_pool` | AgentRoleConfig | No pool scaling logic exists; system is reactive/on-demand; concept doesn't fit architecture |
| `debug` | Config | Fully overlapped by `log_level` field with 3-tier resolution (CLI > env > config) |

**Also removed:** `CoverageStrictness` enum (only consumer was the removed `coverage_strictness` field).

**Note - fields verified as WIRED (kept):**
- `delegate_model` (ChatConfig) - used via `to_delegate_role_config()` in `daemon/handlers/chat.rs:166`
- `provider` (ValidatorConfig) - used in `validator/client.rs:79` for API URL selection
- `provider` (EvaluatorConfig) - used in `daemon/context.rs:348` when constructing evaluator

**Approach**:
1. Serde ignores unknown fields by default (no `deny_unknown_fields` on any config struct), so existing config files with removed keys still parse
2. `otto ci` catches any compile errors from removed references
3. Added test verifying serde ignores removed field names gracefully

#### Update remaining-gaps.md

All gaps in the file have shipped. Mark each with what shipped it:

| Gap | Shipped In |
|-----|-----------|
| #16: Work `depends_on` cycle detection | Item #4: Pipeline Hardening (BFS cycle detection) |
| #10: Tool SIGTERM -> SIGKILL escalation | Item #2: Runner Lane Architecture (`killpg()` with SIGTERM->5s->SIGKILL) |
| #11: Agent session wall-clock timeouts | Item #4: Pipeline Hardening (`tokio::time::timeout` in executor.rs) |
| #22: Bundle `max_loc_changed` enforcement | Wired in this design doc (bundle create/update handlers + Bundle.loc_changed field) |
| Upward feedback / bubble-up logic | Item #3: Semantic Bubble-Up (`ReviseParent`, `bubble_up_count`) |
| Coverage gate in Coordinator loop | Item #3: Semantic Bubble-Up (wired into decision tree) |
| Auto-lock on WriteFile | Item #4: Pipeline Hardening (auto-acquisition in executor) |
| Lock cleanup on agent exit | Item #4: Pipeline Hardening (guaranteed release in `run_agent_task` cleanup) |
| Collaborative Plan interview IPC | Chat funnel handles this via interview_mode; IPC handlers exist |

**Action**: Replace the file's content with a note that all gaps are resolved.

### Implementation Plan

**Phase 1: Daemon Stability Sweep**
- Add `read_cache()` helper method to `AgentContext` that recovers from lock poisoning
- Replace 4 `.expect("read_cache poisoned")` calls with the helper
- Replace 2 daemon startup `.expect()` calls with controlled `exit(1)`
- Add tests: poisoned-lock recovery test for the helper, startup error propagation

**Phase 2: Config Audit & Documentation Cleanup** (revised after investigation)
- Wire up `coverage_enabled` in coordinator decision tree (`identify_next_step()`)
- Wire up `max_loc_changed` - add `loc_changed` field to Bundle, enforce in bundle create/update handlers
- Remove 5 genuinely dead/superseded fields: `coverage_strictness`, `plan_interview_enabled`, `plan_approval_required`, `min_pool`, `debug`
- Remove `CoverageStrictness` enum (no remaining consumers)
- Update `remaining-gaps.md` to reflect all gaps as resolved
- Already verified: no `serde(deny_unknown_fields)` on any config structs, so old configs parse fine

## Alternatives Considered

### Alternative 1: Clear poisoned locks by resetting to Default
- **Description:** On poison, replace the entire ReadCache with `ReadCache::default()`
- **Pros:** Simple, no data from the corrupted state persists
- **Cons:** Unnecessarily destructive - the data inside a poisoned lock is usually valid (the panic happened after the data was written). `into_inner()` preserves valid cache state.
- **Why not chosen:** `into_inner()` is the standard Rust approach for recoverable poisoned locks. Clearing the cache works but wastes cached file reads.

### Alternative 2: Replace Mutex with parking_lot::Mutex (no poisoning)
- **Description:** `parking_lot::Mutex` never poisons - it's always acquirable
- **Pros:** Eliminates poisoning entirely, simpler code
- **Cons:** New dependency, changes semantics across the whole codebase, masks genuine bugs (panic during lock hold might corrupt data)
- **Why not chosen:** Adding a crate dependency for 4 call sites is disproportionate. The explicit recovery with `into_inner()` documents the design intent and is localized.

### Alternative 3: Remove all unwired config knobs without investigation
- **Description:** Remove every config field not currently read in production code
- **Pros:** Minimal code, clean config surface
- **Cons:** Some fields had features behind them that were simply never connected. Removing them hides incomplete wiring rather than fixing it.
- **Why not chosen:** Investigation revealed 2 fields (`coverage_enabled`, `max_loc_changed`) had real features behind them with missing connections. These were wired up instead of removed. Only fields that were genuinely superseded or architecturally mismatched were removed.

## Technical Considerations

### Dependencies

- No new dependencies
- No changes to external crate versions

### Performance

- Lock recovery via `into_inner()` has zero overhead vs `.expect()` on the success path
- Removing unused config fields reduces deserialization time marginally

### Testing Strategy

- **Unit test**: Create a `Mutex`, poison it by panicking in a thread, recover via the helper, verify the inner data is accessible
- **Unit test**: Verify config deserialization still works with YAML that contains removed field names (serde should ignore them)
- **Unit test**: Verify config deserialization works without the removed fields
- **Compile-time**: `otto ci` catches any references to removed config fields

### Rollout Plan

- Both phases are backward-compatible - existing config files continue to parse
- No behavioral changes except replacing panics with warnings + recovery
- No IPC, protocol, or API changes

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Recovering from poisoned lock masks a real bug | Low | Medium | Log at WARN level so poisoning is always visible in daemon logs |
| Removing config field breaks user's config file | Low | Low | Serde ignores unknown fields by default; old configs parse fine |
| Removed config field was actually used via a code path we missed | Low | Medium | `otto ci` catches compile errors; grep audit was exhaustive |
| `into_inner()` returns corrupt data | Very Low | Low | ReadCache is an optimization; worst case is a cache miss, not incorrect behavior |

## Open Questions

- [ ] Should we also add `#[serde(deny_unknown_fields)]` to catch typos in config files? (Currently unknown fields are silently ignored, which is convenient for forward-compat but hides mistakes.)

## References

- `remaining-gaps.md` - stale gap tracking to be updated
- `src/agents/executor.rs:626-732` - HIGH risk lock sites
- `src/daemon/mod.rs:135-144` - MEDIUM risk startup sites
- `src/config.rs` - all config struct definitions
- Rust `PoisonError::into_inner()` docs: recovers inner data from a poisoned lock
- `docs/next-steps.md` - Items #2, #3, and #4 completion notes
- `src/daemon/mod.rs:385` - existing graceful lock pattern (`lock_agent_handles()`) as reference
