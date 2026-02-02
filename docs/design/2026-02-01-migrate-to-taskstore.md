# Design Document: Migrate from JsonlStorage to TaskStore

**Author:** Claude
**Date:** 2026-02-01
**Status:** In Review
**Review Passes Completed:** 5/5

## Summary

Migrate loopr's internal `JsonlStorage` implementation to use the external `taskstore` library as a git dependency. This consolidates persistence logic, gains SQLite-backed indexed queries, file locking, git integration, and staleness detection while eliminating ~300 lines of custom storage code.

## Problem Statement

### Background

Loopr's documentation extensively references "TaskStore" (17 docs files, 60+ mentions) as the persistence layer providing JSONL + SQLite storage with indexed queries, git hooks, and file locking. However, the actual implementation uses a simpler `JsonlStorage` struct that:

- Uses only JSONL files (no SQLite cache)
- Has no indexed queries (scans all records for every query)
- Has no file locking (potential corruption under concurrent writes)
- Has no git integration (no hooks, no merge driver)
- Has no staleness detection (no mtime-based sync)

Meanwhile, a mature `taskstore` library exists at `~/repos/scottidler/taskstore` that implements all these features.

### Problem

The gap between documented capabilities and actual implementation creates:

1. **Performance issues**: Every query scans all records in memory
2. **Concurrency bugs**: No file locking means potential data corruption
3. **Maintenance burden**: Two similar storage implementations to maintain
4. **Feature gap**: Documentation promises features that don't exist

### Goals

- Replace `JsonlStorage` with `taskstore::Store` as a git dependency
- Implement `taskstore::Record` trait for all domain types (`Loop`, `SignalRecord`, `EventRecord`, `ToolJobRecord`)
- Gain SQLite-backed indexed queries for efficient filtering
- Gain file locking for safe concurrent access
- Gain git hooks and merge driver for team collaboration
- Remove `src/storage/jsonl.rs` and `src/storage/traits.rs`
- Maintain backward compatibility with existing JSONL data files

### Non-Goals

- Changing the domain model or API signatures
- Adding new storage features beyond what TaskStore provides
- Migrating existing data (JSONL format is compatible)
- Modifying the TaskStore library itself

## Proposed Solution

### Overview

Replace loopr's `JsonlStorage` + `Storage` trait with `taskstore::Store` + `taskstore::Record` trait. The `Record` trait requires more methods than `HasId`, but provides indexed queries in return.

### Architecture

**Before:**
```
┌─────────────────────────────────────────────────────┐
│ loopr/src/storage/                                  │
│  ├── mod.rs       (re-exports)                      │
│  ├── jsonl.rs     (JsonlStorage - JSONL + RwLock)   │
│  ├── traits.rs    (Storage, HasId, Filter, FilterOp)│
│  └── loops.rs     (LoopStore wrapper)               │
└─────────────────────────────────────────────────────┘
```

**After:**
```
┌─────────────────────────────────────────────────────┐
│ taskstore (git dep)                                 │
│  ├── Store        (JSONL + SQLite + file locking)  │
│  ├── Record       (trait with indexed_fields)      │
│  ├── Filter       (indexed queries)                │
│  └── FilterOp     (Eq, Ne, Gt, Lt, Gte, Lte, etc)  │
└─────────────────────────────────────────────────────┘
         ↓ implements Record trait
┌─────────────────────────────────────────────────────┐
│ loopr/src/domain/                                   │
│  ├── loop_record.rs  (impl Record for Loop)        │
│  ├── signal.rs       (impl Record for SignalRecord)│
│  ├── event.rs        (impl Record for EventRecord) │
│  └── tool_job.rs     (impl Record for ToolJobRecord│
└─────────────────────────────────────────────────────┘
```

### Data Model

#### Trait Comparison

| Aspect | loopr `HasId` | taskstore `Record` |
|--------|---------------|-------------------|
| `id()` | ✅ `&str` | ✅ `&str` |
| `updated_at()` | ❌ | ✅ `i64` (ms epoch) |
| `collection_name()` | ❌ (passed separately) | ✅ `&'static str` |
| `indexed_fields()` | ❌ | ✅ `HashMap<String, IndexValue>` |
| Serde bounds | `Serialize + DeserializeOwned` | `Serialize + DeserializeOwned + Clone + Send + Sync + 'static` |

#### Domain Type Implementations

**Loop** (src/domain/loop_record.rs):
```rust
impl Record for Loop {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at  // Already exists
    }

    fn collection_name() -> &'static str {
        "loops"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert("status".to_string(), IndexValue::String(
            serde_json::to_string(&self.status).unwrap_or_default()
        ));
        fields.insert("loop_type".to_string(), IndexValue::String(
            serde_json::to_string(&self.loop_type).unwrap_or_default()
        ));
        if let Some(parent) = &self.parent_id {
            fields.insert("parent_id".to_string(), IndexValue::String(parent.clone()));
        }
        fields
    }
}
```

**SignalRecord** (src/domain/signal.rs):
```rust
impl Record for SignalRecord {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.created_at  // Signals are immutable after creation
    }

    fn collection_name() -> &'static str {
        "signals"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        if let Some(target) = &self.target_loop {
            fields.insert("target_loop".to_string(), IndexValue::String(target.clone()));
        }
        if let Some(selector) = &self.target_selector {
            fields.insert("target_selector".to_string(), IndexValue::String(selector.clone()));
        }
        // Index whether acknowledged for efficient "pending signals" query
        fields.insert("acknowledged".to_string(), IndexValue::Bool(self.acknowledged_at.is_some()));
        fields
    }
}
```

**EventRecord** (src/domain/event.rs):
```rust
impl Record for EventRecord {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.created_at  // Events are immutable
    }

    fn collection_name() -> &'static str {
        "events"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert("event_type".to_string(), IndexValue::String(self.event_type.clone()));
        if let Some(loop_id) = &self.loop_id {
            fields.insert("loop_id".to_string(), IndexValue::String(loop_id.clone()));
        }
        fields
    }
}
```

**ToolJobRecord** (src/domain/tool_job.rs):
```rust
impl Record for ToolJobRecord {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.completed_at.unwrap_or(self.created_at)
    }

    fn collection_name() -> &'static str {
        "tool_jobs"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert("loop_id".to_string(), IndexValue::String(self.loop_id.clone()));
        fields.insert("iteration".to_string(), IndexValue::Int(self.iteration as i64));
        fields.insert("tool_name".to_string(), IndexValue::String(self.tool_name.clone()));
        fields.insert("status".to_string(), IndexValue::String(
            serde_json::to_string(&self.status).unwrap_or_default()
        ));
        fields
    }
}
```

### API Design

#### Current API (JsonlStorage + Storage trait)

```rust
// Storage trait
pub trait Storage: Send + Sync {
    fn create<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, record: &T) -> Result<()>;
    fn get<T: DeserializeOwned>(&self, collection: &str, id: &str) -> Result<Option<T>>;
    fn update<T: Serialize + DeserializeOwned + HasId>(&self, collection: &str, id: &str, record: &T) -> Result<()>;
    fn delete(&self, collection: &str, id: &str) -> Result<()>;
    fn query<T: DeserializeOwned>(&self, collection: &str, filters: &[Filter]) -> Result<Vec<T>>;
    fn list<T: DeserializeOwned>(&self, collection: &str) -> Result<Vec<T>>;
}
```

#### New API (taskstore::Store)

```rust
// taskstore::Store methods
impl Store {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self>;
    pub fn create<T: Record>(&mut self, record: T) -> Result<String>;
    pub fn get<T: Record>(&self, id: &str) -> Result<Option<T>>;
    pub fn update<T: Record>(&mut self, record: T) -> Result<()>;
    pub fn delete<T: Record>(&mut self, id: &str) -> Result<()>;
    pub fn list<T: Record>(&self, filters: &[Filter]) -> Result<Vec<T>>;
    pub fn sync(&mut self) -> Result<()>;
    pub fn rebuild_indexes<T: Record>(&mut self) -> Result<usize>;
    pub fn install_git_hooks(&self) -> Result<()>;
}
```

#### Key Differences

| Aspect | JsonlStorage | taskstore::Store |
|--------|--------------|-----------------|
| Collection name | Passed to each method | Derived from `T::collection_name()` |
| Mutability | `&self` (interior mutability) | `&mut self` (explicit) |
| Return on create | `()` | `String` (the ID) |
| Filter value type | `serde_json::Value` | `IndexValue` enum |
| Initialization | `JsonlStorage::new(path)` | `Store::open(path)` |

### Implementation Plan

#### Phase 1: Add Dependency and Record Implementations

1. Add `taskstore` as path dependency in `Cargo.toml` (convert to git later):
   ```toml
   [dependencies]
   taskstore = { path = "../taskstore" }
   # Or via git once published:
   # taskstore = { git = "https://github.com/scottidler/taskstore" }
   ```

2. Implement `taskstore::Record` for all four domain types
   - `SignalRecord` and `ToolJobRecord` use `created_at` as `updated_at` (immutable records)
   - All domain types already derive `Clone`; verify `Send + Sync` bounds are satisfied

#### Phase 2: Create Compatibility Layer

Create a thin wrapper to minimize changes to callers. The wrapper provides:
- Interior mutability via `RwLock` (taskstore's `Store` requires `&mut self`)
- Error type conversion from `eyre::Result` to `crate::error::Result`
- Collection name abstraction (derived from `Record::collection_name()`)

```rust
// src/storage/mod.rs
use std::path::Path;
use std::sync::RwLock;
use taskstore::{Store, Record, Filter, FilterOp, IndexValue};
use crate::error::{LooprError, Result};

pub struct StorageWrapper {
    inner: RwLock<Store>,
}

impl StorageWrapper {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let store = Store::open(path).map_err(|e| LooprError::Storage(e.to_string()))?;
        Ok(Self {
            inner: RwLock::new(store),
        })
    }

    pub fn create<T: Record>(&self, record: &T) -> Result<()> {
        self.inner
            .write()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .create(record.clone())
            .map_err(|e| LooprError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn get<T: Record>(&self, id: &str) -> Result<Option<T>> {
        self.inner
            .read()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .get(id)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    pub fn update<T: Record>(&self, record: &T) -> Result<()> {
        self.inner
            .write()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .update(record.clone())
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    pub fn delete<T: Record>(&self, id: &str) -> Result<()> {
        self.inner
            .write()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .delete::<T>(id)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    pub fn list<T: Record>(&self, filters: &[Filter]) -> Result<Vec<T>> {
        self.inner
            .read()
            .map_err(|e| LooprError::Storage(e.to_string()))?
            .list(filters)
            .map_err(|e| LooprError::Storage(e.to_string()))
    }

    pub fn list_all<T: Record>(&self) -> Result<Vec<T>> {
        self.list(&[])
    }
}
```

**Why `RwLock`?** Loopr's storage is read-heavy (queries, checks) with occasional writes. `RwLock` allows concurrent reads while still providing exclusive write access.

#### Phase 3: Update Callers

Update all storage usage sites. Key changes per file:

1. **`src/daemon/context.rs`**:
   - Replace `Arc<JsonlStorage>` with `Arc<StorageWrapper>`
   - Update type alias `DaemonLoopManager`

2. **`src/manager/loop_manager.rs`**:
   - Remove `LOOPS_COLLECTION` constant (collection name from `Record` trait)
   - Change `storage.create(LOOPS_COLLECTION, &record)` → `storage.create(&record)`
   - Change `storage.get(LOOPS_COLLECTION, id)` → `storage.get::<Loop>(id)`
   - Change `storage.update(LOOPS_COLLECTION, id, &record)` → `storage.update(&record)`

3. **`src/coordination/signals.rs`**:
   - Remove `SIGNALS_COLLECTION` constant
   - Remove `impl HasId for SignalRecord` (replaced by `impl Record`)
   - Update filter construction to use `IndexValue`

4. **`src/coordination/invalidate.rs`**:
   - Update to use new filter API

5. **`src/daemon/recovery.rs`**:
   - Update storage method calls

#### Phase 4: Update Filter Usage

Migrate from `serde_json::Value` filters to `IndexValue`. This is the most significant code change.

**Before (loopr's Filter):**
```rust
// signals.rs - checking for unacknowledged signals
let filters = vec![
    Filter::eq("target_loop", loop_id),
    Filter::eq("acknowledged_at", serde_json::Value::Null),
];
let signals: Vec<SignalRecord> = self.storage.query(SIGNALS_COLLECTION, &filters)?;
```

**After (taskstore's Filter):**
```rust
// signals.rs - checking for unacknowledged signals
use taskstore::{Filter, FilterOp, IndexValue};

let filters = vec![
    Filter {
        field: "target_loop".to_string(),
        op: FilterOp::Eq,
        value: IndexValue::String(loop_id.to_string()),
    },
    Filter {
        field: "acknowledged".to_string(),  // Note: indexed as bool, not Option<i64>
        op: FilterOp::Eq,
        value: IndexValue::Bool(false),
    },
];
let signals: Vec<SignalRecord> = self.storage.list(&filters)?;
```

**Key insight:** The old code checked `acknowledged_at == null`. The new code uses a pre-computed boolean index `acknowledged` that's set in `indexed_fields()`. This is more efficient (boolean comparison vs null check) and clearer semantically.

#### Phase 5: Delete Old Code

1. Remove `src/storage/jsonl.rs`
2. Remove `src/storage/traits.rs` (or keep minimal re-exports)
3. Remove `HasId` implementations from domain types
4. Update `src/storage/mod.rs` to re-export from taskstore

#### Phase 6: Enable Git Integration

1. Call `store.install_git_hooks()` during daemon initialization
2. Document the git integration for users

### File Changes Summary

| File | Action |
|------|--------|
| `Cargo.toml` | Add taskstore git dependency |
| `src/storage/mod.rs` | Replace with StorageWrapper + re-exports |
| `src/storage/jsonl.rs` | **DELETE** |
| `src/storage/traits.rs` | **DELETE** or minimize |
| `src/storage/loops.rs` | Update or remove if redundant |
| `src/domain/loop_record.rs` | Add `impl Record for Loop` |
| `src/domain/signal.rs` | Add `impl Record for SignalRecord` |
| `src/domain/event.rs` | Add `impl Record for EventRecord` |
| `src/domain/tool_job.rs` | Add `impl Record for ToolJobRecord` |
| `src/daemon/context.rs` | Use StorageWrapper instead of JsonlStorage |
| `src/manager/loop_manager.rs` | Remove collection name args |
| `src/coordination/signals.rs` | Update filter usage, remove HasId impl |
| `src/coordination/invalidate.rs` | Update filter usage |
| `src/daemon/recovery.rs` | Update storage usage |

## Alternatives Considered

### Alternative 1: Enhance JsonlStorage In-Place

- **Description:** Add SQLite caching, file locking, and indexed queries to the existing `JsonlStorage` implementation.
- **Pros:** No external dependency; full control over implementation.
- **Cons:** Significant development effort (~500+ lines); duplicates existing tested code; maintenance burden.
- **Why not chosen:** TaskStore already implements this well; violates DRY principle.

### Alternative 2: Use SQLite Directly (Without JSONL)

- **Description:** Replace JSONL with pure SQLite storage.
- **Pros:** Simpler single-format storage; built-in transactions.
- **Cons:** Loses git-friendliness (binary SQLite files don't diff/merge well); breaks existing data; deviates from documented architecture.
- **Why not chosen:** JSONL's git-friendliness is a core design principle documented throughout the codebase.

### Alternative 3: Publish TaskStore to crates.io

- **Description:** Publish `taskstore` as a proper crate and depend on it via crates.io.
- **Pros:** Cleaner dependency management; versioned releases.
- **Cons:** Additional maintenance overhead; crates.io publishing process; premature for internal tool.
- **Why not chosen:** Git dependency is simpler for now; can publish later if needed.

## Technical Considerations

### Dependencies

**Internal:**
- All domain types in `src/domain/`
- All storage consumers (`LoopManager`, `SignalManager`, `DaemonContext`, recovery)

**External:**
- `taskstore` (git dependency): Provides `Store`, `Record`, `Filter`, `FilterOp`, `IndexValue`
- Transitive: `rusqlite`, `fs2` (file locking), `eyre`, `serde`, `serde_json`

### Performance

**Improvements:**
- SQLite indexes enable O(log n) queries vs O(n) full scans
- File locking prevents corruption under concurrent access
- Mtime-based staleness detection avoids unnecessary reloads

**Considerations:**
- Initial sync from JSONL to SQLite on first open after modification
- `rebuild_indexes<T>()` must be called after sync for each record type

### Security

- No new security implications
- TaskStore validates collection names and field names to prevent injection
- File permissions remain as configured by the user

### Testing Strategy

1. **Unit tests:** Verify `Record` implementations for all domain types
   - Test `id()`, `updated_at()`, `collection_name()`, `indexed_fields()` for each type
   - Test serialization roundtrip preserves all fields

2. **Integration tests:** Test `StorageWrapper` with real filesystem
   - CRUD operations for each record type
   - Query with various filter combinations
   - Verify SQLite indexes are created and used

3. **Migration tests:** Verify existing JSONL files load correctly after migration
   - Create JSONL files in old format, verify they load in new system
   - Test with tombstones, duplicates, and edge cases
   - Verify no data loss

4. **Concurrency tests:** Verify file locking works under parallel access
   - Multiple readers should succeed concurrently
   - Writer should block readers and other writers
   - No deadlocks under load

5. **Regression tests:** Ensure existing functionality works
   - Run full `otto ci` suite
   - Manual smoke test of TUI and daemon

### Rollout Plan

1. Implement in feature branch
2. Run full test suite with `otto ci`
3. Manual testing with existing loopr data directories
4. Merge to main
5. Document any user-visible changes (git hooks)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| JSONL format incompatibility | Low | High | TaskStore uses same JSONL format; add migration test |
| `&mut self` vs `&self` API friction | Medium | Medium | Use `RwLock` wrapper for interior mutability |
| Immutable records lack `updated_at` | Low | Low | Use `created_at` for `updated_at()` (semantically correct for immutable records) |
| Circular dependency with taskstore | Low | Medium | TaskStore is standalone; no loopr imports |
| SQLite bundling increases binary size | Low | Low | Already acceptable in taskstore; ~1-2MB |
| Index rebuild on startup | Medium | Low | Call `rebuild_indexes<T>()` after `Store::open()` for each record type |
| Concurrent daemon instances | Medium | High | TaskStore's file locking handles this; document single-daemon expectation |
| Tests using `Arc<S: Storage>` generic | High | Medium | Update test helpers to use `StorageWrapper` or mock |

### Edge Cases to Handle

1. **Empty JSONL files**: TaskStore handles gracefully (returns empty results)

2. **Corrupted JSONL lines**: TaskStore logs warning and skips (same as JsonlStorage)

3. **Missing `.taskstore` directory**: `Store::open()` creates it automatically

4. **Stale SQLite after git operations**: TaskStore's mtime-based staleness detection handles this; `sync()` rebuilds from JSONL

5. **Tombstone records**: TaskStore appends `{"id": "...", "deleted": true}` tombstones; handles correctly on sync

6. **Concurrent writes from multiple processes**: File locking via `fs2` prevents corruption; one writer blocks others

7. **Index desync after manual JSONL edit**: User must run `loopr sync` or restart daemon to rebuild indexes

## Open Questions

- [x] Should `StorageWrapper` use `RwLock` or `Mutex`? → **RwLock** (read-heavy workload)
- [ ] Should we call `install_git_hooks()` automatically or make it opt-in?
- [ ] Do we need to support the old `Filter::eq("field", value)` convenience methods?
- [ ] Should we add a `loopr sync` CLI command for manual index rebuilding?
- [ ] How should we handle the `Storage` trait - keep for testing/mocking or remove entirely?

## Migration Checklist

Pre-implementation:
- [ ] Review and approve this design document
- [ ] Ensure taskstore repo is accessible and tests pass

Phase 1 - Dependencies:
- [ ] Add `taskstore` dependency to `Cargo.toml`
- [ ] Verify `cargo build` succeeds with new dependency

Phase 2 - Record implementations:
- [ ] Implement `Record` for `Loop`
- [ ] Implement `Record` for `SignalRecord`
- [ ] Implement `Record` for `EventRecord`
- [ ] Implement `Record` for `ToolJobRecord`
- [ ] Add unit tests for each implementation

Phase 3 - StorageWrapper:
- [ ] Create `StorageWrapper` struct
- [ ] Implement all methods with error conversion
- [ ] Add integration tests

Phase 4 - Update callers:
- [ ] Update `src/daemon/context.rs`
- [ ] Update `src/manager/loop_manager.rs`
- [ ] Update `src/coordination/signals.rs`
- [ ] Update `src/coordination/invalidate.rs`
- [ ] Update `src/daemon/recovery.rs`
- [ ] Update all tests in affected files

Phase 5 - Cleanup:
- [ ] Delete `src/storage/jsonl.rs`
- [ ] Delete or minimize `src/storage/traits.rs`
- [ ] Remove `HasId` implementations
- [ ] Update `src/storage/mod.rs` exports

Phase 6 - Validation:
- [ ] Run `otto ci` - all tests pass
- [ ] Manual test with existing data directory
- [ ] Verify JSONL files are readable
- [ ] Test concurrent access

Phase 7 - Documentation:
- [ ] Update `docs/persistence.md` if needed
- [ ] Add migration notes if user action required

## References

- [taskstore source](~/repos/scottidler/taskstore)
- [loopr persistence.md](../persistence.md) - TaskStore design documentation
- [loopr architecture.md](../architecture.md) - System architecture
- [loopr loop-coordination.md](../loop-coordination.md) - TaskStore polling pattern
- [taskstore README](~/repos/scottidler/taskstore/README.md) - TaskStore usage guide
