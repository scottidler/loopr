# Design Document: Resource Loader Hardening

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Implemented
**Review Passes Completed:** 5/5
**Parent:** [Resource Embedding and Reorganization](2026-04-14-resource-embedding-and-reorganization.md)

## Summary

The resource embedding and reorganization work shipped in v0.1.133. The Architect's final review identified two structural risks (silent IO error swallowing, debug-only prefix guard) and one known limitation (triggers cannot be disabled). This design doc covers all three.

## Problem Statement

### Background

`Resources::load()` and `Resources::load_dir()` implement a three-tier resolution chain: repo-local override, XDG override, embedded default. The implementation is correct for the happy path but has three gaps identified during post-ship review.

### Problem 1: Silent IO error swallowing in override chain (structural risk)

When `read_to_string` fails on a repo-local or XDG override file that *exists* (e.g., permission denied, filesystem lock, I/O error), the loader silently falls through to the next tier. The current code in `src/resources.rs:51-59`:

```rust
if let Ok(content) = std::fs::read_to_string(&local)
    && !content.trim().is_empty()
{
    return Ok(content);
}
```

If `read_to_string` returns `Err`, the code skips the file with no log output. A user who places an override file at `~/.config/loopr/resources/agents/coordinator.pmt` but sets incorrect permissions will see the embedded default used instead, with no indication that their override was ignored.

The same pattern appears in `load_dir` at a directory level: `if let Ok(entries) = std::fs::read_dir(&dir)` at `src/resources.rs:104` silently skips unreadable override directories, and `entries.flatten()` silently drops IO errors on individual directory entries. A user with `{repo}/resources/engine/triggers/` that exists but has wrong permissions gets zero signal.

**Impact:** Debugging override behavior is difficult. A user troubleshooting "why isn't my custom prompt being used" gets no signal that the file was found but unreadable.

### Problem 2: Triggers cannot be disabled (known limitation)

`StrategyDefinition` in `src/engine/schema.rs:56-85` has an `enabled: bool` field (defaults to `true` via `default_enabled()`). The `collect_pending` method in `src/engine/tick.rs:151` filters by `s.enabled` before matching strategies to fired triggers. `TriggerDefinition` in `src/trigger/schema.rs:61-75` has no such field - once a trigger is loaded, it's always evaluated.

A user who wants to disable an embedded trigger must currently override it with a file that changes its behavior indirectly (e.g., setting impossible thresholds). There's no clean way to say "don't fire this trigger."

The original design doc explicitly deferred this: "if needed later, add `enabled: false` to trigger schema." The Architect flagged this as ready to resolve.

**Impact:** Users cannot cleanly disable individual embedded triggers without removing the file (which they can't do because it's embedded).

### Problem 3: load_dir prefix guard is debug-only (structural risk)

`load_dir` requires its prefix to end with `/`. This is enforced via `debug_assert!` at `src/resources.rs:92-96`, which is stripped in release builds. A caller passing a malformed prefix in release mode would produce incorrect path joins silently.

All current callsites are correct (hardcoded string literals), so this is low risk. But the guard exists precisely to catch future mistakes, and stripping it in release defeats that purpose.

**Impact:** Low - all current callsites are correct. But the invariant should hold in all builds.

### Goals

- Override chain failures are visible in logs at warn level
- Triggers can be disabled via `enabled: false` in YAML, matching strategy behavior
- `load_dir` prefix invariant holds in release builds

### Non-Goals

- Changing the override resolution order
- Adding the ability to disable embedded defaults via empty file (still deferred)
- Changing PromptStore compile-time access patterns (the Architect confirmed this is correct)
- Adding a `loopr init` scaffolding command
- Hot-reloading of override files (restart required, same as today)

## Proposed Solution

### Phase 1: Warn on override read failures

**Model:** sonnet

Add `tracing::warn!` when `read_to_string` fails for any reason other than the file not existing. The approach: inspect `io::ErrorKind` on the error directly, rather than making a second filesystem call via `exists()`. This avoids an extra syscall and the TOCTOU race that `exists()` introduces. It also correctly catches broken symlinks (which `read_to_string` fails on with a non-`NotFound` error on most platforms) and permission-denied errors.

**In `Resources::load()` (`src/resources.rs:41-74`, both repo-local and XDG blocks):**

```rust
// Repo-local override
if let Some(repo) = repo_path {
    let local = repo.join("resources").join(path);
    match std::fs::read_to_string(&local) {
        Ok(content) if !content.trim().is_empty() => {
            info!("resource override loaded: {}", local.display());
            return Ok(content);
        }
        Ok(_) => {
            // Empty file - fall through (intentional, documented behavior)
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!("resource override failed to read: {}: {}", local.display(), e);
        }
        Err(_) => {
            // File doesn't exist - normal fallthrough
        }
    }
}
```

Apply the same match pattern to the XDG override block (lines 62-69), substituting `config_dir.join("loopr/resources").join(path)` for `local`.

**In `Resources::load_dir()` (`src/resources.rs:91-144`, both repo-local and XDG directory scan blocks):**

```rust
if let Some(repo) = repo_path {
    let dir = repo.join("resources").join(prefix);
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                // ... existing file discovery logic unchanged ...
            }
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            warn!("resource override dir failed to read: {}: {}", dir.display(), e);
        }
        Err(_) => {
            // Directory doesn't exist - normal fallthrough
        }
    }
}
```

Apply the same pattern to the XDG directory scan block (lines 117-129).

Note: `entries.flatten()` still silently drops individual entry-level IO errors (e.g., one bad symlink inside a readable directory). This is intentional - one bad entry should not prevent loading the rest of the directory. The warn covers the directory-level failure (can't read the directory at all).

**AC:**
- warn! emitted when override file read fails with any ErrorKind other than NotFound (repo-local and XDG)
- warn! emitted when override directory read fails with any ErrorKind other than NotFound (repo-local and XDG)
- No behavior change for happy path or normal fallthrough
- Empty file still falls through silently (not an error)
- No extra filesystem syscalls on the error path (ErrorKind inspection only)

### Phase 2: Add `enabled` field to TriggerDefinition

**Model:** opus

Add `enabled: bool` to `TriggerDefinition` with `#[serde(default = "default_enabled")]` defaulting to `true`. Filter disabled triggers at evaluation time so they never fire. Add startup validation to catch dangerous combinations.

**Implementation:**

1. **Add field to TriggerDefinition** (`src/trigger/schema.rs:61-75`):
   ```rust
   pub struct TriggerDefinition {
       #[serde(skip_deserializing)]
       pub name: String,
       #[serde(default = "default_enabled")]
       pub enabled: bool,
       #[serde(default)]
       pub cooldown_secs: Option<u32>,
       #[serde(flatten)]
       pub kind: TriggerKind,
   }

   fn default_enabled() -> bool {
       true
   }
   ```

   **Field ordering is load-bearing.** `enabled` and `cooldown_secs` must appear *before* the `#[serde(flatten)]` field `kind`. When serde encounters `flatten`, it buffers all remaining map entries into an intermediate representation for the flattened type. Fields defined before the flatten are extracted first with their defaults intact. Fields defined after may have their `#[serde(default)]` bypassed due to known serde buffering behavior (`serde-rs/serde#1626`). Placing `enabled` before `kind` ensures the default is always applied when the YAML omits the field.

   Note: `default_enabled()` already exists in `src/engine/schema.rs:87` for StrategyDefinition. Duplicating the one-liner in trigger/schema.rs is fine - it's local to its module.

   A serde round-trip test (YAML with and without the `enabled` field) is the first implementation step to verify this works correctly.

2. **Filter in TriggerEvaluator** (`src/trigger/evaluate.rs`):
   - In `evaluate_pull()` (line 57-61): add `.filter(|t| t.enabled)` to the iterator that collects trigger names
   - In `evaluate_push()` (line 77-82): same filter

   ```rust
   let names: Vec<String> = self
       .triggers
       .iter()
       .filter(|t| t.enabled)  // new: skip disabled triggers
       .filter(|t| !matches!(t.kind, TriggerKind::Event { .. }))
       .map(|t| t.name.clone())
       .collect();
   ```

3. **Handle composite sub-trigger references** (`src/trigger/evaluate.rs:121-154`):
   `eval_composite()` (line 332) calls `evaluate_raw()` (line 121) on sub-triggers by name. `evaluate_raw` resolves the trigger by index and evaluates it directly - it does not check `enabled`. An early return must be added:

   ```rust
   fn evaluate_raw(&self, name: &str, ctx: &ObservationCtx<'_>) -> TriggerResult {
       let idx = match self.index.get(name).copied() {
           Some(i) => i,
           None => return TriggerResult::Idle,
       };
       let def = &self.triggers[idx];
       if !def.enabled {
           return TriggerResult::Idle;
       }
       // ... existing evaluation logic ...
   }
   ```

   Behavioral consequences of disabled sub-triggers:
   - **AND composite:** one leg permanently Idle, composite never fires - **correct**
   - **OR composite:** disabled arm ignored, other arms can still fire - **correct**
   - **NOT composite:** inner trigger returns Idle, NOT inverts to "fired for all scope_ids" - **dangerous, fires unconditionally**. Must be caught by startup validation.

4. **Startup cross-validation** - new function in `src/engine/schema.rs`, called from `src/daemon.rs` between trigger and strategy loading (after line 299, before engine construction at line 323):

   Currently, the trigger-strategy reference check only exists as a test (`src/engine/tests.rs:1207` - `all_strategy_triggers_exist_in_trigger_definitions`). This phase promotes it to runtime startup validation and extends it with `enabled` checks.

   ```rust
   // src/engine/schema.rs

   #[derive(Debug, Clone, PartialEq)]
   pub enum Severity {
       Error,
       Warn,
   }

   #[derive(Debug, Clone)]
   pub struct ValidationResult {
       pub severity: Severity,
       pub message: String,
   }

   pub fn validate_cross_references(
       strategies: &[StrategyDefinition],
       triggers: &[TriggerDefinition],
   ) -> Vec<ValidationResult> {
       let mut results = Vec::new();
       let trigger_map: HashMap<&str, &TriggerDefinition> =
           triggers.iter().map(|t| (t.name.as_str(), t)).collect();

       for strategy in strategies {
           match trigger_map.get(strategy.trigger.as_str()) {
               None => results.push(ValidationResult {
                   severity: Severity::Error,
                   message: format!(
                       "strategy '{}': trigger '{}' not found",
                       strategy.name, strategy.trigger
                   ),
               }),
               Some(t) if !t.enabled => results.push(ValidationResult {
                   severity: Severity::Warn,
                   message: format!(
                       "strategy '{}': trigger '{}' is disabled (strategy will never fire)",
                       strategy.name, strategy.trigger
                   ),
               }),
               _ => {}
           }
       }

       // Check composites referencing disabled triggers
       for trigger in triggers {
           if let TriggerKind::Composite { operator, triggers: sub_names } = &trigger.kind {
               for sub_name in sub_names {
                   if let Some(sub) = trigger_map.get(sub_name.as_str()) {
                       if !sub.enabled {
                           let severity = if matches!(operator, CompositeOperator::Not) {
                               Severity::Error // NOT + disabled = unconditional fire
                           } else {
                               Severity::Warn
                           };
                           results.push(ValidationResult {
                               severity,
                               message: format!(
                                   "composite '{}' ({:?}): sub-trigger '{}' is disabled",
                                   trigger.name, operator, sub_name
                               ),
                           });
                       }
                   }
               }
           }
       }

       results
   }
   ```

   In `src/daemon.rs`, call after strategy validation (line 312):
   ```rust
   let cross_results = crate::engine::schema::validate_cross_references(&strategies, &triggers);
   for result in &cross_results {
       match result.severity {
           crate::engine::schema::Severity::Error => error!("run_engine: {}", result.message),
           crate::engine::schema::Severity::Warn => warn!("run_engine: {}", result.message),
       }
   }
   let fatal_count = cross_results.iter().filter(|r| r.severity == crate::engine::schema::Severity::Error).count();
   if fatal_count > 0 {
       fatal!(stores, "run_engine: {} fatal cross-validation error(s)", fatal_count);
   }
   ```

**AC:**
- TriggerDefinition has `enabled: bool` field defaulting to true via serde default
- Disabled triggers are skipped in evaluate_pull() and evaluate_push()
- Disabled triggers return Idle from evaluate_raw() (composites treat them as never-fired)
- NOT composite referencing a disabled trigger is a fatal startup validation error
- AND/OR composite or strategy referencing a disabled trigger emits a startup warning
- Existing trigger YAML files parse identically (default true)
- Serde round-trip test verifies `flatten` + `default` interaction works correctly

### Phase 3: Upgrade load_dir prefix guard to runtime check

**Model:** sonnet

Replace `debug_assert!` with `eyre::ensure!` in `src/resources.rs:92-96`. This converts the invariant from a debug-only assertion to a runtime contract.

```rust
eyre::ensure!(
    prefix.ends_with('/'),
    "load_dir prefix must end with '/': got {:?}",
    prefix
);
```

**AC:**
- `debug_assert!` replaced with `eyre::ensure!`
- Malformed prefix returns Err in all builds, not just debug
- Existing callsites unaffected (all pass valid prefixes)
- Unit test confirming Err on missing trailing slash

## Alternatives Considered

### Alternative 1: Make override read failures fatal instead of warn

- **Description:** If a file exists at an override path but can't be read, return an error instead of warning and falling through
- **Pros:** No ambiguity - if a file is present, it must be readable
- **Cons:** Breaks the "override is optional" contract. A file with temporarily wrong permissions would crash the daemon instead of degrading gracefully
- **Why not chosen:** The override chain is designed to be resilient. Logging the issue at warn level gives operators visibility without breaking availability

### Alternative 2: Skip trigger `enabled` - use strategy `enabled` only

- **Description:** Since strategies already have `enabled`, and a trigger only fires through a strategy, disabling all strategies that reference a trigger effectively disables it
- **Pros:** No schema change. Already works for the single-strategy-per-trigger case
- **Cons:** Doesn't work when multiple strategies share a trigger. Forces users to understand the trigger-strategy binding to disable behavior. Doesn't prevent the trigger evaluation itself (wasted work)
- **Why not chosen:** Users think in terms of "I want to turn off the SLA trigger," not "I want to disable all strategies that reference the SLA trigger"

### Alternative 3: Filter disabled triggers in collect_pending instead of the evaluator

- **Description:** Keep evaluating all triggers, but filter out results from disabled triggers when matching strategies in `collect_pending()` (`src/engine/tick.rs:138`)
- **Pros:** Single filter point, no changes to TriggerEvaluator
- **Cons:** Disabled triggers still execute (wasted evaluation work). Composite triggers would still evaluate disabled sub-triggers via `evaluate_raw()`, producing wrong results for NOT composites (disabled inner trigger returns a result, NOT inverts it to fire-all)
- **Why not chosen:** Filtering at the source (evaluator) is both more efficient and more correct for composites

### Alternative 4: Keep debug_assert, document the invariant

- **Description:** All current callsites are correct, so the guard is only for future callers
- **Pros:** Zero code change
- **Cons:** Internal callers can still violate it in release. The assert exists precisely to catch mistakes
- **Why not chosen:** The upgrade from debug_assert to ensure is one line and has no downside

## Technical Considerations

### Dependencies

None. All changes use existing crates (tracing, eyre, serde).

### Performance

- Phase 1: Adds an `ErrorKind` check on the error path only. No impact on the happy path.
- Phase 2: Adds one boolean check per trigger per tick in the evaluator's name-collection iterator. Negligible.
- Phase 3: Adds one string suffix check at the start of `load_dir`. Negligible.

### Testing Strategy

- **Phase 1:** Unit test that `Resources::load()` warns when `read_to_string` fails with a non-NotFound error. Technique: pass a directory path as the "file" argument - `read_to_string` on a directory returns `Err` with `ErrorKind::IsADirectory` (or platform equivalent), which is not `NotFound`. Or use `tempfile` with mode 000 for a `PermissionDenied` variant.
- **Phase 2:**
  - Serde round-trip: TriggerDefinition YAML with and without `enabled` field. **Critical** - must verify `#[serde(flatten)]` on `kind` does not interfere with `#[serde(default)]` on `enabled`.
  - evaluate_pull/evaluate_push: construct a TriggerEvaluator with one enabled and one disabled trigger, verify only the enabled one fires.
  - evaluate_raw + composite: construct a composite with a disabled sub-trigger, verify AND=Idle, OR=partial, NOT=Idle (since validation prevents NOT+disabled from loading, but the code path should be safe regardless).
  - Cross-validation: verify `validate_cross_references` catches strategy->disabled-trigger and NOT-composite->disabled-trigger.
- **Phase 3:** Unit test that `load_dir("")` and `load_dir("no-slash")` return Err.

### Rollout Plan

All three phases are independently shippable. No breaking changes - all new behavior is additive (new log line, new optional YAML field with default true, stricter validation of an already-correct invariant).

Recommended order: Phase 3 first (trivial, one line), then Phase 1 (small, self-contained), then Phase 2 (largest scope, benefits from the other two being done).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| warn! log noise from NFS/FUSE edge cases | Low | Low | The warn is on the error path only; healthy systems never hit it |
| NOT-composite + disabled sub-trigger fires unconditionally | Medium | High | Startup validation rejects this combination as a fatal error |
| serde flatten + default interaction breaks enabled field | Low | Medium | `enabled` placed before `kind` (the flatten field) per serde-rs/serde#1626; round-trip test verifies |
| Users confused by "disabled trigger" warnings in validation | Low | Low | Validation messages are specific: "strategy X: trigger Y is disabled (strategy will never fire)" |
| Existing YAML with unknown `enabled` field fails to parse | None | None | serde(default) handles missing field; `enabled` is not currently used in trigger YAML |

## Open Questions

None. All three changes are straightforward follow-ups with clear scope.

## References

- [Resource Embedding and Reorganization](2026-04-14-resource-embedding-and-reorganization.md) - parent design doc
- Architect final review (v0.1.133 session) - identified these gaps
- `src/resources.rs:41-74` - Resources::load() override chain
- `src/resources.rs:91-144` - Resources::load_dir() additive override scan
- `src/trigger/schema.rs:61-75` - TriggerDefinition struct (no enabled field)
- `src/trigger/evaluate.rs:54,75` - evaluate_pull() and evaluate_push() (top-level filter point)
- `src/trigger/evaluate.rs:121` - evaluate_raw() (no-cooldown evaluator, called by composites)
- `src/trigger/evaluate.rs:332` - eval_composite() (sub-trigger evaluation via evaluate_raw)
- `src/engine/schema.rs:56-85` - StrategyDefinition struct (reference for `enabled` pattern)
- `src/engine/schema.rs:87` - default_enabled() function (already exists for strategies)
- `src/engine/schema.rs:185` - validate() structural validation (called at startup)
- `src/engine/tick.rs:151` - strategy enabled filtering in collect_pending()
- `src/engine/tests.rs:1207` - all_strategy_triggers_exist_in_trigger_definitions (test-only, being promoted to runtime)
- `src/daemon.rs:280-323` - trigger/strategy loading and engine construction (validation insertion point)
